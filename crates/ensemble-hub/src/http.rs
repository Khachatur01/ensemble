//! Just enough HTTP/1.1 for the hub's REST API, on tokio: one
//! request per connection, a `Content-Length` body, a JSON answer, then the
//! connection closes. A WebSocket upgrade is read the same way and handed to
//! the link with the bytes already read put back in front ([`Replay`]).

use serde_json::Value;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

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

    /// The address the client reached the hub at, such as
    /// `https://hub.example.com`: from a reverse proxy's `X-Forwarded-Proto`
    /// and `X-Forwarded-Host` when it sends them, else from `Host`.
    pub fn reached_at(&self) -> Option<String> {
        let first = |name: &str| Some(self.header(name)?.split(',').next()?.trim());
        let host = first("x-forwarded-host").or_else(|| first("host"))?;
        if host.is_empty() || host.contains(['/', ' ', '@']) {
            return None;
        }
        let scheme = match first("x-forwarded-proto") {
            Some(p) if p.eq_ignore_ascii_case("https") => "https",
            _ => "http",
        };
        Some(format!("{scheme}://{host}"))
    }

    /// The token after `Bearer `, when there is one.
    pub fn bearer(&self) -> Option<&str> {
        self.header("authorization")?
            .strip_prefix("Bearer ")
            .map(str::trim)
    }

    /// The value of the cookie `name`, when the request carries it.
    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .filter(|(n, _)| n == "cookie")
            .flat_map(|(_, v)| v.split(';'))
            .filter_map(|c| c.trim().split_once('='))
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.trim())
    }

    /// Does it only read, as a GET or a HEAD does?
    pub fn is_reading(&self) -> bool {
        matches!(self.method.as_str(), "GET" | "HEAD")
    }

    /// Is it a browser's request from a page of another origin than the
    /// address it reached? Such a page is refused: the hub has no CORS.
    pub fn is_from_elsewhere(&self) -> bool {
        self.header("origin")
            .is_some_and(|origin| Some(origin) != self.reached_at().as_deref())
    }
}

/// Read a request's head. Returns it, and every byte read so far, which may
/// run past the head into the body.
pub async fn read_head(
    stream: &mut (impl AsyncRead + Unpin),
) -> Result<(Head, Vec<u8>, usize), String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    let end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD {
            return Err("the request's head is too large".into());
        }
        let n = stream.read(&mut chunk).await.map_err(|e| e.to_string())?;
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
pub async fn read_body(
    stream: &mut (impl AsyncRead + Unpin),
    head: &Head,
    already: &[u8],
) -> Result<Vec<u8>, String> {
    read_body_up_to(stream, head, already, MAX_BODY).await
}

/// [`read_body`], for a body of at most `max` bytes.
pub async fn read_body_up_to(
    stream: &mut (impl AsyncRead + Unpin),
    head: &Head,
    already: &[u8],
    max: usize,
) -> Result<Vec<u8>, String> {
    let length: usize = match head.header("content-length") {
        None => 0,
        Some(v) => v.parse().map_err(|_| "Content-Length is not a number")?,
    };
    if length > max {
        return Err(format!("the body is larger than {max} bytes"));
    }
    let mut body = already[..already.len().min(length)].to_vec();
    let mut chunk = [0u8; 4096];
    while body.len() < length {
        let n = stream.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("the connection closed in the middle of the body".into());
        }
        body.extend_from_slice(&chunk[..n.min(length - body.len())]);
    }
    Ok(body)
}

/// Write a JSON response and end the exchange.
pub async fn respond(
    stream: &mut (impl AsyncWrite + Unpin),
    status: u16,
    body: &Value,
) -> std::io::Result<()> {
    respond_with(stream, status, &[], Some(body)).await
}

/// [`respond`], with more headers, and with no body when `body` is `None`.
pub async fn respond_with(
    stream: &mut (impl AsyncWrite + Unpin),
    status: u16,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> std::io::Result<()> {
    let text = body.map(Value::to_string).unwrap_or_default();
    let mut response = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    if body.is_some() {
        response.push_str("content-type: application/json\r\n");
    }
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str(&format!(
        "cache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
        text.len()
    ));
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

/// Write a response of `bytes`, of the type `content_type`, and end the
/// exchange.
pub async fn respond_bytes(
    stream: &mut (impl AsyncWrite + Unpin),
    status: u16,
    content_type: &str,
    headers: &[(&str, &str)],
    bytes: &[u8],
) -> std::io::Result<()> {
    let mut response = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: {content_type}\r\n",
        reason(status)
    );
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n",
        bytes.len()
    ));
    stream.write_all(response.as_bytes()).await?;
    stream.write_all(bytes).await?;
    stream.flush().await
}

/// The most read away after a refused body, before the connection is closed
/// anyway.
const MAX_DRAIN: usize = 16 * MAX_BODY;

/// How long [`drain`] waits for the client.
const DRAIN_WAIT: Duration = Duration::from_secs(10);

/// Read what the client still sends, up to [`MAX_DRAIN`], after the answer
/// was written: it ends with the client's end of the request, or after a
/// while.
pub async fn drain(stream: &mut tokio::net::TcpStream) {
    let _ = stream.shutdown().await;
    let read_away = async {
        let mut chunk = [0u8; 8192];
        let mut read = 0;
        while read < MAX_DRAIN {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => read += n,
            }
        }
    };
    let _ = tokio::time::timeout(DRAIN_WAIT, read_away).await;
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        429 => "Too Many Requests",
        502 => "Bad Gateway",
        504 => "Gateway Timeout",
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
    inner: S,
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

impl<S: AsyncRead + Unpin> AsyncRead for Replay<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.at < self.first.len() {
            let n = out.remaining().min(self.first.len() - self.at);
            let at = self.at;
            out.put_slice(&self.first[at..at + n]);
            self.at += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, out)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Replay<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_core::runtime::block_on;

    #[test]
    fn a_request_is_read_head_then_body() {
        let raw = b"POST /api/v1/register?x=1 HTTP/1.1\r\nHost: h\r\nContent-Length: 13\r\nAuthorization: Bearer ens_1\r\n\r\n{\"name\":\"a\"}!";
        let mut stream = &raw[..];
        let (head, read, end) = block_on(read_head(&mut stream)).unwrap();
        assert_eq!(
            (head.method.as_str(), head.path.as_str()),
            ("POST", "/api/v1/register")
        );
        assert_eq!(head.bearer(), Some("ens_1"));
        assert!(!head.is_upgrade());
        let body = block_on(read_body(&mut stream, &head, &read[end..])).unwrap();
        assert_eq!(body, b"{\"name\":\"a\"}!");
    }

    #[test]
    fn the_address_a_client_reached_is_the_proxys_when_it_says_it() {
        let head = |headers: &[(&str, &str)]| Head {
            method: "POST".into(),
            path: "/api/v1/me/tokens".into(),
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
        };
        assert_eq!(
            head(&[("host", "127.0.0.1:7700")]).reached_at().as_deref(),
            Some("http://127.0.0.1:7700")
        );
        assert_eq!(
            head(&[
                ("host", "127.0.0.1:7700"),
                ("x-forwarded-host", "hub.example.com, inner"),
                ("x-forwarded-proto", "https"),
            ])
            .reached_at()
            .as_deref(),
            Some("https://hub.example.com")
        );
        assert_eq!(head(&[]).reached_at(), None);
        assert_eq!(head(&[("host", "a/b")]).reached_at(), None);
    }

    #[test]
    fn a_cookie_is_found_by_name_and_a_page_of_another_origin_is_told_apart() {
        let head = |headers: &[(&str, &str)]| Head {
            method: "POST".into(),
            path: "/api/v1/logout".into(),
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
        };
        let cookies = head(&[("cookie", "theme=dark; ensemble_session=ens_s1 ;x=y")]);
        assert_eq!(cookies.cookie("ensemble_session"), Some("ens_s1"));
        assert_eq!(cookies.cookie("session"), None);
        assert!(!cookies.is_reading());
        let host = ("host", "127.0.0.1:7700");
        assert!(!head(&[host]).is_from_elsewhere(), "no Origin: no page");
        assert!(!head(&[host, ("origin", "http://127.0.0.1:7700")]).is_from_elsewhere());
        assert!(head(&[host, ("origin", "https://evil.example")]).is_from_elsewhere());
        assert!(head(&[("origin", "http://127.0.0.1:7700")]).is_from_elsewhere());
    }

    #[test]
    fn a_body_too_large_or_a_head_that_is_not_http_is_refused() {
        let raw = format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        let mut stream = raw.as_bytes();
        let (head, read, end) = block_on(read_head(&mut stream)).unwrap();
        assert!(block_on(read_body(&mut stream, &head, &read[end..])).is_err());
        assert!(block_on(read_head(&mut &b"hello\r\n\r\n"[..])).is_err());
    }

    #[test]
    fn replay_gives_back_what_was_read_then_the_rest() {
        let mut r = Replay::new(b"ab".to_vec(), &b"cd"[..]);
        let mut all = String::new();
        block_on(r.read_to_string(&mut all)).unwrap();
        assert_eq!(all, "abcd");
    }
}
