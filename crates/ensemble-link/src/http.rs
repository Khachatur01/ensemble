//! Just enough HTTP/1.1 for the hub's REST API, synchronous like the rest: one
//! request per connection, a `Content-Length` body, a JSON answer, then the
//! connection closes. A WebSocket upgrade is read the same way and handed to
//! the link with the bytes already read put back in front ([`Replay`]).

use serde_json::Value;
use std::io::{Read, Write};

/// The most a request's head may be.
const MAX_HEAD: usize = 16 * 1024;
/// The most a request's body may be.
pub const MAX_BODY: usize = 64 * 1024;

/// A request's first line and headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub method: String,
    /// Without the query string.
    pub path: String,
    /// Names in lowercase.
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// Is it a WebSocket upgrade?
    pub fn is_upgrade(&self) -> bool {
        self.header("upgrade")
            .is_some_and(|u| u.eq_ignore_ascii_case("websocket"))
    }

    /// The token after `Bearer `, when there is one.
    pub fn bearer(&self) -> Option<&str> {
        self.header("authorization")?
            .strip_prefix("Bearer ")
            .map(str::trim)
    }
}

/// Read a request's head. Returns it, and every byte read so far, which may
/// run past the head into the body.
pub fn read_head(stream: &mut impl Read) -> Result<(Head, Vec<u8>, usize), String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    let end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD {
            return Err("the request's head is too large".into());
        }
        let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("the connection closed before a request came".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let text = std::str::from_utf8(&buf[..end]).map_err(|_| "the request's head is not text")?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, target) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    if method.is_empty() || !target.starts_with('/') {
        return Err("not an HTTP request".into());
    }
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let head = Head {
        method: method.to_string(),
        path: target.split('?').next().unwrap_or("/").to_string(),
        headers,
    };
    Ok((head, buf, end))
}

/// Read the body that follows a head: `already` holds what was read past it.
pub fn read_body(stream: &mut impl Read, head: &Head, already: &[u8]) -> Result<Vec<u8>, String> {
    let length: usize = match head.header("content-length") {
        None => 0,
        Some(v) => v.parse().map_err(|_| "Content-Length is not a number")?,
    };
    if length > MAX_BODY {
        return Err(format!("the body is larger than {MAX_BODY} bytes"));
    }
    let mut body = already[..already.len().min(length)].to_vec();
    let mut chunk = [0u8; 4096];
    while body.len() < length {
        let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("the connection closed in the middle of the body".into());
        }
        body.extend_from_slice(&chunk[..n.min(length - body.len())]);
    }
    Ok(body)
}

/// Write a JSON response and end the exchange.
pub fn respond(stream: &mut impl Write, status: u16, body: &Value) -> std::io::Result<()> {
    let text = body.to_string();
    write!(
        stream,
        "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
        reason(status),
        text.len()
    )?;
    stream.flush()
}

/// The most read away after a refused body, before the connection is closed
/// anyway.
const MAX_DRAIN: usize = 16 * MAX_BODY;

/// Read what the client still sends, up to [`MAX_DRAIN`], after the answer
/// was written: it ends with the client's end of the request, or with the
/// stream's read timeout.
pub fn drain(stream: &mut std::net::TcpStream) {
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut chunk = [0u8; 8192];
    let mut read = 0;
    while read < MAX_DRAIN {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => read += n,
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        429 => "Too Many Requests",
        _ => "Internal Server Error",
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A stream with bytes already read put back in front of it, so a WebSocket
/// handshake can read the request the router looked at.
pub struct Replay<S> {
    first: Vec<u8>,
    at: usize,
    pub inner: S,
}

impl<S> Replay<S> {
    pub fn new(first: Vec<u8>, inner: S) -> Self {
        Replay {
            first,
            at: 0,
            inner,
        }
    }
}

impl<S: Read> Read for Replay<S> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.at < self.first.len() {
            let n = out.len().min(self.first.len() - self.at);
            out[..n].copy_from_slice(&self.first[self.at..self.at + n]);
            self.at += n;
            return Ok(n);
        }
        self.inner.read(out)
    }
}

impl<S: Write> Write for Replay<S> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_read_head_then_body() {
        let raw = b"POST /api/v1/register?x=1 HTTP/1.1\r\nHost: h\r\nContent-Length: 13\r\nAuthorization: Bearer ens_1\r\n\r\n{\"name\":\"a\"}!";
        let mut stream = &raw[..];
        let (head, read, end) = read_head(&mut stream).unwrap();
        assert_eq!(
            (head.method.as_str(), head.path.as_str()),
            ("POST", "/api/v1/register")
        );
        assert_eq!(head.bearer(), Some("ens_1"));
        assert!(!head.is_upgrade());
        let body = read_body(&mut stream, &head, &read[end..]).unwrap();
        assert_eq!(body, b"{\"name\":\"a\"}!");
    }

    #[test]
    fn a_body_too_large_or_a_head_that_is_not_http_is_refused() {
        let raw = format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        let mut stream = raw.as_bytes();
        let (head, read, end) = read_head(&mut stream).unwrap();
        assert!(read_body(&mut stream, &head, &read[end..]).is_err());
        assert!(read_head(&mut &b"hello\r\n\r\n"[..]).is_err());
    }

    #[test]
    fn replay_gives_back_what_was_read_then_the_rest() {
        let mut r = Replay::new(b"ab".to_vec(), &b"cd"[..]);
        let mut all = String::new();
        r.read_to_string(&mut all).unwrap();
        assert_eq!(all, "abcd");
    }
}
