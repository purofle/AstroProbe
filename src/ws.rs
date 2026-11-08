//! Minimal broadcast WebSocket server (tungstenite, plain ws://).
//!
//! - acceptor thread: TCP accept + WS handshake, sends a `hello`, adds the client
//! - broadcaster thread: drains the event queue, JSON-encodes once, sends to all
//!   clients; a client whose write fails or times out is dropped.
//! Clients are send-only from our side; anything they send is ignored.
use crate::{config::Config, events::{self, Event}};
use serde_json::json;
use std::{
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{sync_channel, Receiver, SyncSender},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
use tungstenite::{Message, WebSocket};

type Clients = Arc<Mutex<Vec<(String, WebSocket<TcpStream>)>>>;

static HOOKED: AtomicBool = AtomicBool::new(false);

pub fn set_hooked(v: bool) {
    HOOKED.store(v, Ordering::Release);
}

fn hello() -> String {
    json!({
        "type": "hello",
        "name": "judgehook",
        "version": env!("CARGO_PKG_VERSION"),
        "hooked": HOOKED.load(Ordering::Acquire),
        "dropped": events::DROPPED.load(Ordering::Relaxed),
        "grades": ["CriticalPerfect", "Perfect", "Great", "Good", "Miss"],
    })
    .to_string()
}

pub fn start(cfg: &Config) -> SyncSender<Event> {
    let (tx, rx) = sync_channel::<Event>(cfg.queue);
    let clients: Clients = Arc::default();

    let addr = format!("{}:{}", cfg.bind, cfg.port);
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
        for stream in listener.incoming().flatten() {
            let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
            let clients = acc_clients.clone();
            // handshake on its own thread so a slow client can't stall accept()
            let _ = thread::Builder::new().name("judgehook-hs".into()).spawn(move || {
                let _ = stream.set_nodelay(true);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
                match tungstenite::accept(stream) {
                    Ok(mut ws) => {
                        if ws.send(Message::Text(hello().into())).is_ok() {
                            log::info!("client connected: {peer}");
                            clients.lock().unwrap().push((peer, ws));
                        }
                    }
                    Err(e) => log::warn!("handshake with {peer} failed: {e}"),
                }
            });
        }
    });

    let _ = thread::Builder::new().name("judgehook-send".into()).spawn(move || broadcast(rx, clients));
    tx
}

fn broadcast(rx: Receiver<Event>, clients: Clients) {
    for ev in rx {
        let text = match serde_json::to_string(&ev) {
            Ok(t) => t,
            Err(e) => {
                log::error!("serialize: {e}");
                continue;
            }
        };
        let mut list = clients.lock().unwrap();
        list.retain_mut(|(peer, ws)| match ws.send(Message::Text(text.clone().into())) {
            Ok(()) => true,
            Err(e) => {
                log::info!("client {peer} dropped: {e}");
                false
            }
        });
    }
}
