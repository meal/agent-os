//! A local fake of the Messages API: a std TCP server on 127.0.0.1 with one detached thread
//! per connection. Its depth rule is written independently of `FakeProvider` (the oracle).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub enum Reply {
    Raw(Vec<u8>),
    Transcript(PathBuf),
    Status(u16, String),
    Hang(Duration),
    CloseMidBody,
    /// A redirect with this status to this `Location`.
    Redirect(u16, String),
    /// Closes the connection without writing anything.
    CloseAtOnce,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub struct FakeApi {
    pub addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl FakeApi {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line).ok()?;
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        let (k, v) = l.split_once(':')?;
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
    let len = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok()?;
    Some(Request { headers, body })
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

fn respond(stream: &mut TcpStream, status: u16, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        status_text(status),
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// The transcript entry for a request, by the depth of its conversation.
fn transcript_reply(path: &PathBuf, body: &[u8]) -> (u16, Vec<u8>) {
    let file: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let request: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    let depth = request["messages"].as_array().map_or(0, |m| m.len());
    if depth.is_multiple_of(2) {
        return (400, format!("depth {depth}").into_bytes());
    }
    match file["responses"].get(depth / 2) {
        Some(entry) => (200, serde_json::to_vec(&entry["response"]).unwrap()),
        None => (400, format!("no entry at depth {depth}").into_bytes()),
    }
}

fn handle(mut stream: TcpStream, reply: &Reply, hits: &AtomicUsize, requests: &Mutex<Vec<Request>>) {
    let Some(req) = read_request(&stream) else { return };
    hits.fetch_add(1, Ordering::SeqCst);
    let body = req.body.clone();
    requests.lock().unwrap().push(req);
    match reply {
        Reply::Raw(bytes) => {
            let _ = stream.write_all(bytes);
            let _ = stream.flush();
        }
        Reply::Transcript(path) => {
            let (status, out) = transcript_reply(path, &body);
            respond(&mut stream, status, &out);
        }
        Reply::Status(status, text) => respond(&mut stream, *status, text.as_bytes()),
        Reply::Hang(d) => thread::sleep(*d),
        Reply::CloseAtOnce => {}
        Reply::Redirect(status, location) => {
            let head = format!(
                "HTTP/1.1 {status} Redirect\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.flush();
        }
        Reply::CloseMidBody => {
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100\r\nconnection: close\r\n\r\n0123456789",
            );
            let _ = stream.flush();
        }
    }
}

pub fn serve(reply: Reply) -> FakeApi {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake API");
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let reply = Arc::new(reply);
    let (h, r) = (hits.clone(), requests.clone());
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (reply, h, r) = (reply.clone(), h.clone(), r.clone());
            thread::spawn(move || handle(stream, &reply, &h, &r));
        }
    });
    FakeApi { addr, hits, requests }
}
