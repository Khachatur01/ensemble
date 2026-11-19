//! The runner's side of the link: one WebSocket to a hub server
//! ([`Connection`]); a link that opens one when it is first needed, and again
//! once when it breaks ([`Link`]); and signing in without a token, for one
//! ([`sign_in`]). Every link says the protocol version it speaks.

use crate::protocol::{Event, HELLO, Reply, Request, VERSION, VERSION_HEADER};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// How long one request may wait for its answer.
const TIMEOUT: Duration = Duration::from_secs(30);

/// A WebSocket to a hub.
pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Why a request got no result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The hub answered with this error.
    Hub(String),
    /// The link broke, or the hub said nothing in time.
    Broken(String),
}

impl Failure {
    pub fn message(self) -> String {
        match self {
            Failure::Hub(m) | Failure::Broken(m) => m,
        }
    }
}

/// One open WebSocket to a hub server, signed in with a token.
pub struct Connection {
    url: String,
    socket: Socket,
    next: u64,
    /// Events that arrived while a request waited for its reply.
    events: VecDeque<Event>,
}

impl Connection {
    /// A link signed in with `token`.
    pub async fn open(url: &str, token: &str) -> Result<Connection, String> {
        Connection::connect(url, Some(token)).await
    }

    /// A link with a token, or without one, which may only sign in.
    async fn connect(url: &str, token: Option<&str>) -> Result<Connection, String> {
        let url = crate::tokens::normal(url);
        let target = format!("{}{}", url.trim_end_matches('/'), crate::protocol::PATH);
        let socket = handshake(&url, &target, token).await?;
        Ok(Connection {
            url,
            socket,
            next: 1,
            events: VecDeque::new(),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Ask the hub and wait for its answer. Events that come meanwhile are
    /// kept for [`Connection::next_event`].
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, Failure> {
        let id = self.send(method, params).await.map_err(Failure::Broken)?;
        let answer = async {
            loop {
                match self.socket.next().await {
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(reply) = serde_json::from_str::<Reply>(&t)
                            && reply.id == id
                        {
                            return match (reply.result, reply.error) {
                                (_, Some(e)) => Err(Failure::Hub(e)),
                                (Some(v), None) => Ok(v),
                                (None, None) => Err(Failure::Hub(format!(
                                    "the hub answered `{method}` with nothing"
                                ))),
                            };
                        }
                        if let Ok(event) = serde_json::from_str::<Event>(&t) {
                            self.events.push_back(event);
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        return Err(Failure::Broken("the hub closed the link".into()));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(Failure::Broken(e.to_string())),
                }
            }
        };
        tokio::time::timeout(TIMEOUT, answer)
            .await
            .unwrap_or_else(|_| {
                Err(Failure::Broken(format!(
                    "the hub did not answer `{method}` within {}s",
                    TIMEOUT.as_secs()
                )))
            })
    }

    /// Ask the hub without waiting for its answer: what a runner says about a
    /// run while it goes on reading events. The answer is read, and dropped,
    /// with the events. Returns the request's id.
    pub async fn send(&mut self, method: &str, params: Value) -> Result<u64, String> {
        let id = self.next;
        self.next += 1;
        let req = Request {
            id,
            method: method.into(),
            params,
        };
        let json = serde_json::to_string(&req).map_err(|e| e.to_string())?;
        self.socket
            .send(Message::text(json))
            .await
            .map_err(|e| e.to_string())?;
        Ok(id)
    }

    /// The next event from the hub. An error means the link broke. Nothing
    /// is lost when this is dropped before it resolves, so it may race a
    /// timer or another source of work.
    pub async fn next_event(&mut self) -> Result<Event, String> {
        if let Some(event) = self.events.pop_front() {
            return Ok(event);
        }
        loop {
            match self.socket.next().await {
                Some(Ok(Message::Text(t))) => {
                    if let Ok(event) = serde_json::from_str::<Event>(&t) {
                        return Ok(event);
                    }
                }
                Some(Ok(Message::Close(_))) | None => return Err("the hub closed the link".into()),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.to_string()),
            }
        }
    }

    /// Close the link.
    pub async fn close(&mut self) {
        let _ = self.socket.close(None).await;
    }
}

/// A WebSocket to `target` on the hub `url`, saying the protocol version
/// and, when there is one, signed in with `token`.
async fn handshake(url: &str, target: &str, token: Option<&str>) -> Result<Socket, String> {
    let mut req = target
        .into_client_request()
        .map_err(|e| format!("{target}: {e}"))?;
    req.headers_mut()
        .insert(VERSION_HEADER, VERSION.to_string().parse().expect("digits"));
    if let Some(token) = token {
        let bearer = format!("Bearer {}", token.trim())
            .parse()
            .map_err(|_| "the token has characters a header cannot carry")?;
        req.headers_mut().insert("authorization", bearer);
    }
    let config = Some(crate::protocol::socket_config());
    let opened = tokio::time::timeout(
        TIMEOUT,
        tokio_tungstenite::connect_async_with_config(req, config, false),
    )
    .await
    .map_err(|_| format!("cannot reach the hub {url}: it did not answer in time"))?;
    let (socket, _) = opened.map_err(|e| match e {
        tungstenite::Error::Http(resp) if resp.status() == 401 => format!(
            "the hub {url} did not accept the token; `ensemble-runner login {url}` with a new one"
        ),
        tungstenite::Error::Http(resp) if resp.status() == 426 => {
            format!("the hub {url} refused the link: {}", outdated(&resp))
        }
        e => format!("cannot reach the hub {url}: {e}"),
    })?;
    Ok(socket)
}

/// A runner's relay socket to the hub `url`, signed in with `token`
/// (`crate::relay`).
pub async fn relay_socket(url: &str, token: &str) -> Result<Socket, String> {
    let url = crate::tokens::normal(url);
    let target = format!("{}{}", url.trim_end_matches('/'), crate::relay::PATH);
    handshake(&url, &target, Some(token)).await
}

/// Why a hub refused a runner of another version: what it said, or, when its
/// words did not come with the refusal, what the version it names means.
fn outdated(resp: &tungstenite::http::Response<Option<Vec<u8>>>) -> String {
    let said = resp
        .body()
        .as_deref()
        .map(String::from_utf8_lossy)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    said.unwrap_or_else(|| {
        let hub = resp
            .headers()
            .get(VERSION_HEADER)
            .and_then(|v| v.to_str().ok()?.trim().parse().ok());
        match hub {
            Some(hub) => crate::protocol::mismatch(hub, Some(VERSION)),
            None => format!("it does not speak link protocol {VERSION}"),
        }
    })
}

/// Sign in to the hub at `url` without a token: ask `method`, [`LOGIN`] or
/// [`JOIN`], with `params`, and return the access token the hub made for
/// this computer.
///
/// [`LOGIN`]: crate::protocol::LOGIN
/// [`JOIN`]: crate::protocol::JOIN
pub async fn sign_in(url: &str, method: &str, params: Value) -> Result<String, Failure> {
    let mut link = Connection::connect(url, None)
        .await
        .map_err(Failure::Broken)?;
    let answer = link.call(method, params).await;
    link.close().await;
    answer?["token"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| Failure::Hub(format!("the hub answered `{method}` with no token")))
}

/// Who the runner is on the hub, and what it may do there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub user: String,
    pub role: String,
    pub permissions: Vec<String>,
}

/// A hub token that may be replaced while it is used, as when `ensemble-runner
/// login` signs in again: every link given it connects again with the new one.
#[derive(Clone)]
pub struct Token(std::sync::Arc<tokio::sync::watch::Sender<String>>);

impl Token {
    pub fn new(token: &str) -> Self {
        Token(std::sync::Arc::new(tokio::sync::watch::Sender::new(
            token.trim().to_string(),
        )))
    }

    pub fn get(&self) -> String {
        self.0.borrow().clone()
    }

    /// Use `token` from now on; whether it is another one.
    pub fn set(&self, token: &str) -> bool {
        self.0.send_if_modified(|t| {
            let changed = t != token.trim();
            if changed {
                *t = token.trim().to_string();
            }
            changed
        })
    }

    /// Something that wakes when the token is replaced.
    pub fn watch(&self) -> tokio::sync::watch::Receiver<String> {
        self.0.subscribe()
    }
}

/// A hub server, asked one request at a time over one WebSocket, opened when
/// it is first needed, once more when it broke since, and again when its
/// token is replaced.
pub struct Link {
    url: String,
    token: Token,
    /// The open connection, and the token it was opened with.
    socket: tokio::sync::Mutex<Option<(Connection, String)>>,
}

impl Link {
    pub fn new(url: &str, token: &str) -> Self {
        Self::with_token(url, Token::new(token))
    }

    /// A link that follows `token` when it is replaced.
    pub fn with_token(url: &str, token: Token) -> Self {
        Link {
            url: crate::tokens::normal(url),
            token,
            socket: tokio::sync::Mutex::new(None),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Ask the hub and wait for its answer.
    pub async fn ask(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut socket = self.socket.lock().await;
        let token = self.token.get();
        if socket.as_ref().is_some_and(|(_, t)| *t != token)
            && let Some((mut old, _)) = socket.take()
        {
            old.close().await;
        }
        for attempt in 0..2 {
            if socket.is_none() {
                *socket = Some((Connection::open(&self.url, &token).await?, token.clone()));
            }
            let (link, _) = socket.as_mut().expect("just opened");
            match link.call(method, params.clone()).await {
                Ok(v) => return Ok(v),
                Err(Failure::Hub(e)) => return Err(e),
                Err(Failure::Broken(e)) => {
                    *socket = None;
                    if attempt == 1 {
                        return Err(format!("the link to {} broke: {e}", self.url));
                    }
                }
            }
        }
        unreachable!("the loop returns")
    }

    /// Who the token is on the hub, and what it may do there.
    pub async fn hello(&self) -> Result<Hello, String> {
        self.ask(HELLO, json!({})).await.map(|v| hello_from(&v))
    }

    /// Close the connection, if one is open; the next request opens another.
    pub async fn close(&self) {
        if let Some((mut link, _)) = self.socket.lock().await.take() {
            link.close().await;
        }
    }
}

/// A `hello` answer, read.
fn hello_from(v: &Value) -> Hello {
    let text = |k: &str| v[k].as_str().unwrap_or("").to_string();
    Hello {
        user: text("user"),
        role: text("role"),
        permissions: v["permissions"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in hub on loopback that answers the first request of each
    /// connection as `ana`, and then hangs up.
    async fn hub_that_hangs_up() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                    if let Some(Ok(Message::Text(text))) = socket.next().await {
                        let asked: Request = serde_json::from_str(&text).unwrap();
                        let hello = json!({"user": "ana", "role": "member", "permissions": []});
                        let reply = serde_json::to_string(&Reply::ok(asked.id, hello)).unwrap();
                        let _ = socket.send(Message::text(reply)).await;
                    }
                });
            }
        });
        url
    }

    /// A stand-in hub that refuses every link as one of version `hub` does,
    /// with its words when `say` is set, and says what version the runner
    /// sent.
    async fn hub_of_version(hub: u32, say: bool) -> (String, tokio::sync::mpsc::Receiver<String>) {
        use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (heard, hear) = tokio::sync::mpsc::channel(4);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let heard = heard.clone();
                // The handshake callback's error type is tungstenite's own response.
                #[allow(clippy::result_large_err)]
                let refuse = move |req: &Request, _: Response| -> Result<Response, ErrorResponse> {
                    let sent = req.headers()[VERSION_HEADER].to_str().unwrap().to_string();
                    let runner = sent.parse().ok();
                    heard.try_send(sent).unwrap();
                    let words = say.then(|| crate::protocol::mismatch(hub, runner));
                    let mut resp = ErrorResponse::new(words);
                    *resp.status_mut() = tungstenite::http::StatusCode::UPGRADE_REQUIRED;
                    resp.headers_mut()
                        .insert(VERSION_HEADER, hub.to_string().parse().unwrap());
                    Err(resp)
                };
                let _ = tokio_tungstenite::accept_hdr_async(stream, refuse).await;
            }
        });
        (url, hear)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hub_of_another_version_is_named_with_what_to_update() {
        let (url, mut heard) = hub_of_version(VERSION + 1, true).await;
        let err = Connection::open(&url, "ens_any").await.err().unwrap();
        assert_eq!(heard.recv().await.unwrap(), VERSION.to_string());
        assert!(
            err.contains(&format!(
                "this hub speaks link protocol {}, the runner {VERSION}: update ensemble-runner",
                VERSION + 1
            )),
            "{err}"
        );
        // A refusal without words: the runner says it from the version named.
        let (url, _heard) = hub_of_version(VERSION - 1, false).await;
        let err = sign_in(&url, crate::protocol::JOIN, json!({}))
            .await
            .unwrap_err()
            .message();
        assert!(err.contains("update ensemble-hub"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_link_is_opened_again_when_it_breaks() {
        let link = Link::new(&hub_that_hangs_up().await, "ens_any");
        assert_eq!(link.hello().await.unwrap().user, "ana");
        // The hub hung up after it answered: the next request opens a new one.
        assert_eq!(link.hello().await.unwrap().user, "ana");
    }
}
