//! The DG-LAB controller page, served on the WebSocket port: a request without
//! `Upgrade: websocket` gets plain HTTP, everything else goes to tungstenite.
//!
//! The page (web/index.html) talks to dglab.rs over ws://<host>/control.
use std::{
    io::{self, Read, Write},
    net::TcpStream,
    thread,
    time::{Duration, Instant},
};

const INDEX: &str = include_str!("../web/index.html");
const QRCODE: &str = include_str!("../web/vendor/qrcode.js");

const MAX_HEAD: usize = 8192;

#[derive(Debug, PartialEq, Eq)]
pub enum Head {
    /// a WebSocket upgrade (with its request path), or anything we don't
    /// understand (no path): left to tungstenite
    WebSocket(Option<String>),
    /// a plain HTTP request; `len` = bytes of the request head
    Http { method: String, path: String, len: usize },
    /// the head is not complete yet
    Partial,
}

/// Classifies the (possibly incomplete) request head at the start of `buf`.
pub fn classify(buf: &[u8]) -> Head {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(buf) {
        Ok(httparse::Status::Partial) => Head::Partial,
        Ok(httparse::Status::Complete(len)) => {
            let upgrade = req.headers.iter().any(|h| {
                h.name.eq_ignore_ascii_case("upgrade")
                    && std::str::from_utf8(h.value).is_ok_and(|v| v.trim().eq_ignore_ascii_case("websocket"))
            });
            match (upgrade, req.method, req.path) {
                (false, Some(m), Some(p)) => Head::Http { method: m.to_owned(), path: p.to_owned(), len },
                (_, _, p) => Head::WebSocket(p.map(str::to_owned)),
            }
        }
        Err(_) => Head::WebSocket(None),
    }
}

/// Waits (up to 5 s) for the request head without consuming it, so a
/// WebSocket handshake can still be read by tungstenite.
pub fn peek_head(stream: &TcpStream) -> io::Result<Head> {
    let mut buf = vec![0u8; MAX_HEAD];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // blocks until at least one byte is there (bounded by the read timeout)
        let n = stream.peek(&mut buf)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        match classify(&buf[..n]) {
            Head::Partial if n < buf.len() && Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            // too long or too slow: let tungstenite deal with (and reject) it
            Head::Partial => return Ok(Head::WebSocket(None)),
            head => return Ok(head),
        }
    }
}

pub struct Response {
    pub status: &'static str,
    pub content_type: &'static str,
    pub body: &'static str,
}

pub fn route(method: &str, path: &str) -> Response {
    let path = path.split(['?', '#']).next().unwrap_or("");
    if method != "GET" && method != "HEAD" {
        return Response { status: "405 Method Not Allowed", content_type: "text/plain; charset=utf-8", body: "method not allowed\n" };
    }
    let (content_type, body) = match path {
        "/" | "/index.html" | "/dglab" => ("text/html; charset=utf-8", INDEX),
        "/vendor/qrcode.js" => ("text/javascript; charset=utf-8", QRCODE),
        _ => return Response { status: "404 Not Found", content_type: "text/plain; charset=utf-8", body: "not found\n" },
    };
    Response { status: "200 OK", content_type, body }
}

/// Answers one plain HTTP request (whose head is `head_len` bytes) and closes.
pub fn serve(mut stream: TcpStream, method: &str, path: &str, head_len: usize) -> io::Result<&'static str> {
    let mut head = vec![0u8; head_len];
    stream.read_exact(&mut head)?;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let r = route(method, path);
    let header = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        r.status,
        r.content_type,
        r.body.len()
    );
    stream.write_all(header.as_bytes())?;
    if method != "HEAD" {
        stream.write_all(r.body.as_bytes())?;
    }
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(r.status)
}

