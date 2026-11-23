//! `ensemble-ui`: the web UI of an Ensemble hub, on a port of its own. It
//! serves the app's files, and passes the app's calls to `/api/` on to the
//! hub's REST API, so the browser sees one origin and keeps the hub's session
//! cookie for it. The hub knows nothing of it; it is a client of the hub as
//! any other.
//!
//! A request to the API that changes anything must come from a page of this
//! origin, as the hub requires of its own: the browser names it. It is then
//! passed on as from the hub's own origin.

use clap::Parser;
use std::borrow::Cow;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

include!(concat!(env!("OUT_DIR"), "/files.rs"));

/// The web UI of an Ensemble hub: the app, and its calls passed on to the
/// hub.
#[derive(Parser, Debug)]
#[command(name = "ensemble-ui", version)]
struct Cli {
    /// The hub's address, as this computer reaches it: http://host:port.
    #[arg(long, value_name = "URL", default_value = "http://127.0.0.1:7700")]
    hub: String,
    /// The address the UI is served on: 0.0.0.0 for other computers too.
    #[arg(long, value_name = "ADDRESS", default_value = "127.0.0.1")]
    http_host: std::net::IpAddr,
    /// The port the UI is served on.
    #[arg(long, value_name = "PORT", default_value_t = 7701)]
    http_port: u16,
    /// Serve the app from this folder, such as `ui/dist` while it is worked
    /// on, in place of the app built into ensemble-ui.
    #[arg(long, value_name = "DIR")]
    dist: Option<PathBuf>,
}

/// The most a request's or a response's head may be.
const MAX_HEAD: usize = 16 * 1024;
/// How long a connection may take to send its request's head.
const HEAD_WAIT: Duration = Duration::from_secs(10);

/// What a page of the UI may load and reach: its own files and its origin,
/// nothing of another, and no framing by another site.
const POLICY: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; \
style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; \
frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

/// What `/` says when there is no app to serve.
const NO_APP: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<title>Ensemble</title></head><body><h1>Ensemble</h1>\
<p>ensemble-ui was built without the app: give it one with <code>--dist</code>.</p>\
</body></html>";

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let hub = match hub_authority(&cli.hub) {
        Ok(hub) => hub,
        Err(e) => {
            eprintln!("ensemble-ui: --hub: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    let files = Arc::new(match cli.dist {
        Some(dir) => Files::Folder(dir),
        None => Files::Embedded,
    });
    let listen = SocketAddr::new(cli.http_host, cli.http_port);
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    runtime.block_on(async move {
        let listener = match TcpListener::bind(listen).await {
            Ok(listener) => listener,
            Err(e) => {
                eprintln!("ensemble-ui: cannot listen on {listen}: {e}");
                return std::process::ExitCode::FAILURE;
            }
        };
        println!("ensemble-ui: serving http://{listen}, for the hub at http://{hub}");
        let hub: Arc<str> = hub.into();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let (files, hub) = (files.clone(), hub.clone());
                    tokio::spawn(async move {
                        if let Err(e) = connection(stream, &files, &hub).await {
                            eprintln!("ensemble-ui: {e}");
                        }
                    });
                }
                _ = stop() => return std::process::ExitCode::SUCCESS,
            }
        }
    })
}

/// When the process is asked to stop: Ctrl-C, or SIGTERM.
async fn stop() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("a SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

/// `url`'s host and port, when it is a hub's plain HTTP address.
fn hub_authority(url: &str) -> Result<String, String> {
    let rest = url
        .trim()
        .trim_end_matches('/')
        .strip_prefix("http://")
        .ok_or("give the hub's http:// address, such as http://127.0.0.1:7700")?;
    if rest.is_empty() || rest.contains(['/', '@', ' ']) {
        return Err(format!("'{url}' is not host:port"));
    }
    Ok(if rest.contains(':') {
        rest.to_string()
    } else {
        format!("{rest}:80")
    })
}

/// A request's first line and headers, as they came.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Head {
    /// Such as `POST /api/v1/session HTTP/1.1`.
    line: String,
    /// Each as it was sent, its name in any case.
    headers: Vec<(String, String)>,
}

impl Head {
    fn method(&self) -> &str {
        self.line.split(' ').next().unwrap_or("")
    }

    /// The path, without the query.
    fn path(&self) -> &str {
        let target = self.line.split(' ').nth(1).unwrap_or("/");
        target.split('?').next().unwrap_or("/")
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The origin the browser reached this server at: from a reverse proxy's
    /// `X-Forwarded-Proto` and `X-Forwarded-Host` when it sends them, else
    /// from `Host`.
    fn origin(&self) -> Option<String> {
        let first = |name: &str| Some(self.header(name)?.split(',').next()?.trim());
        let host = first("x-forwarded-host").or_else(|| first("host"))?;
        if host.is_empty() || host.contains(['/', ' ', '@']) {
            return None;
        }
        Some(format!("{}://{host}", self.scheme()))
    }

    /// `https` when a reverse proxy says the browser came over TLS.
    fn scheme(&self) -> &'static str {
        match self.header("x-forwarded-proto") {
            Some(p)
                if p.split(',')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("https") =>
            {
                "https"
            }
            _ => "http",
        }
    }
}

/// Read a head: it, and every byte read past it.
async fn read_head(stream: &mut (impl AsyncRead + Unpin)) -> Result<(Head, Vec<u8>), String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD {
            return Err("a head too large".into());
        }
        let n = stream.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("the connection closed before a head came".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let text = std::str::from_utf8(&buf[..end - 4]).map_err(|_| "a head that is not text")?;
    let mut lines = text.split("\r\n");
    let line = lines.next().unwrap_or("").to_string();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
        .collect();
    Ok((Head { line, headers }, buf[end..].to_vec()))
}

/// Answer one connection: a call to the API passed on to the hub, or a file
/// of the app.
async fn connection(mut stream: TcpStream, files: &Files, hub: &str) -> Result<(), String> {
    let (head, rest) = tokio::time::timeout(HEAD_WAIT, read_head(&mut stream))
        .await
        .map_err(|_| "no request came in time".to_string())??;
    if head.path() == "/api" || head.path().starts_with("/api/") {
        return pass_on(stream, &head, rest, hub).await;
    }
    if !matches!(head.method(), "GET" | "HEAD") {
        return respond(
            &mut stream,
            405,
            "text/plain; charset=utf-8",
            &[],
            b"GET only",
        )
        .await;
    }
    let (status, kind, bytes) = files.answer(head.path());
    let etag = tag(&bytes);
    let same = status == 200 && head.header("if-none-match") == Some(etag.as_str());
    let (status, bytes): (u16, &[u8]) = match (same, head.method()) {
        (true, _) => (304, &[]),
        (false, "HEAD") => (status, &[]),
        (false, _) => (status, &bytes),
    };
    let headers = [
        ("content-security-policy", POLICY),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("cache-control", "no-cache"),
        ("etag", etag.as_str()),
    ];
    respond(&mut stream, status, kind, &headers, bytes).await
}

/// The head that passes `head` on to the hub at `hub`, as from the hub's own
/// origin; refused when it would change anything and comes from a page of
/// another origin than this server's, or names none.
fn passed_on(head: &Head, hub: &str) -> Result<String, &'static str> {
    let changes = !matches!(head.method(), "GET" | "HEAD");
    let origin = head.header("origin");
    if changes && (origin.is_none() || origin != head.origin().as_deref()) {
        return Err("a request that changes anything must come from a page of this UI");
    }
    let mut out = format!("{}\r\nhost: {hub}\r\n", head.line);
    if origin.is_some() {
        out.push_str(&format!("origin: http://{hub}\r\n"));
    }
    for (name, value) in &head.headers {
        let skipped = [
            "host",
            "origin",
            "connection",
            "x-forwarded-host",
            "x-forwarded-proto",
            "x-forwarded-for",
        ];
        if !skipped.iter().any(|s| name.eq_ignore_ascii_case(s)) {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    out.push_str("connection: close\r\n\r\n");
    Ok(out)
}

/// The hub's response head, with its cookies Secure when the browser came
/// over TLS: the hub, reached over plain HTTP from here, does not know.
fn answered(head: &str, secure: bool) -> String {
    if !secure {
        return head.to_string();
    }
    head.split("\r\n")
        .map(|line| match line.split_once(':') {
            Some((name, value))
                if name.eq_ignore_ascii_case("set-cookie")
                    && !value.to_ascii_lowercase().contains("secure") =>
            {
                format!("{line}; Secure")
            }
            _ => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// Pass the call `head`, with the body begun in `rest`, on to the hub, and
/// its answer back.
async fn pass_on(
    mut client: TcpStream,
    head: &Head,
    rest: Vec<u8>,
    hub: &str,
) -> Result<(), String> {
    let outgoing = match passed_on(head, hub) {
        Ok(outgoing) => outgoing,
        Err(why) => return refuse(&mut client, 403, why).await,
    };
    let Ok(mut upstream) = TcpStream::connect(hub).await else {
        let why = format!("the hub at http://{hub} could not be reached");
        return refuse(&mut client, 502, &why).await;
    };
    upstream
        .write_all(outgoing.as_bytes())
        .await
        .and(upstream.write_all(&rest).await)
        .map_err(|e| e.to_string())?;
    let secure = head.scheme() == "https";
    let (mut from_client, mut to_client) = client.split();
    let (mut from_hub, mut to_hub) = upstream.split();
    // The rest of the body, while the hub answers; a client that closes its
    // side once it sent it still gets the answer.
    let body = async {
        let _ = tokio::io::copy(&mut from_client, &mut to_hub).await;
        std::future::pending::<()>().await
    };
    let answer = async {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            if buf.len() > MAX_HEAD {
                return Err("the hub's head is too large".to_string());
            }
            let n = from_hub.read(&mut chunk).await.map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("the hub closed the connection before it answered".to_string());
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = std::str::from_utf8(&buf[..end]).map_err(|_| "the hub's head is not text")?;
        to_client
            .write_all(answered(head, secure).as_bytes())
            .await
            .and(to_client.write_all(&buf[end..]).await)
            .map_err(|e| e.to_string())?;
        tokio::io::copy(&mut from_hub, &mut to_client)
            .await
            .map_err(|e| e.to_string())?;
        to_client.shutdown().await.map_err(|e| e.to_string())
    };
    tokio::select! {
        answered = answer => answered,
        () = body => Ok(()),
    }
}

/// Answer an API call this server does not pass on, with why, as the hub
/// says its errors.
async fn refuse(stream: &mut TcpStream, status: u16, why: &str) -> Result<(), String> {
    let body = format!(
        "{{\"error\":{:?},\"code\":{:?}}}",
        why,
        if status == 403 {
            "forbidden"
        } else {
            "unavailable"
        }
    );
    respond(stream, status, "application/json", &[], body.as_bytes()).await
}

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    headers: &[(&str, &str)],
    bytes: &[u8],
) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        304 => "Not Modified",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Bad Gateway",
    };
    let mut response = format!("HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\n");
    for (name, value) in headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n",
        bytes.len()
    ));
    stream
        .write_all(response.as_bytes())
        .await
        .and(stream.write_all(bytes).await)
        .and(stream.flush().await)
        .map_err(|e| e.to_string())
}

/// Where the app's files come from.
enum Files {
    /// Those embedded in the executable.
    Embedded,
    /// Those of a folder, read for every request.
    Folder(PathBuf),
}

impl Files {
    /// The file at `path`, under the app's root.
    fn file(&self, path: &str) -> Option<Cow<'static, [u8]>> {
        match self {
            Files::Embedded => FILES
                .iter()
                .find(|(name, _)| *name == path)
                .map(|(_, bytes)| Cow::Borrowed(*bytes)),
            Files::Folder(dir) => std::fs::read(dir.join(path)).ok().map(Cow::Owned),
        }
    }

    /// The status, type and bytes that answer `GET path`. A path that names
    /// no file of the app is one of its pages: it gets `index.html`.
    fn answer(&self, path: &str) -> (u16, &'static str, Cow<'static, [u8]>) {
        let missing = || (404, "text/plain", Cow::Borrowed(b"no such file".as_slice()));
        let Some(path) = relative(path) else {
            return missing();
        };
        if let Some(bytes) = self.file(&path) {
            return (200, content_type(&path), bytes);
        }
        if is_asset(&path) && path != "index.html" {
            return missing();
        }
        match self.file("index.html") {
            Some(bytes) => (200, content_type("index.html"), bytes),
            None => (
                200,
                content_type("index.html"),
                Cow::Borrowed(NO_APP.as_bytes()),
            ),
        }
    }
}

/// The entity tag of a file: a hash of its bytes, so a browser keeps it and
/// asks again, answered with 304 while it is the same.
fn tag(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let hex: String = digest.as_ref()[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("\"{hex}\"")
}

/// `path` under the app's root, `index.html` for `/`; none when it would
/// leave the root.
fn relative(path: &str) -> Option<String> {
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        return Some("index.html".into());
    }
    let only_names = Path::new(path)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    (only_names && !path.contains('\\')).then(|| path.to_string())
}

/// Does `path` name one of the app's files, by its extension, rather than one
/// of its pages, which may have a dot too, such as `/settings/backend.add`?
fn is_asset(path: &str) -> bool {
    let last = path.rsplit('/').next().unwrap_or("");
    last.rsplit_once('.').is_some_and(|(_, extension)| {
        matches!(
            extension,
            "html"
                | "js"
                | "mjs"
                | "wasm"
                | "css"
                | "json"
                | "map"
                | "svg"
                | "png"
                | "ico"
                | "woff2"
                | "txt"
        )
    })
}

/// The media type of the file at `path`, by its extension.
fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(line: &str, headers: &[(&str, &str)]) -> Head {
        Head {
            line: line.into(),
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn the_app_answers_its_files_its_pages_with_index_html_and_nothing_outside_it() {
        let dir = std::env::temp_dir().join(format!("ensemble-ui-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("dist/assets")).unwrap();
        std::fs::write(dir.join("dist/index.html"), "<html>app</html>").unwrap();
        std::fs::write(dir.join("dist/assets/app.wasm"), b"\0asm").unwrap();
        std::fs::write(dir.join("secret"), "no").unwrap();
        let files = Files::Folder(dir.join("dist"));
        let answer = |path: &str| {
            let (status, kind, bytes) = files.answer(path);
            (status, kind, String::from_utf8_lossy(&bytes).into_owned())
        };
        assert_eq!(answer("/").2, "<html>app</html>");
        assert_eq!(
            answer("/assets/app.wasm"),
            (200, "application/wasm", "\0asm".into())
        );
        assert_eq!(answer("/tasks/shop/7").2, "<html>app</html>", "a page");
        assert_eq!(
            answer("/settings/backend.add").2,
            "<html>app</html>",
            "a dot"
        );
        assert_eq!(answer("/assets/gone.js").0, 404, "a file it lacks");
        assert_eq!(answer("/../secret").0, 404);
        assert_eq!(answer("/assets/../../secret").0, 404);
        let (_, _, none) = Files::Folder(dir.join("none")).answer("/");
        assert!(String::from_utf8_lossy(&none).contains("without the app"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_call_is_passed_on_as_from_the_hubs_origin_only_from_a_page_of_this_ui() {
        let hub = "127.0.0.1:7700";
        let host = ("Host", "127.0.0.1:7701");
        let read = head(
            "GET /api/v1/me HTTP/1.1",
            &[host, ("Cookie", "ensemble_session=s")],
        );
        let out = passed_on(&read, hub).unwrap();
        assert!(
            out.starts_with("GET /api/v1/me HTTP/1.1\r\nhost: 127.0.0.1:7700\r\n"),
            "{out}"
        );
        assert!(
            out.contains("Cookie: ensemble_session=s\r\n") && !out.contains("7701"),
            "{out}"
        );
        assert!(out.ends_with("connection: close\r\n\r\n"));

        let change = |origin: Option<&str>| {
            let mut headers = vec![host, ("Content-Length", "2")];
            headers.extend(origin.map(|o| ("Origin", o)));
            passed_on(
                &head("POST /api/v1/ops/task.create HTTP/1.1", &headers),
                hub,
            )
        };
        let out = change(Some("http://127.0.0.1:7701")).unwrap();
        assert!(out.contains("origin: http://127.0.0.1:7700\r\n"), "{out}");
        assert!(change(Some("https://evil.example")).is_err());
        assert!(change(None).is_err(), "a change names its origin");

        let proxied = head(
            "DELETE /api/v1/session HTTP/1.1",
            &[
                host,
                ("X-Forwarded-Host", "ui.example.com"),
                ("X-Forwarded-Proto", "https"),
                ("Origin", "https://ui.example.com"),
            ],
        );
        let out = passed_on(&proxied, hub).unwrap();
        assert!(!out.contains("ui.example.com"), "{out}");
        assert_eq!(proxied.scheme(), "https");
    }

    #[test]
    fn the_hubs_cookies_are_secure_when_the_browser_came_over_tls() {
        let head = "HTTP/1.1 200 OK\r\nset-cookie: ensemble_session=s; Path=/; HttpOnly\r\ncontent-length: 2\r\n\r\n";
        assert_eq!(answered(head, false), head);
        let secure = answered(head, true);
        assert!(secure.contains("HttpOnly; Secure\r\n"), "{secure}");
        assert!(secure.ends_with("\r\n\r\n"), "{secure:?}");
    }

    #[test]
    fn the_hub_is_named_by_its_http_address() {
        assert_eq!(
            hub_authority("http://127.0.0.1:7700/").unwrap(),
            "127.0.0.1:7700"
        );
        assert_eq!(hub_authority("http://hub.lan").unwrap(), "hub.lan:80");
        assert!(hub_authority("https://hub.example.com").is_err());
        assert!(hub_authority("ws://127.0.0.1:7700").is_err());
    }
}
