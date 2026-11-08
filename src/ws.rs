//! Minimal WebSocket server (tungstenite, plain ws://) on one port:
//!
//! - acceptor thread: TCP accept, then per connection (own thread) by request:
//!   - plain HTTP GET           -> the DG-LAB control page (web.rs)
//!   - ws `...?tid=<id>`        -> a DG-LAB APP joining our relay (dglab.rs)
//!   - ws `/control`            -> the control page's live connection (dglab.rs)
//!   - any other ws             -> a judgement stream client: gets a `hello`,
//!     then every event; anything it sends is ignored
//! - broadcaster thread: drains the event queue, hands each event to dglab.rs,
//!   JSON-encodes it once and sends it to all stream clients; a client whose
//!   write fails or times out is dropped.
use crate::{
    config::Config,
    dglab,
    events::{self, Event},
    web::{self, Head},
};
use serde_json::json;
use std::{
    io,
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{sync_channel, Receiver, SyncSender, TryRecvError},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tungstenite::{
    protocol::{frame::coding::CloseCode, CloseFrame},
    Message, WebSocket,
};

type Clients = Arc<Mutex<Vec<(String, WebSocket<TcpStream>)>>>;

static HOOKED: AtomicBool = AtomicBool::new(false);

pub fn set_hooked(v: bool) {
    HOOKED.store(v, Ordering::Release);
}

pub fn hooked() -> bool {
    HOOKED.load(Ordering::Acquire)
}

fn hello() -> String {
    json!({
        "type": "hello",
        "name": "judgehook",
        "version": env!("CARGO_PKG_VERSION"),
        "hooked": hooked(),
        "dropped": events::DROPPED.load(Ordering::Relaxed),
        "grades": ["CriticalPerfect", "Perfect", "Great", "Good", "Miss"],
        "song": events::current_song(),
    })
    .to_string()
}

pub fn start(cfg: &Config) -> SyncSender<Event> {
    let (tx, rx) = sync_channel::<Event>(cfg.queue);
    let clients: Clients = Arc::default();

    let addr = format!("{}:{}", cfg.bind, cfg.port);
    let web = cfg.web;
    if web {
        dglab::init(cfg.port);
    }
    let acc_clients = clients.clone();
    let _ = thread::Builder::new().name("judgehook-accept".into()).spawn(move || {
        // the port may be briefly unavailable (e.g. previous process still dying)
        let listener = loop {
            match TcpListener::bind(&addr) {
                Ok(l) => break l,
                Err(e) => {
                    log::warn!("bind {addr} failed: {e}, retrying in 2s");
                    thread::sleep(Duration::from_secs(2));
                }
            }
        };
        log::info!("WebSocket server listening on ws://{addr}");
        if web {
            log::info!("DG-LAB control page on http://{addr}/");
        }
        for stream in listener.incoming().flatten() {
            let clients = acc_clients.clone();
            // handshake on its own thread so a slow client can't stall accept()
            let _ = thread::Builder::new()
                .name("judgehook-conn".into())
                .spawn(move || handle_conn(stream, clients, web));
        }
    });

    let _ = thread::Builder::new()
        .name("judgehook-send".into())
        .spawn(move || broadcast(rx, clients, web));
    tx
}

enum Route {
    Stream,
    App(String),
    Control,
}

/// One accepted connection (see the module docs). Without `web` everything is
/// a judgement stream client, as before the page existed.
fn handle_conn(stream: TcpStream, clients: Clients, web: bool) {
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let mut route = Route::Stream;
    if web {
        match web::peek_head(&stream) {
            Ok(Head::Http { method, path, len }) => {
                match web::serve(stream, &method, &path, len) {
                    Ok(status) => log::info!("http {peer}: {method} {path} -> {status}"),
                    Err(e) => log::warn!("http {peer}: {method} {path} failed: {e}"),
                }
                return;
            }
            Ok(Head::WebSocket(Some(path))) => {
                if let Some(tid) = dglab::tid_of(&path) {
                    route = Route::App(tid.to_owned());
                } else if path == "/control" || path.starts_with("/control?") {
                    route = Route::Control;
                }
            }
            Ok(_) => {}
            Err(e) => {
                log::warn!("connection from {peer} failed: {e}");
                return;
            }
        }
    }
    let mut ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(e) => {
            log::warn!("handshake with {peer} failed: {e}");
            return;
        }
    };
    match route {
        Route::Stream => {
            if ws.send(Message::Text(hello())).is_ok() {
                log::info!("client connected: {peer}");
                clients.lock().unwrap().push((peer, ws));
            }
        }
        Route::App(tid) => match dglab::app_connect(&tid, &peer) {
            Ok(conn) => {
                if let Err(e) = run_peer(&mut ws, &conn.rx, |t| dglab::app_text(conn.id, t), true) {
                    log::info!("APP {peer}: {e}");
                }
                dglab::app_disconnect(conn.id);
            }
            Err(frame) => {
                log::warn!("APP {peer} used an unknown tid");
                let _ = ws.send(Message::Text(frame));
                let close = CloseFrame { code: CloseCode::from(4001), reason: "controller_not_found".into() };
                let _ = ws.close(Some(close));
                let _ = ws.flush();
            }
        },
        Route::Control => {
            let conn = dglab::control_connect();
            if let Err(e) = run_peer(&mut ws, &conn.rx, dglab::control_text, false) {
                log::info!("control page {peer}: {e}");
            }
            dglab::control_disconnect(conn.id);
        }
    }
}

/// Two-way connection on one thread: reads with a short timeout and sends
/// whatever was queued in `rx` in between (latency <= POLL). Returns when the
/// peer closes, the queue's sender is gone, or a read/write fails.
#[allow(clippy::result_large_err)]
fn run_peer(
    ws: &mut WebSocket<TcpStream>,
    rx: &Receiver<String>,
    mut on_text: impl FnMut(&str),
    heartbeat: bool,
) -> tungstenite::Result<()> {
    const POLL: Duration = Duration::from_millis(10);
    const HEARTBEAT: Duration = Duration::from_secs(30);
    ws.get_mut().set_read_timeout(Some(POLL))?;
    ws.get_mut().set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut beat = Instant::now();
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => on_text(&t),
            Ok(Message::Close(_)) => {
                let _ = ws.flush();
                return Ok(());
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(tungstenite::Error::ConnectionClosed) => return Ok(()),
            Err(e) => return Err(e),
        }
        loop {
            match rx.try_recv() {
                Ok(text) => ws.send(Message::Text(text))?,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    let _ = ws.close(None);
                    let _ = ws.flush();
                    return Ok(());
                }
            }
        }
        if heartbeat && beat.elapsed() >= HEARTBEAT {
            beat = Instant::now();
            ws.send(Message::Text(json!({ "type": "heartbeat" }).to_string()))?;
        }
    }
}

fn broadcast(rx: Receiver<Event>, clients: Clients, web: bool) {
    for ev in rx {
        if web {
            dglab::on_event(&ev);
        }
        let text = match serde_json::to_string(&ev) {
            Ok(t) => t,
            Err(e) => {
                log::error!("serialize: {e}");
                continue;
            }
        };
        let mut list = clients.lock().unwrap();
        list.retain_mut(|(peer, ws)| match ws.send(Message::Text(text.clone())) {
            Ok(()) => true,
            Err(e) => {
                log::info!("client {peer} dropped: {e}");
                false
            }
        });
    }
}
