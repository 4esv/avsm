//! Stubs the integration tests stand in for the world with: an HTTP server
//! that answers by route and remembers what it saw, and a one-connection
//! IMAP server.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Do these ledger lines show a run of `lever`: its intent, then the
/// outcome of that intent, done, with `detail` in it?
pub fn ran(lines: &str, lever: &str, detail: &str) -> bool {
    lines
        .lines()
        .filter(|l| l.contains(&format!("intent    {lever}: starting")))
        .filter_map(|l| {
            l.trim_start()
                .trim_start_matches('#')
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        })
        .any(|seq| {
            let done = format!("outcome   #{seq} done");
            lines
                .lines()
                .any(|l| l.contains(&done) && l.contains(detail))
        })
}

/// One answer: for requests with this method whose path starts with `path`.
/// The longest matching path wins.
pub struct Route {
    pub method: String,
    pub path: String,
    pub status: u16,
    pub body: String,
}

pub fn route(method: &str, path: &str, status: u16, body: &str) -> Route {
    Route {
        method: method.to_string(),
        path: path.to_string(),
        status,
        body: body.to_string(),
    }
}

/// A request the stub saw.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// The header lines, lowercased.
    pub head: String,
    pub body: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|l| {
            l.strip_prefix(&format!("{}:", name.to_lowercase()))
                .map(|v| v.trim().to_string())
        })
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

pub struct Stub {
    pub url: String,
    seen: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    addr: std::net::SocketAddr,
    handle: Option<JoinHandle<()>>,
}

impl Stub {
    pub fn start(routes: Vec<Route>) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let seen = Arc::clone(&seen);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(mut stream) = stream else { break };
                    let Some(req) = read_request(&mut stream) else {
                        continue;
                    };
                    let best = routes
                        .iter()
                        .filter(|r| r.method == req.method && req.path.starts_with(&r.path))
                        .max_by_key(|r| r.path.len());
                    let (status, body) = match best {
                        Some(r) => (r.status, r.body.clone()),
                        None => (
                            404,
                            format!(
                                "{{\"message\":\"no route for {} {}\"}}",
                                req.method, req.path
                            ),
                        ),
                    };
                    let response = format!(
                        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        reason(status),
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                    seen.lock().unwrap().push(req);
                }
            })
        };
        Stub {
            url: format!("http://{addr}"),
            seen,
            stop,
            addr,
            handle: Some(handle),
        }
    }

    /// Every request so far, oldest first.
    pub fn seen(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }

    /// The requests to paths starting with `prefix`.
    pub fn seen_at(&self, prefix: &str) -> Vec<Request> {
        self.seen()
            .into_iter()
            .filter(|r| r.path.starts_with(prefix))
            .collect()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Status",
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut header_end = None;
    let mut content_length = 0usize;
    loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if header_end.is_none() {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                header_end = Some(pos + 4);
                let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                for line in head.lines() {
                    if let Some(v) = line.strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
            }
        }
        if let Some(h) = header_end {
            if buf.len() >= h + content_length {
                break;
            }
        }
    }
    let h = header_end?;
    let head = String::from_utf8_lossy(&buf[..h]).into_owned();
    let mut lines = head.lines();
    let first = lines.next()?;
    let mut parts = first.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    Some(Request {
        method,
        path,
        head: lines.map(|l| l.to_lowercase() + "\n").collect(),
        body: String::from_utf8_lossy(&buf[h..h + content_length.min(buf.len() - h)]).into_owned(),
    })
}

/// A one-connection IMAP server with `unseen` unseen messages. Returns its
/// host:port and a handle that yields every line the client sent.
pub fn imap_stub(unseen: u64) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut stream = stream;
        let mut seen = Vec::new();
        stream.write_all(b"* OK stub ready\r\n").unwrap();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            seen.push(line.trim_end().to_string());
            let mut words = line.split_whitespace();
            let tag = words.next().unwrap_or("x").to_string();
            let command = words.next().unwrap_or("").to_uppercase();
            let reply = match command.as_str() {
                "LOGIN" => format!("{tag} OK logged in\r\n"),
                "STATUS" => {
                    format!("* STATUS INBOX (MESSAGES 10 UNSEEN {unseen})\r\n{tag} OK status\r\n")
                }
                "LOGOUT" => format!("* BYE\r\n{tag} OK bye\r\n"),
                _ => format!("{tag} BAD unknown\r\n"),
            };
            stream.write_all(reply.as_bytes()).unwrap();
            if command == "LOGOUT" {
                break;
            }
        }
        seen
    });
    (addr.to_string(), handle)
}
