//! Where a process serves the backends it reaches through the hub: an
//! OpenAI-compatible endpoint on loopback, `http://127.0.0.1:<port>/<name>`
//! for each backend by its name, with a key made when it starts. The agents
//! and the distiller call it like any backend; each request becomes a call
//! that an [`Opener`] carries (the runner's relay socket, or the hub itself),
//! and the answer is written back as it comes.
//!
//! Only this process knows the key, and only loopback reaches the port.

use crate::relay::{Frame, Open, UNAVAILABLE};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedReceiver;

/// The most a request's head may be.
const MAX_HEAD: usize = 64 * 1024;
/// The most a request's body may be: a long prompt, and some room.
const MAX_BODY: usize = 16 * 1024 * 1024;

/// Who carries the calls of a [`Loopback`].
pub trait Opener: Send + Sync {
    /// Start a call: `open`, with the request's `body`. What comes back is
    /// the answer's frames, [`Frame::Head`], its [`Frame::Reply`] chunks, then
    /// [`Frame::Done`]; or [`Frame::Error`]. Dropping the receiver gives the
    /// call up.
    fn open(&self, open: Open, body: Vec<u8>) -> UnboundedReceiver<Frame>;
}

/// Each backend's name here, with the runner that serves it and its name
/// there.
pub type Names = BTreeMap<String, (String, String)>;

/// A loopback endpoint, served until it is dropped.
pub struct Loopback {
    /// `http://127.0.0.1:<port>`; a backend is at `<base_url>/<name>`.
    pub base_url: String,
    /// What a request must send as its API key.
    pub key: String,
    names: Arc<RwLock<Names>>,
    task: tokio::task::JoinHandle<()>,
}

impl Loopback {
    /// Serve `names` on a free loopback port, carrying each call with
    /// `opener`.
    pub async fn start(names: Names, opener: Arc<dyn Opener>) -> std::io::Result<Loopback> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let key = new_key()?;
        let names = Arc::new(RwLock::new(names));
        let serving = Serving {
            key: key.clone(),
            names: names.clone(),
            opener,
        };
        let serving = Arc::new(serving);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let serving = serving.clone();
                tokio::spawn(async move { serving.request(stream).await });
            }
        });
        Ok(Loopback {
            base_url,
            key,
            names,
            task,
        })
    }

    /// Serve `names` from now on, as a changed hub.yaml says.
    pub fn set_names(&self, names: Names) {
        *self.names.write().unwrap_or_else(|e| e.into_inner()) = names;
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A key no one guesses: 32 random bytes, as hex.
fn new_key() -> std::io::Result<String> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| std::io::Error::other("no randomness for the relay's key"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

struct Serving {
    key: String,
    names: Arc<RwLock<Names>>,
    opener: Arc<dyn Opener>,
}

/// A request as it came: its method, its path, and its body.
struct Request {
    method: String,
    path: String,
    bearer: Option<String>,
    body: Vec<u8>,
}

impl Serving {
    async fn request(&self, mut stream: TcpStream) {
        let request = match read_request(&mut stream).await {
            Ok(r) => r,
            Err((status, why)) => return refuse(&mut stream, status, &why).await,
        };
        if request.bearer.as_deref() != Some(self.key.as_str()) {
            return refuse(&mut stream, 401, "the relay's key is needed as the API key").await;
        }
        let path = request.path.trim_start_matches('/');
        let (name, rest) = path.split_once('/').unwrap_or((path, ""));
        let found = self
            .names
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned();
        let Some((runner, backend)) = found else {
            let known: Vec<String> = self
                .names
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .cloned()
                .collect();
            let why = format!(
                "no backend '{name}' is reached through the hub here; those that are: {}",
                if known.is_empty() {
                    "(none)".to_string()
                } else {
                    known.join(", ")
                }
            );
            return refuse(&mut stream, 404, &why).await;
        };
        let open = Open {
            runner,
            backend,
            method: request.method,
            path: format!("/{rest}"),
        };
        let mut answer = self.opener.open(open, request.body);
        let _ = write_answer(&mut stream, &mut answer).await;
    }
}

/// Write what comes for a call as it comes: the head first, then each
/// chunk, until it is done. An error before the head is the answer; one after
/// it cuts the answer off.
async fn write_answer(
    stream: &mut TcpStream,
    answer: &mut UnboundedReceiver<Frame>,
) -> std::io::Result<()> {
    let mut started = false;
    loop {
        let frame = answer.recv().await.unwrap_or_else(|| {
            Frame::error(0, UNAVAILABLE, "the relay went away before it answered")
        });
        match frame {
            Frame::Head {
                status,
                content_type,
                ..
            } if !started => {
                started = true;
                let head = format!(
                    "HTTP/1.1 {status} {}\r\ncontent-type: {content_type}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n",
                    if status < 400 { "OK" } else { "Error" }
                );
                stream.write_all(head.as_bytes()).await?;
            }
            Frame::Reply { bytes, .. } if started => stream.write_all(&bytes).await?,
            Frame::Error {
                status, message, ..
            } if !started => {
                refuse(stream, status, &message).await;
                return Ok(());
            }
            Frame::Done { .. } | Frame::Error { .. } => break,
            _ => {}
        }
    }
    stream.flush().await?;
    stream.shutdown().await
}

/// Answer with `status` and why, as an OpenAI-compatible error, and close.
async fn refuse(stream: &mut TcpStream, status: u16, why: &str) {
    let body = json!({"error": {"message": why, "type": "ensemble_relay"}}).to_string();
    let head = format!(
        "HTTP/1.1 {status} Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Read one request, or the status and why it cannot be served.
async fn read_request(stream: &mut TcpStream) -> Result<Request, (u16, String)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD {
            return Err((431, "the request's head is too long".into()));
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| (400, e.to_string()))?;
        if n == 0 {
            return Err((400, "the request ended before its head did".into()));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).to_string();
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let method = first.next().unwrap_or("").to_string();
    let path = first.next().unwrap_or("/").to_string();
    let (mut length, mut bearer) = (0usize, None);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim().to_ascii_lowercase(), value.trim());
        match name.as_str() {
            "content-length" => {
                length = value
                    .parse()
                    .map_err(|_| (400, format!("content-length '{value}' is no number")))?;
            }
            "authorization" => {
                bearer = value
                    .strip_prefix("Bearer ")
                    .or_else(|| value.strip_prefix("bearer "))
                    .map(|t| t.trim().to_string());
            }
            "transfer-encoding" if !value.eq_ignore_ascii_case("identity") => {
                return Err((411, "send the body with a content-length".into()));
            }
            _ => {}
        }
    }
    if length > MAX_BODY {
        return Err((413, format!("the body is over {MAX_BODY} bytes")));
    }
    let mut body = buf[end..].to_vec();
    while body.len() < length {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| (400, e.to_string()))?;
        if n == 0 {
            return Err((400, "the request ended before its body did".into()));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    Ok(Request {
        method,
        path,
        bearer,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::sync::mpsc::unbounded_channel;

    /// An opener that answers every call in two chunks, and keeps what it was
    /// asked; a call to the runner `busy/box` it answers busy.
    #[derive(Default)]
    struct Echo(Mutex<Vec<(Open, Vec<u8>)>>);

    impl Opener for Echo {
        fn open(&self, open: Open, body: Vec<u8>) -> UnboundedReceiver<Frame> {
            let (tx, rx) = unbounded_channel();
            if open.runner == "busy/box" {
                let _ = tx.send(Frame::error(0, UNAVAILABLE, "busy"));
            } else {
                for frame in [
                    Frame::Head {
                        call: 0,
                        status: 200,
                        content_type: "text/event-stream".into(),
                    },
                    Frame::Reply {
                        call: 0,
                        bytes: b"data: one\n\n".to_vec(),
                    },
                    Frame::Reply {
                        call: 0,
                        bytes: b"data: [DONE]\n\n".to_vec(),
                    },
                    Frame::Done { call: 0 },
                ] {
                    let _ = tx.send(frame);
                }
            }
            self.0.lock().unwrap().push((open, body));
            rx
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_to_a_backend_by_its_name_is_a_call_and_its_answer_streams_back() {
        let echo = Arc::new(Echo::default());
        let names = Names::from([
            (
                "gpu".to_string(),
                ("ana/gpu-box".to_string(), "local".to_string()),
            ),
            (
                "full".to_string(),
                ("busy/box".to_string(), "local".to_string()),
            ),
        ]);
        let lo = Loopback::start(names, echo.clone()).await.unwrap();
        let (url, key) = (lo.base_url.clone(), lo.key.clone());
        let ask = move |path: &str, key: &str| {
            let (url, key, path) = (url.clone(), key.to_string(), path.to_string());
            std::thread::spawn(move || {
                let mut answer = ureq::post(format!("{url}{path}"))
                    .header("authorization", format!("Bearer {key}"))
                    .config()
                    .http_status_as_error(false)
                    .build()
                    .send("{\"model\": \"qwen\"}")
                    .unwrap();
                let kind = answer
                    .headers()
                    .get("content-type")
                    .map(|v| v.to_str().unwrap().to_string());
                (
                    answer.status().as_u16(),
                    kind,
                    answer.body_mut().read_to_string().unwrap(),
                )
            })
            .join()
            .unwrap()
        };
        let (status, kind, body) = ask("/gpu/chat/completions", &key);
        assert_eq!(status, 200);
        assert_eq!(kind.as_deref(), Some("text/event-stream"));
        assert_eq!(body, "data: one\n\ndata: [DONE]\n\n");
        let (open, sent) = echo.0.lock().unwrap()[0].clone();
        assert_eq!(
            open,
            Open {
                runner: "ana/gpu-box".into(),
                backend: "local".into(),
                method: "POST".into(),
                path: "/chat/completions".into(),
            }
        );
        assert_eq!(sent, b"{\"model\": \"qwen\"}");

        let (status, _, body) = ask("/full/chat/completions", &key);
        assert_eq!(status, 503);
        assert!(body.contains("busy"), "{body}");
        let (status, _, body) = ask("/nowhere/chat/completions", &key);
        assert_eq!(status, 404);
        assert!(body.contains("full, gpu"), "{body}");
        let (status, _, _) = ask("/gpu/chat/completions", "wrong");
        assert_eq!(status, 401);
        lo.set_names(Names::new());
        assert_eq!(ask("/gpu/chat/completions", &key).0, 404);
    }
}
