//! The runner's side of the link: one WebSocket to a hub server
//! ([`Connection`]); and a link that opens one when it is first needed, and
//! again once when it breaks ([`Link`]).

use crate::protocol::{Event, HELLO, Reply, Request};
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

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

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
    pub async fn open(url: &str, token: &str) -> Result<Connection, String> {
        let url = crate::tokens::normal(url);
        let mut req = url
            .as_str()
            .into_client_request()
            .map_err(|e| format!("{url}: {e}"))?;
        let bearer = format!("Bearer {}", token.trim())
            .parse()
            .map_err(|_| "the token has characters a header cannot carry")?;
        req.headers_mut().insert("authorization", bearer);
        let config = Some(crate::protocol::socket_config());
        let opened = tokio::time::timeout(
            TIMEOUT,
            tokio_tungstenite::connect_async_with_config(req, config, false),
        )
        .await
        .map_err(|_| format!("cannot reach the hub {url}: it did not answer in time"))?;
        let (socket, _) = opened.map_err(|e| match e {
            tungstenite::Error::Http(resp) if resp.status() == 401 => format!(
                "the hub {url} did not accept the token; `ensemble runner login {url}` with a new one"
            ),
            e => format!("cannot reach the hub {url}: {e}"),
        })?;
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

/// Who the runner is on the hub, and what it may do there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub user: String,
    pub role: String,
    pub permissions: Vec<String>,
}

/// A hub token that may be replaced while it is used, as when `ensemble runner
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
    use crate::server::testing::start;

    #[tokio::test(flavor = "multi_thread")]
    async fn hello_says_who_the_token_is_and_a_bad_token_is_named() {
        let hub = start();
        let hello = Link::new(&hub.url, &hub.ana).hello().await.unwrap();
        assert_eq!(
            (hello.user.as_str(), hello.role.as_str()),
            ("ana", "member")
        );
        assert!(
            hello.permissions.contains(&"share".to_string()),
            "{hello:?}"
        );
        let rex = Link::new(&hub.url, &hub.rex).hello().await.unwrap();
        assert_eq!(rex.permissions, vec!["recall"]);

        let err = Link::new(&hub.url, "ens_wrong").hello().await.unwrap_err();
        assert!(err.contains("did not accept the token"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_link_is_opened_again_when_it_breaks() {
        let hub = start();
        let link = Link::new(&hub.url, &hub.ana);
        link.hello().await.unwrap();
        // Break it from this side: the next request opens a new one.
        if let Some((open, _)) = link.socket.lock().await.as_mut() {
            open.close().await;
        }
        assert_eq!(link.hello().await.unwrap().user, "ana");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_replaced_token_is_used_from_the_next_request_on() {
        let hub = start();
        let token = Token::new("ens_wrong");
        let link = Link::with_token(&hub.url, token.clone());
        assert!(link.hello().await.is_err());
        assert!(token.set(&hub.rex));
        assert!(!token.set(&hub.rex), "the same token is no change");
        assert_eq!(link.hello().await.unwrap().user, "rex");
        token.set(&hub.ana);
        assert_eq!(link.hello().await.unwrap().user, "ana");
    }
}

#[cfg(test)]
mod task_tests {
    use super::*;
    use crate::protocol::{TASK_METHODS, TASKS, TaskService};
    use crate::server::testing::start_with;
    use std::sync::Mutex;

    /// What runners asked of the tasks.
    #[derive(Default)]
    struct FakeTasks {
        heard: Mutex<Vec<(String, String, Value)>>,
    }

    impl TaskService for FakeTasks {
        fn handle(&self, user: &str, method: &str, params: &Value) -> Result<Value, String> {
            self.heard
                .lock()
                .unwrap()
                .push((user.into(), method.into(), params.clone()));
            Ok(json!({ "tasks": [] }))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn task_requests_reach_the_service_as_the_signed_in_account() {
        let tasks: &'static FakeTasks = Box::leak(Box::default());
        let hub = start_with(Some(tasks));
        let link = Link::new(&hub.url, &hub.ana);
        let v = link
            .ask(TASKS, json!({"project": "shop", "owner": "mallory"}))
            .await
            .unwrap();
        assert_eq!(v, json!({ "tasks": [] }));
        let heard = tasks.heard.lock().unwrap().clone();
        assert_eq!(
            heard,
            vec![(
                "ana".to_string(),
                TASKS.to_string(),
                json!({"project": "shop", "owner": "mallory"})
            )],
            "the account is the token's, never one the runner sends"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_token_without_the_tasks_permission_reaches_no_task() {
        let tasks: &'static FakeTasks = Box::leak(Box::default());
        let hub = start_with(Some(tasks));
        let link = Link::new(&hub.url, &hub.rex);
        for method in TASK_METHODS {
            let err = link
                .ask(method, json!({"project": "shop"}))
                .await
                .unwrap_err();
            assert!(err.contains("do not allow `tasks`"), "{method}: {err}");
        }
        assert!(tasks.heard.lock().unwrap().is_empty());
    }
}
