//! A small synchronous HTTP/1.1 server.
//!
//! WHY THIS AND NOT A CRATE
//! ------------------------
//! The rest of the program is synchronous on purpose — see the note on `ureq`
//! in `Cargo.toml` — and an async server would drag a runtime in behind a
//! `ModelPort::complete` that blocks anyway. Among synchronous servers the
//! surface actually needed here is small: loopback, no TLS, two routes, one
//! request encoding, two response encodings. `tests/real_http.rs` already
//! hand-rolls the client-facing half of this in forty lines.
//!
//! The parsing is kept free of sockets for the same reason the `Http` trait in
//! `backend/openai.rs` is injected: every branch below — a truncated head, a
//! body that lies about its length, a second request on one connection — is
//! covered by a test that binds nothing.
//!
//! This module knows nothing about Ensemble, roles or OpenAI JSON. It moves
//! bytes.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub addr: SocketAddr,
    pub max_header_bytes: usize,
    pub max_body_bytes: usize,
    /// Deadline for reading one request once its first byte has arrived.
    pub read_timeout: Duration,
    pub write_timeout: Duration,
    /// How long a kept-alive connection may sit idle before it is closed.
    pub idle_timeout: Duration,
    pub workers: usize,
    pub queue_depth: usize,
    /// Cap on requests served over one connection, so a client cannot pin a
    /// worker forever.
    pub max_requests_per_connection: u32,
}

impl ServerConfig {
    pub fn new(addr: SocketAddr) -> Self {
        ServerConfig {
            addr,
            max_header_bytes: 64 * 1024,
            max_body_bytes: 8 * 1024 * 1024,
            read_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(120),
            workers: 8,
            queue_depth: 8,
            max_requests_per_connection: 100,
        }
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub method: String,
    /// Request target with the query string still attached.
    pub target: String,
    /// 0 for HTTP/1.0, 1 for HTTP/1.1.
    pub version_minor: u8,
    /// Names lowercased, values trimmed.
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// Path with the query string removed.
    pub path: String,
    pub query: String,
    pub version_minor: u8,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The bearer token, if the client sent one.
    pub fn bearer(&self) -> Option<&str> {
        let v = self.header("authorization")?;
        let (scheme, token) = v.split_once(' ')?;
        scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
    }

    fn wants_close(&self) -> bool {
        self.header("connection")
            .is_some_and(|v| v.to_ascii_lowercase().contains("close"))
    }

    fn expects_continue(&self) -> bool {
        self.header("expect")
            .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
    }
}

/// Why a request could not be read. Each maps to exactly one status, because
/// "400 Bad Request" for a body that was merely too large sends the client off
/// to debug its JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BadRequest {
    Malformed(String),
    HeadersTooLarge,
    BodyTooLarge,
    /// A chunked *request*. Refused rather than mis-read: silently treating it
    /// as a zero-length body would truncate the conversation.
    UnsupportedTransferEncoding,
    UnsupportedVersion,
}

impl BadRequest {
    pub fn status(&self) -> u16 {
        match self {
            BadRequest::Malformed(_) => 400,
            BadRequest::HeadersTooLarge => 431,
            BadRequest::BodyTooLarge => 413,
            BadRequest::UnsupportedTransferEncoding => 501,
            BadRequest::UnsupportedVersion => 505,
        }
    }

    pub fn message(&self) -> String {
        match self {
            BadRequest::Malformed(d) => format!("malformed HTTP request: {d}"),
            BadRequest::HeadersTooLarge => "request headers are too large".into(),
            BadRequest::BodyTooLarge => "request body is too large".into(),
            BadRequest::UnsupportedTransferEncoding => {
                "this server reads Content-Length bodies only; \
                 chunked request bodies are not supported"
                    .into()
            }
            BadRequest::UnsupportedVersion => "only HTTP/1.0 and HTTP/1.1 are supported".into(),
        }
    }
}

/// Parse the request line and headers. `head` excludes the blank line.
pub fn parse_head(head: &str) -> Result<Head, BadRequest> {
    let mut lines = head.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));

    let start = lines
        .next()
        .filter(|l| !l.is_empty())
        .ok_or_else(|| BadRequest::Malformed("empty request line".into()))?;

    let parts: Vec<&str> = start.split(' ').collect();
    if parts.len() != 3 {
        return Err(BadRequest::Malformed(format!(
            "request line is not `METHOD TARGET VERSION`: {start:?}"
        )));
    }
    let version_minor = match parts[2] {
        "HTTP/1.1" => 1,
        "HTTP/1.0" => 0,
        _ => return Err(BadRequest::UnsupportedVersion),
    };

    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        // Obsolete line folding. Deprecated, and a request-smuggling vector.
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(BadRequest::Malformed("folded header line".into()));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| BadRequest::Malformed(format!("header without a colon: {line:?}")))?;
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() {
            return Err(BadRequest::Malformed("header with an empty name".into()));
        }
        headers.push((name, value.trim().to_string()));
    }

    Ok(Head {
        method: parts[0].to_string(),
        target: parts[1].to_string(),
        version_minor,
        headers,
    })
}

/// How long the body is, per the head. `None` means there is no body.
fn content_length(head: &Head, max: usize) -> Result<Option<usize>, BadRequest> {
    if head.headers.iter().any(|(k, _)| k == "transfer-encoding") {
        return Err(BadRequest::UnsupportedTransferEncoding);
    }

    let mut seen: Option<usize> = None;
    for (k, v) in &head.headers {
        if k != "content-length" {
            continue;
        }
        let n: usize = v
            .parse()
            .map_err(|_| BadRequest::Malformed(format!("content-length is not a number: {v:?}")))?;
        // Two different lengths is the classic smuggling setup.
        if seen.is_some_and(|prev| prev != n) {
            return Err(BadRequest::Malformed(
                "conflicting content-length headers".into(),
            ));
        }
        seen = Some(n);
    }

    if seen.is_some_and(|n| n > max) {
        return Err(BadRequest::BodyTooLarge);
    }
    Ok(seen)
}

/// Read the head. `Ok(None)` means the peer closed cleanly before sending one,
/// which is how a kept-alive connection normally ends.
pub fn read_head<R: BufRead>(r: &mut R, cfg: &ServerConfig) -> Result<Option<Head>, BadRequest> {
    let mut raw: Vec<u8> = Vec::new();
    loop {
        let mut line = Vec::new();
        let n = r
            .read_until(b'\n', &mut line)
            .map_err(|e| BadRequest::Malformed(format!("read failed: {e}")))?;
        if n == 0 {
            return if raw.is_empty() {
                Ok(None)
            } else {
                Err(BadRequest::Malformed("connection closed mid-head".into()))
            };
        }
        let blank = line == b"\r\n" || line == b"\n";
        raw.extend_from_slice(&line);
        if raw.len() > cfg.max_header_bytes {
            return Err(BadRequest::HeadersTooLarge);
        }
        if blank {
            break;
        }
    }

    let text = String::from_utf8(raw)
        .map_err(|_| BadRequest::Malformed("the head is not valid UTF-8".into()))?;
    parse_head(&text).map(Some)
}

/// Read the body named by the head, and assemble the request.
pub fn read_body<R: BufRead>(
    r: &mut R,
    head: Head,
    cfg: &ServerConfig,
) -> Result<Request, BadRequest> {
    let len = content_length(&head, cfg.max_body_bytes)?;
    let mut body = Vec::new();
    if let Some(len) = len {
        body.resize(len, 0);
        r.read_exact(&mut body).map_err(|e| {
            BadRequest::Malformed(format!("body is shorter than its content-length: {e}"))
        })?;
    }

    let (path, query) = match head.target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (head.target.clone(), String::new()),
    };

    Ok(Request {
        method: head.method,
        path,
        query,
        version_minor: head.version_minor,
        headers: head.headers,
        body,
    })
}

/// Head plus body in one step. The server splits them so it can answer
/// `Expect: 100-continue` in between; tests rarely need to.
pub fn read_request<R: BufRead>(
    r: &mut R,
    cfg: &ServerConfig,
) -> Result<Option<Request>, BadRequest> {
    match read_head(r, cfg)? {
        None => Ok(None),
        Some(head) => read_body(r, head, cfg).map(Some),
    }
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// What a streaming handler writes into.
///
/// One writer, one thread, flushed after every event: a buffered SSE event is
/// an event the client is still waiting for, and that is indistinguishable
/// from a hung server.
pub trait EventSink {
    /// One `data:` event.
    fn event(&mut self, data: &str) -> std::io::Result<()>;
    /// One `:` comment. Keeps an idle connection alive and carries progress
    /// notes that are not part of the completion.
    fn comment(&mut self, text: &str) -> std::io::Result<()>;
}

pub type StreamBody = Box<dyn FnOnce(&mut dyn EventSink) -> std::io::Result<()> + Send>;

pub enum Body {
    Bytes(Vec<u8>),
    /// Server-sent events, written as the handler produces them.
    Stream(StreamBody),
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

impl Response {
    pub fn json(status: u16, body: String) -> Self {
        Response {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: Body::Bytes(body.into_bytes()),
        }
    }

    pub fn text(status: u16, body: &str) -> Self {
        Response {
            status,
            headers: vec![("content-type".into(), "text/plain; charset=utf-8".into())],
            body: Body::Bytes(body.as_bytes().to_vec()),
        }
    }

    pub fn sse(f: impl FnOnce(&mut dyn EventSink) -> std::io::Result<()> + Send + 'static) -> Self {
        Response {
            status: 200,
            headers: Vec::new(),
            body: Body::Stream(Box::new(f)),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn is_stream(&self) -> bool {
        matches!(self.body, Body::Stream(_))
    }
}

pub fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "Status",
    }
}

/// May this connection serve another request after this response?
pub fn keep_alive(req: &Request, resp: &Response) -> bool {
    req.version_minor == 1 && !req.wants_close() && !resp.is_stream()
}

/// Writes SSE events as HTTP/1.1 chunks.
///
/// Chunked rather than close-delimited because a closed connection cannot be
/// told apart from a truncated one, and a silently truncated answer is the
/// failure class this codebase refuses everywhere else.
struct ChunkedSink<'a, W: Write> {
    w: &'a mut W,
}

impl<W: Write> ChunkedSink<'_, W> {
    fn chunk(&mut self, s: &str) -> std::io::Result<()> {
        write!(self.w, "{:x}\r\n", s.len())?;
        self.w.write_all(s.as_bytes())?;
        self.w.write_all(b"\r\n")?;
        self.w.flush()
    }
}

impl<W: Write> EventSink for ChunkedSink<'_, W> {
    fn event(&mut self, data: &str) -> std::io::Result<()> {
        self.chunk(&format!("data: {data}\n\n"))
    }
    fn comment(&mut self, text: &str) -> std::io::Result<()> {
        self.chunk(&format!(": {text}\n\n"))
    }
}

/// Writes SSE events straight out, for an HTTP/1.0 client that has no chunked
/// encoding. The stream is then delimited by the close.
struct PlainSink<'a, W: Write> {
    w: &'a mut W,
}

impl<W: Write> EventSink for PlainSink<'_, W> {
    fn event(&mut self, data: &str) -> std::io::Result<()> {
        write!(self.w, "data: {data}\n\n")?;
        self.w.flush()
    }
    fn comment(&mut self, text: &str) -> std::io::Result<()> {
        write!(self.w, ": {text}\n\n")?;
        self.w.flush()
    }
}

pub fn write_response<W: Write>(
    w: &mut W,
    resp: Response,
    keep_alive: bool,
    version_minor: u8,
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\n", resp.status, status_text(resp.status));
    for (k, v) in &resp.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }

    match resp.body {
        Body::Bytes(bytes) => {
            head.push_str(&format!("content-length: {}\r\n", bytes.len()));
            head.push_str(if keep_alive {
                "connection: keep-alive\r\n"
            } else {
                "connection: close\r\n"
            });
            head.push_str("\r\n");
            w.write_all(head.as_bytes())?;
            w.write_all(&bytes)?;
            w.flush()
        }
        Body::Stream(f) => {
            head.push_str("content-type: text/event-stream\r\n");
            head.push_str("cache-control: no-cache\r\n");
            // Tells a reverse proxy not to sit on the events.
            head.push_str("x-accel-buffering: no\r\n");
            head.push_str("connection: close\r\n");
            let chunked = version_minor == 1;
            if chunked {
                head.push_str("transfer-encoding: chunked\r\n");
            }
            head.push_str("\r\n");
            w.write_all(head.as_bytes())?;
            w.flush()?;

            let result = if chunked {
                f(&mut ChunkedSink { w })
            } else {
                f(&mut PlainSink { w })
            };
            // The terminating chunk goes out even when the handler failed
            // part-way: a stream that simply stops is a stream the client
            // cannot tell from a dropped connection.
            if chunked {
                let _ = w.write_all(b"0\r\n\r\n");
            }
            let _ = w.flush();
            result
        }
    }
}

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

pub trait Handler: Send + Sync {
    fn handle(&self, req: &Request) -> Response;
    /// How to render a read failure. The caller owns the error vocabulary —
    /// this module does not know that OpenAI clients want a particular shape.
    fn bad_request(&self, bad: &BadRequest) -> Response {
        Response::text(bad.status(), &bad.message())
    }
    /// What to say when every worker is busy.
    fn at_capacity(&self) -> Response {
        Response::text(503, "the server is at capacity; try again shortly")
    }
}

pub struct Server {
    listener: TcpListener,
    cfg: ServerConfig,
    stop: Arc<AtomicBool>,
}

/// Stops a running server. Held by tests and by anything that owns the
/// server's lifetime.
pub struct ShutdownHandle {
    stop: Arc<AtomicBool>,
    addr: SocketAddr,
}

impl ShutdownHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocked `accept`. Cheaper and more predictable than
        // polling with a timeout, which would add latency to every accept to
        // serve a shutdown that happens once.
        let _ = TcpStream::connect(self.addr);
    }
}

impl Server {
    pub fn bind(cfg: ServerConfig) -> std::io::Result<Server> {
        let listener = TcpListener::bind(cfg.addr)?;
        Ok(Server {
            listener,
            cfg,
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The address actually bound. With port 0 this is how the caller learns
    /// which port it got.
    pub fn local_addr(&self) -> SocketAddr {
        self.listener.local_addr().unwrap_or(self.cfg.addr)
    }

    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            stop: Arc::clone(&self.stop),
            addr: self.local_addr(),
        }
    }

    /// Serve until `ShutdownHandle::stop` is called.
    ///
    /// A bounded pool, not a thread per connection: one request here can hold
    /// a worker for minutes while a model answers, so unbounded spawning is a
    /// memory sink anything on the box can reach. A full queue is an honest
    /// 503 rather than an unbounded wait.
    pub fn serve(self, handler: Arc<dyn Handler>) -> std::io::Result<()> {
        let (tx, rx) = sync_channel::<TcpStream>(self.cfg.queue_depth);
        let rx = Arc::new(Mutex::new(rx));
        let mut workers = Vec::new();

        for _ in 0..self.cfg.workers.max(1) {
            let rx: Arc<Mutex<Receiver<TcpStream>>> = Arc::clone(&rx);
            let handler = Arc::clone(&handler);
            let cfg = self.cfg.clone();
            workers.push(std::thread::spawn(move || {
                loop {
                    let stream = {
                        let guard = rx.lock().unwrap_or_else(|e| e.into_inner());
                        guard.recv()
                    };
                    match stream {
                        Ok(s) => serve_connection(s, &*handler, &cfg),
                        Err(_) => break, // the sender is gone: shutting down
                    }
                }
            }));
        }

        let tx: SyncSender<TcpStream> = tx;
        for incoming in self.listener.incoming() {
            if self.stop.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = incoming else { continue };
            match tx.try_send(stream) {
                Ok(()) => {}
                Err(TrySendError::Full(mut stream)) => {
                    let resp = handler.at_capacity().with_header("retry-after", "1");
                    let _ = write_response(&mut stream, resp, false, 1);
                }
                Err(TrySendError::Disconnected(_)) => break,
            }
        }

        drop(tx);
        for w in workers {
            let _ = w.join();
        }
        Ok(())
    }
}

fn serve_connection(stream: TcpStream, handler: &dyn Handler, cfg: &ServerConfig) {
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let _ = stream.set_write_timeout(Some(cfg.write_timeout));
    let mut reader = BufReader::new(stream);
    let mut writer = write_half;

    for _ in 0..cfg.max_requests_per_connection {
        let _ = reader.get_ref().set_read_timeout(Some(cfg.idle_timeout));
        let head = match read_head(&mut reader, cfg) {
            Ok(None) => return,
            Ok(Some(h)) => h,
            Err(bad) => {
                let resp = handler.bad_request(&bad);
                let _ = write_response(&mut writer, resp, false, 1);
                return;
            }
        };

        // Past the first byte the client is mid-request; a stall now is a
        // broken peer rather than an idle one.
        let _ = reader.get_ref().set_read_timeout(Some(cfg.read_timeout));
        let version_minor = head.version_minor;
        let expects_continue = head
            .headers
            .iter()
            .any(|(k, v)| k == "expect" && v.eq_ignore_ascii_case("100-continue"));

        if expects_continue {
            // Without this, curl waits out its own one-second timer first.
            if writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_err() {
                return;
            }
            let _ = writer.flush();
        }

        let request = match read_body(&mut reader, head, cfg) {
            Ok(r) => r,
            Err(bad) => {
                let resp = handler.bad_request(&bad);
                let _ = write_response(&mut writer, resp, false, version_minor);
                return;
            }
        };

        debug_assert!(request.expects_continue() == expects_continue);

        let response = handler.handle(&request);
        let alive = keep_alive(&request, &response);
        if write_response(&mut writer, response, alive, version_minor).is_err() || !alive {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

    fn cfg() -> ServerConfig {
        ServerConfig::new("127.0.0.1:0".parse().unwrap())
    }

    fn read(bytes: &str) -> Result<Option<Request>, BadRequest> {
        let mut c = Cursor::new(bytes.as_bytes().to_vec());
        read_request(&mut c, &cfg())
    }

    #[test]
    fn a_request_with_content_length_is_read_exactly_and_not_one_byte_further() {
        let mut c = Cursor::new(
            b"POST /v1/chat/completions HTTP/1.1\r\nhost: x\r\ncontent-length: 2\r\n\r\n{}LEFTOVER"
                .to_vec(),
        );
        let r = read_request(&mut c, &cfg()).unwrap().unwrap();
        assert_eq!(r.body, b"{}");
        let mut rest = String::new();
        c.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "LEFTOVER", "the next request's bytes are untouched");
    }

    #[test]
    fn two_requests_on_one_connection_are_both_read() {
        let mut c = Cursor::new(
            b"GET /v1/models HTTP/1.1\r\nhost: x\r\n\r\nGET /v1/models HTTP/1.1\r\nhost: x\r\n\r\n"
                .to_vec(),
        );
        assert_eq!(
            read_request(&mut c, &cfg()).unwrap().unwrap().path,
            "/v1/models"
        );
        assert_eq!(
            read_request(&mut c, &cfg()).unwrap().unwrap().path,
            "/v1/models"
        );
        assert!(read_request(&mut c, &cfg()).unwrap().is_none(), "then EOF");
    }

    #[test]
    fn the_query_string_is_split_off_the_path() {
        let r = read("GET /v1/models?limit=1 HTTP/1.1\r\nhost: x\r\n\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(r.path, "/v1/models");
        assert_eq!(r.query, "limit=1");
    }

    #[test]
    fn a_chunked_request_body_is_refused_rather_than_mis_read() {
        // Treating it as a zero-length body would truncate the conversation
        // and look like the client sent nothing.
        let err = read("POST /x HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n").unwrap_err();
        assert_eq!(err, BadRequest::UnsupportedTransferEncoding);
        assert_eq!(err.status(), 501);
        assert!(
            err.message().contains("Content-Length"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn a_body_over_the_limit_is_refused_before_it_is_buffered() {
        let mut c = ServerConfig::new("127.0.0.1:0".parse().unwrap());
        c.max_body_bytes = 10;
        let mut r = Cursor::new(b"POST /x HTTP/1.1\r\ncontent-length: 99\r\n\r\n".to_vec());
        let err = read_request(&mut r, &c).unwrap_err();
        assert_eq!(err, BadRequest::BodyTooLarge);
        assert_eq!(err.status(), 413);
    }

    #[test]
    fn a_head_over_the_limit_is_a_431_not_a_400() {
        let mut c = ServerConfig::new("127.0.0.1:0".parse().unwrap());
        c.max_header_bytes = 40;
        let long = format!("GET /x HTTP/1.1\r\nx-pad: {}\r\n\r\n", "y".repeat(200));
        let mut r = Cursor::new(long.into_bytes());
        assert_eq!(
            read_request(&mut r, &c).unwrap_err(),
            BadRequest::HeadersTooLarge
        );
    }

    #[test]
    fn conflicting_content_lengths_are_refused() {
        let err = read("POST /x HTTP/1.1\r\ncontent-length: 1\r\ncontent-length: 2\r\n\r\nxx")
            .unwrap_err();
        assert!(matches!(err, BadRequest::Malformed(_)), "{err:?}");
    }

    #[test]
    fn a_folded_header_line_is_refused() {
        // Deprecated by the spec and a request-smuggling vector.
        let err = read("GET /x HTTP/1.1\r\nhost: a\r\n  continued\r\n\r\n").unwrap_err();
        assert!(matches!(err, BadRequest::Malformed(_)), "{err:?}");
    }

    #[test]
    fn an_unsupported_version_is_a_505() {
        assert_eq!(
            read("GET /x HTTP/2.0\r\n\r\n").unwrap_err(),
            BadRequest::UnsupportedVersion
        );
    }

    #[test]
    fn a_clean_close_before_any_request_is_not_an_error() {
        assert!(read("").unwrap().is_none());
    }

    #[test]
    fn header_names_are_lowercased_and_values_trimmed() {
        let r = read("GET /x HTTP/1.1\r\nAuthorization:   Bearer sk-abc   \r\n\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(r.header("authorization"), Some("Bearer sk-abc"));
        assert_eq!(r.bearer(), Some("sk-abc"));
    }

    #[test]
    fn a_non_bearer_authorization_scheme_yields_no_token() {
        let r = read("GET /x HTTP/1.1\r\nauthorization: Basic abc\r\n\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(r.bearer(), None);
    }

    #[test]
    fn keep_alive_holds_for_http_11_and_not_for_a_stream_or_an_explicit_close() {
        let plain = read("GET /x HTTP/1.1\r\n\r\n").unwrap().unwrap();
        assert!(keep_alive(&plain, &Response::text(200, "hi")));

        let closing = read("GET /x HTTP/1.1\r\nconnection: close\r\n\r\n")
            .unwrap()
            .unwrap();
        assert!(!keep_alive(&closing, &Response::text(200, "hi")));

        let old = read("GET /x HTTP/1.0\r\n\r\n").unwrap().unwrap();
        assert!(!keep_alive(&old, &Response::text(200, "hi")));

        // An SSE response ends its connection, always.
        assert!(!keep_alive(&plain, &Response::sse(|_| Ok(()))));
    }

    #[test]
    fn a_json_response_carries_its_length_and_its_body() {
        let mut out = Vec::new();
        write_response(&mut out, Response::json(200, "{\"a\":1}".into()), true, 1).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"), "{s}");
        assert!(s.contains("content-length: 7\r\n"), "{s}");
        assert!(s.contains("connection: keep-alive\r\n"), "{s}");
        assert!(s.ends_with("\r\n\r\n{\"a\":1}"), "{s}");
    }

    #[test]
    fn the_sse_stream_is_chunked_and_each_event_is_its_own_chunk() {
        let mut out = Vec::new();
        write_response(
            &mut out,
            Response::sse(|sink| {
                sink.comment("keep-alive")?;
                sink.event("{\"a\":1}")?;
                sink.event("[DONE]")
            }),
            false,
            1,
        )
        .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("content-type: text/event-stream\r\n"), "{s}");
        assert!(s.contains("transfer-encoding: chunked\r\n"), "{s}");
        assert!(s.contains("connection: close\r\n"), "{s}");
        assert!(s.contains(": keep-alive\n\n"), "{s}");
        assert!(s.contains("data: {\"a\":1}\n\n"), "{s}");
        assert!(s.contains("data: [DONE]\n\n"), "{s}");
        assert!(s.ends_with("0\r\n\r\n"), "the stream is terminated: {s}");
    }

    #[test]
    fn an_http_10_client_gets_a_close_delimited_stream_instead_of_chunks() {
        let mut out = Vec::new();
        write_response(&mut out, Response::sse(|s| s.event("[DONE]")), false, 0).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("transfer-encoding"), "{s}");
        assert!(s.ends_with("data: [DONE]\n\n"), "{s}");
    }

    #[test]
    fn the_terminating_chunk_is_written_even_when_the_handler_fails() {
        // Otherwise a failed turn is indistinguishable from a dropped socket.
        let mut out = Vec::new();
        let r = write_response(
            &mut out,
            Response::sse(|sink| {
                sink.event("partial")?;
                Err(std::io::Error::other("boom"))
            }),
            false,
            1,
        );
        assert!(r.is_err());
        assert!(String::from_utf8(out).unwrap().ends_with("0\r\n\r\n"));
    }
}
