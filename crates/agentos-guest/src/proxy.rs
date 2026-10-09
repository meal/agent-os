//! The guest's loopback model proxy: the CLI in the VM talks to `127.0.0.1` as if it were the
//! Messages API, and each `POST /v1/messages` is handed to a [`Bridge`] (the guest session
//! sends it to the host as a `ModelRequest` and returns the `ModelReply`). Everything else is
//! answered here, without the bridge and without the host: the CLI must never hang on a path
//! the proxy does not serve.
//!
//! Blocking `std::net` and one thread per connection; one request per connection, every reply
//! carries `Connection: close`. Only the body crosses to the bridge: request headers are read
//! for framing and then dropped, and neither headers nor bodies are ever logged or echoed.

use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use agentos_core::guest::RAW_FRAME_LIMIT;
use agentos_core::messages::{sse_from_message, wants_stream};
use serde_json::{Value, json};

/// Per-connection budget for reading the request: a client that sends nothing, or trickles
/// bytes, for this long is answered 408 and closed.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// The request line and headers, up to and including the blank line.
pub const HEADER_LIMIT: usize = 16 * 1024;
/// The largest request body accepted (the raw-frame limit of the guest protocol).
pub const BODY_LIMIT: usize = RAW_FRAME_LIMIT;
/// After a reply the connection is half-closed and whatever the client still sends is read
/// (bounded) so that closing does not reset the reply away. Capped by the read timeout.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const DRAIN_LIMIT: usize = 1 << 20;

/// Forwards one Messages request body to the host and returns the upstream status and body.
/// `Err` means the bridge is gone; the proxy answers 502.
pub trait Bridge: Send + Sync {
    fn forward(&self, body: Vec<u8>) -> Result<(u16, Vec<u8>), String>;
}

/// A running proxy. Dropping it stops it; [`Proxy::stop`] does so explicitly and joins.
pub struct Proxy {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl Proxy {
    /// Binds `127.0.0.1:0` and serves with [`READ_TIMEOUT`].
    pub fn start(bridge: Arc<dyn Bridge>) -> io::Result<Proxy> {
        Proxy::start_with_timeout(bridge, READ_TIMEOUT)
    }

    /// Same as [`Proxy::start`] with the read timeout as a parameter (tests use a short one).
    pub fn start_with_timeout(
        bridge: Arc<dyn Bridge>,
        read_timeout: Duration,
    ) -> io::Result<Proxy> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let stop = stop.clone();
            thread::Builder::new()
                .name("guest-proxy-accept".into())
                .spawn(move || accept_loop(listener, bridge, stop, read_timeout))?
        };
        Ok(Proxy {
            addr,
            stop,
            accept: Some(accept),
        })
    }

    /// Stops accepting and joins the accept thread. Connections already being served finish
    /// on their own threads (each is bounded by the read timeout).
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let Some(handle) = self.accept.take() else {
            return;
        };
        self.stop.store(true, Ordering::SeqCst);
        // The accept thread is blocked in `accept`; one connection wakes it to see the flag.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
        let _ = handle.join();
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(
    listener: TcpListener,
    bridge: Arc<dyn Bridge>,
    stop: Arc<AtomicBool>,
    read_timeout: Duration,
) {
    for connection in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match connection {
            Ok(stream) => {
                let bridge = bridge.clone();
                // A failed spawn drops the connection; the client sees a reset and retries.
                let _ = thread::Builder::new()
                    .name("guest-proxy-conn".into())
                    .spawn(move || serve(stream, bridge.as_ref(), read_timeout));
            }
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Why a request was refused before the bridge was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// The client went away without a complete request: nothing to answer.
    Silent,
    /// No complete request within the read timeout.
    Timeout,
    /// The header block is over `HEADER_LIMIT`.
    HeadersTooLarge,
    /// `Content-Length` is over `BODY_LIMIT`; the body is not read.
    BodyTooLarge,
    /// Request line, header or `Content-Length` is not well-formed.
    Malformed,
    /// A request body without `Content-Length` (chunked or absent).
    LengthRequired,
}

impl Failure {
    fn response(self) -> Option<Response> {
        let (status, kind, message) = match self {
            Failure::Silent => return None,
            Failure::Timeout => (408, "invalid_request_error", "request timed out"),
            Failure::HeadersTooLarge => (431, "invalid_request_error", "request headers too large"),
            Failure::BodyTooLarge => (413, "invalid_request_error", "request body too large"),
            Failure::Malformed => (400, "invalid_request_error", "malformed request"),
            Failure::LengthRequired => (411, "invalid_request_error", "content-length required"),
        };
        Some(Response::json(status, &error_body(kind, message)))
    }
}

struct Head {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn header_values(&self, name: &str) -> impl Iterator<Item = &str> {
        self.headers
            .iter()
            .filter(move |(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn json(status: u16, body: &Value) -> Response {
        Response {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(body).unwrap_or_default(),
        }
    }

    fn encode(&self) -> Vec<u8> {
        let head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.status,
            reason(self.status),
            self.content_type,
            self.body.len()
        );
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

fn error_body(kind: &str, message: &str) -> Value {
    json!({"type": "error", "error": {"type": kind, "message": message}})
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        529 => "Overloaded",
        _ => "Upstream Status",
    }
}

fn serve(stream: TcpStream, bridge: &dyn Bridge, read_timeout: Duration) {
    let _ = stream.set_read_timeout(Some(read_timeout));
    let _ = stream.set_nodelay(true);
    let mut reader = BufReader::new(stream);
    let deadline = Instant::now() + read_timeout;
    let response = match handle(&mut reader, bridge, deadline) {
        Ok(response) => Some(response),
        Err(failure) => failure.response(),
    };
    let stream = reader.get_mut();
    if let Some(response) = response {
        let _ = stream.write_all(&response.encode());
    }
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(read_timeout.min(DRAIN_TIMEOUT)));
    let mut sink = [0u8; 8192];
    let mut drained = 0;
    while drained < DRAIN_LIMIT {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained += n,
        }
    }
}

fn handle(
    reader: &mut BufReader<TcpStream>,
    bridge: &dyn Bridge,
    deadline: Instant,
) -> Result<Response, Failure> {
    let raw = read_head(reader, deadline)?;
    let head = parse_head(&raw)?;
    if head.method != "POST" || head.path != "/v1/messages" {
        // Answered here, never forwarded, and the body (if any) is not read.
        return Ok(Response::json(
            404,
            &error_body("not_found_error", "not served by the agent proxy"),
        ));
    }
    if head.header_values("transfer-encoding").next().is_some() {
        return Err(Failure::LengthRequired);
    }
    let mut lengths = head.header_values("content-length");
    let length = match (lengths.next(), lengths.next()) {
        (None, _) => return Err(Failure::LengthRequired),
        (Some(_), Some(_)) => return Err(Failure::Malformed),
        (Some(value), None) => parse_length(value)?,
    };
    if length > BODY_LIMIT {
        return Err(Failure::BodyTooLarge);
    }
    let body = read_body(reader, length, deadline)?;
    Ok(messages(bridge, body))
}

/// Reads up to and including the blank line that ends the header block, one byte at a time
/// through the buffered reader, and never more than `HEADER_LIMIT` bytes.
fn read_head(reader: &mut impl Read, deadline: Instant) -> Result<Vec<u8>, Failure> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if Instant::now() >= deadline {
            return Err(Failure::Timeout);
        }
        match reader.read(&mut byte) {
            Ok(0) => return Err(Failure::Silent),
            Ok(_) => head.push(byte[0]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if is_timeout(&e) => return Err(Failure::Timeout),
            Err(_) => return Err(Failure::Silent),
        }
        if head.len() > HEADER_LIMIT {
            return Err(Failure::HeadersTooLarge);
        }
        if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            return Ok(head);
        }
    }
}

fn read_body(reader: &mut impl Read, length: usize, deadline: Instant) -> Result<Vec<u8>, Failure> {
    let mut body = vec![0u8; length];
    let mut filled = 0;
    while filled < length {
        if Instant::now() >= deadline {
            return Err(Failure::Timeout);
        }
        match reader.read(&mut body[filled..]) {
            Ok(0) => return Err(Failure::Silent),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if is_timeout(&e) => return Err(Failure::Timeout),
            Err(_) => return Err(Failure::Silent),
        }
    }
    Ok(body)
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn parse_length(value: &str) -> Result<usize, Failure> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Failure::Malformed);
    }
    // Digits that do not fit in a usize are certainly over the limit.
    Ok(value.parse::<usize>().unwrap_or(usize::MAX))
}

fn parse_head(raw: &[u8]) -> Result<Head, Failure> {
    let text = std::str::from_utf8(raw).map_err(|_| Failure::Malformed)?;
    let mut lines = text.lines();
    let request_line = lines.next().ok_or(Failure::Malformed)?;
    let parts: Vec<&str> = request_line.split(' ').collect();
    let [method, target, version] = parts[..] else {
        return Err(Failure::Malformed);
    };
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(Failure::Malformed);
    }
    if !target.starts_with('/') || !version.starts_with("HTTP/1.") {
        return Err(Failure::Malformed);
    }
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or(Failure::Malformed)?;
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(Failure::Malformed);
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
    }
    let path = target.split('?').next().unwrap_or(target);
    Ok(Head {
        method: method.to_string(),
        path: path.to_string(),
        headers,
    })
}

/// The `/v1/messages` answer: the upstream reply relayed, or SSE built from it when the
/// request asked for `stream: true` and the upstream answered 200 with a JSON message.
fn messages(bridge: &dyn Bridge, body: Vec<u8>) -> Response {
    let streaming = wants_stream(&body);
    match bridge.forward(body) {
        Ok((status, reply)) if (100..=999).contains(&status) => {
            if status == 200
                && streaming
                && let Ok(message) = serde_json::from_slice::<Value>(&reply)
            {
                return Response {
                    status,
                    content_type: "text/event-stream",
                    body: sse_from_message(&message),
                };
            }
            Response {
                status,
                content_type: "application/json",
                body: reply,
            }
        }
        _ => Response::json(502, &error_body("api_error", "model bridge unavailable")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;
    use std::time::Instant;

    /// Records every forwarded body and answers with a fixed reply.
    struct FakeBridge {
        reply: Result<(u16, Vec<u8>), String>,
        calls: Mutex<Vec<Vec<u8>>>,
    }

    impl FakeBridge {
        fn new(reply: Result<(u16, Vec<u8>), String>) -> Arc<FakeBridge> {
            Arc::new(FakeBridge {
                reply,
                calls: Mutex::new(Vec::new()),
            })
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl Bridge for FakeBridge {
        fn forward(&self, body: Vec<u8>) -> Result<(u16, Vec<u8>), String> {
            self.calls.lock().unwrap().push(body);
            self.reply.clone()
        }
    }

    const MESSAGE: &str = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":3,"output_tokens":1}}"#;

    fn start(bridge: Arc<FakeBridge>) -> Proxy {
        Proxy::start_with_timeout(bridge, Duration::from_secs(5)).unwrap()
    }

    /// Sends raw bytes, reads the whole reply (the proxy closes after it), and splits it.
    fn exchange(addr: SocketAddr, request: &[u8]) -> (String, Vec<u8>) {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(request).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("reply has a header block");
        let head = String::from_utf8(raw[..split].to_vec()).unwrap();
        (head, raw[split + 4..].to_vec())
    }

    fn status_line(head: &str) -> &str {
        head.lines().next().unwrap()
    }

    fn post(path: &str, body: &[u8]) -> Vec<u8> {
        let mut request = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(body);
        request
    }

    fn messages_body(stream: bool) -> Vec<u8> {
        format!(
            r#"{{"model":"claude-opus-5-5","max_tokens":4096,"stream":{stream},"messages":[{{"role":"user","content":"hi"}}]}}"#
        )
        .into_bytes()
    }

    #[test]
    fn plain_call_returns_the_upstream_body_and_status() {
        let bridge = FakeBridge::new(Ok((200, MESSAGE.as_bytes().to_vec())));
        let proxy = start(bridge.clone());
        let body = messages_body(false);
        let (head, reply) = exchange(proxy.addr, &post("/v1/messages", &body));
        assert_eq!(status_line(&head), "HTTP/1.1 200 OK");
        assert!(
            head.to_ascii_lowercase()
                .contains("content-type: application/json")
        );
        assert!(head.to_ascii_lowercase().contains("connection: close"));
        assert_eq!(reply, MESSAGE.as_bytes());
        assert_eq!(bridge.calls.lock().unwrap().as_slice(), &[body]);
        proxy.stop();
    }

    #[test]
    fn streaming_call_returns_a_valid_sse_sequence() {
        let bridge = FakeBridge::new(Ok((200, MESSAGE.as_bytes().to_vec())));
        let proxy = start(bridge.clone());
        let (head, reply) = exchange(proxy.addr, &post("/v1/messages", &messages_body(true)));
        assert_eq!(status_line(&head), "HTTP/1.1 200 OK");
        assert!(
            head.to_ascii_lowercase()
                .contains("content-type: text/event-stream")
        );
        let text = String::from_utf8(reply).unwrap();
        assert!(text.starts_with("event: message_start\n"), "{text}");
        let last_event = text.trim_end().rsplit("\n\n").next().unwrap();
        assert!(
            last_event.starts_with("event: message_stop\n"),
            "{last_event}"
        );
        assert_eq!(
            text,
            String::from_utf8(sse_from_message(&serde_json::from_str(MESSAGE).unwrap())).unwrap()
        );
        assert_eq!(bridge.call_count(), 1);
        proxy.stop();
    }

    #[test]
    fn query_string_is_ignored_for_routing_and_forwarded() {
        let bridge = FakeBridge::new(Ok((200, MESSAGE.as_bytes().to_vec())));
        let proxy = start(bridge.clone());
        let body = messages_body(false);
        let (head, _) = exchange(proxy.addr, &post("/v1/messages?beta=true", &body));
        assert_eq!(status_line(&head), "HTTP/1.1 200 OK");
        assert_eq!(bridge.call_count(), 1);
        proxy.stop();
    }

    #[test]
    fn count_tokens_and_unknown_paths_are_404_without_the_bridge() {
        let bridge = FakeBridge::new(Ok((200, MESSAGE.as_bytes().to_vec())));
        let proxy = start(bridge.clone());
        let body = messages_body(false);
        for path in [
            "/v1/messages/count_tokens",
            "/api/event_logging",
            "/anything",
        ] {
            let (head, reply) = exchange(proxy.addr, &post(path, &body));
            assert_eq!(status_line(&head), "HTTP/1.1 404 Not Found", "{path}");
            assert!(
                head.to_ascii_lowercase()
                    .contains("content-type: application/json")
            );
            let json: Value = serde_json::from_slice(&reply).unwrap();
            assert_eq!(json["error"]["type"], "not_found_error", "{path}");
        }
        let (head, _) = exchange(proxy.addr, b"GET /v1/messages HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status_line(&head), "HTTP/1.1 404 Not Found");
        assert_eq!(bridge.call_count(), 0);
        proxy.stop();
    }

    #[test]
    fn oversized_body_is_413_and_never_read_or_forwarded() {
        let bridge = FakeBridge::new(Ok((200, MESSAGE.as_bytes().to_vec())));
        let proxy = start(bridge.clone());
        // Only the head is sent: the proxy must answer from Content-Length alone.
        let head_only = format!(
            "POST /v1/messages HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
            BODY_LIMIT + 1
        );
        let (head, _) = exchange(proxy.addr, head_only.as_bytes());
        assert_eq!(status_line(&head), "HTTP/1.1 413 Payload Too Large");
        assert_eq!(bridge.call_count(), 0);
        proxy.stop();
    }

    #[test]
    fn huge_header_block_is_431() {
        let bridge = FakeBridge::new(Ok((200, MESSAGE.as_bytes().to_vec())));
        let proxy = start(bridge.clone());
        let mut request = b"POST /v1/messages HTTP/1.1\r\nHost: x\r\n".to_vec();
        request.extend_from_slice(b"X-Pad: ");
        request.extend(std::iter::repeat_n(b'a', HEADER_LIMIT + 1024));
        request.extend_from_slice(b"\r\n\r\n");
        let (head, _) = exchange(proxy.addr, &request);
        assert_eq!(
            status_line(&head),
            "HTTP/1.1 431 Request Header Fields Too Large"
        );
        assert_eq!(bridge.call_count(), 0);
        proxy.stop();
    }

    #[test]
    fn upstream_error_status_and_body_pass_through() {
        let overloaded =
            br#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#;
        let bridge = FakeBridge::new(Ok((529, overloaded.to_vec())));
        let proxy = start(bridge.clone());
        let (head, reply) = exchange(proxy.addr, &post("/v1/messages", &messages_body(true)));
        assert_eq!(status_line(&head).split(' ').nth(1), Some("529"));
        assert!(
            head.to_ascii_lowercase()
                .contains("content-type: application/json")
        );
        assert_eq!(reply, overloaded);
        proxy.stop();
    }

    #[test]
    fn bridge_failure_is_502_with_the_api_error_shape() {
        let bridge = FakeBridge::new(Err("host gone".into()));
        let proxy = start(bridge.clone());
        let (head, reply) = exchange(proxy.addr, &post("/v1/messages", &messages_body(false)));
        assert_eq!(status_line(&head), "HTTP/1.1 502 Bad Gateway");
        let json: Value = serde_json::from_slice(&reply).unwrap();
        assert_eq!(json["type"], "error");
        assert_eq!(json["error"]["type"], "api_error");
        assert_eq!(json["error"]["message"], "model bridge unavailable");
        assert!(!String::from_utf8_lossy(&reply).contains("host gone"));
        proxy.stop();
    }

    #[test]
    fn chunked_request_body_is_411() {
        let bridge = FakeBridge::new(Ok((200, MESSAGE.as_bytes().to_vec())));
        let proxy = start(bridge.clone());
        let request = b"POST /v1/messages HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let (head, _) = exchange(proxy.addr, request);
        assert_eq!(status_line(&head), "HTTP/1.1 411 Length Required");
        assert_eq!(bridge.call_count(), 0);
        proxy.stop();
    }

    #[test]
    fn post_without_content_length_is_411() {
        let bridge = FakeBridge::new(Ok((200, MESSAGE.as_bytes().to_vec())));
        let proxy = start(bridge.clone());
        let (head, _) = exchange(proxy.addr, b"POST /v1/messages HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(status_line(&head), "HTTP/1.1 411 Length Required");
        assert_eq!(bridge.call_count(), 0);
        proxy.stop();
    }

    #[test]
    fn malformed_request_line_is_400() {
        let bridge = FakeBridge::new(Ok((200, MESSAGE.as_bytes().to_vec())));
        let proxy = start(bridge.clone());
        let (head, _) = exchange(proxy.addr, b"this is not http\r\n\r\n");
        assert_eq!(status_line(&head), "HTTP/1.1 400 Bad Request");
        assert_eq!(bridge.call_count(), 0);
        proxy.stop();
    }

    #[test]
    fn silent_client_is_408_and_closed_within_the_timeout() {
        let bridge = FakeBridge::new(Ok((200, MESSAGE.as_bytes().to_vec())));
        let proxy = Proxy::start_with_timeout(bridge.clone(), Duration::from_millis(200)).unwrap();
        let started = Instant::now();
        let mut stream = TcpStream::connect(proxy.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "closed after {:?}",
            started.elapsed()
        );
        assert!(String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 408"));
        assert_eq!(bridge.call_count(), 0);
        proxy.stop();
    }

    #[test]
    fn stop_joins_promptly() {
        let bridge = FakeBridge::new(Ok((200, MESSAGE.as_bytes().to_vec())));
        let proxy = start(bridge);
        let started = Instant::now();
        proxy.stop();
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
