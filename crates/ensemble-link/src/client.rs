//! The node's side of the link: one WebSocket to a hub server ([`Connection`]),
//! and a hub server's knowledge over it, used by an agent session as if the hub
//! were embedded ([`RemoteMemory`]), opened when it is first needed and opened
//! again once when it breaks.

use crate::protocol::{Event, HELLO, RECALL, Reply, Request, SHARE};
use async_trait::async_trait;
use ensemble_knowledge::{KnowledgeAccess, Tool};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Mutex;
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
                "the hub {url} did not accept the token; `ensemble login {url}` with a new one"
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

    /// Ask the hub without waiting for its answer: what a node says about a
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

/// Who the node is on the hub, and what it may do there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub user: String,
    pub role: String,
    pub permissions: Vec<String>,
    pub instructions: String,
}

/// A hub server's knowledge, over the link.
pub struct RemoteMemory {
    url: String,
    token: String,
    /// The working folder's name, never its path.
    project: Option<String>,
    client: Mutex<Option<String>>,
    socket: tokio::sync::Mutex<Option<Connection>>,
    hello: Mutex<Option<Hello>>,
}

impl RemoteMemory {
    pub fn new(url: &str, token: &str, project: Option<String>) -> Self {
        RemoteMemory {
            url: crate::tokens::normal(url),
            token: token.trim().to_string(),
            project,
            client: Mutex::new(None),
            socket: tokio::sync::Mutex::new(None),
            hello: Mutex::new(None),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Who the node is on the hub; asked once, then remembered.
    pub async fn hello(&self) -> Result<Hello, String> {
        if let Some(hello) = self.hello.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Ok(hello);
        }
        let v = self.ask(HELLO, json!({})).await?;
        let hello = hello_from(&v);
        *self.hello.lock().unwrap_or_else(|e| e.into_inner()) = Some(hello.clone());
        Ok(hello)
    }

    /// Ask the hub, opening the link when it is not open, and once more when
    /// it broke since.
    async fn ask(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut socket = self.socket.lock().await;
        for attempt in 0..2 {
            if socket.is_none() {
                *socket = Some(Connection::open(&self.url, &self.token).await?);
            }
            let link = socket.as_mut().expect("just opened");
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
}

/// A `hello` answer, read.
pub fn hello_from(v: &Value) -> Hello {
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
        instructions: text("instructions"),
    }
}

#[async_trait]
impl KnowledgeAccess for RemoteMemory {
    async fn tools(&self) -> Vec<Tool> {
        // A hub that cannot be asked yet gets every tool; the call says why
        // it fails.
        let permissions = self.hello().await.map(|h| h.permissions).ok();
        ensemble_knowledge::tool_definitions()
            .into_iter()
            .filter(|t| {
                permissions
                    .as_ref()
                    .is_none_or(|p| p.iter().any(|p| p == t.name))
            })
            .collect()
    }

    async fn instructions(&self) -> String {
        match self.hello().await {
            Ok(h) => h.instructions,
            Err(_) => ensemble_knowledge::instructions_for(&[SHARE, RECALL]),
        }
    }

    async fn call(&self, name: &str, args: &Value) -> Option<Result<String, String>> {
        let text = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        match name {
            SHARE => {
                let client = self
                    .client
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                Some(
                    self.ask(
                        SHARE,
                        json!({
                            "title": text("title"),
                            "text": text("text"),
                            "client": client,
                            "project": self.project,
                        }),
                    )
                    .await
                    .map(|v| {
                        format!(
                            "Shared as {} on the hub {}. It turns it into notes in the background.",
                            v["id"].as_str().unwrap_or("?"),
                            self.url
                        )
                    }),
                )
            }
            RECALL => Some(
                self.ask(RECALL, json!({ "query": text("query") }))
                    .await
                    .map(|v| v["text"].as_str().unwrap_or("").to_string()),
            ),
            _ => None,
        }
    }

    fn set_client(&self, name: &str) {
        let name = name.trim();
        if !name.is_empty() {
            *self.client.lock().unwrap_or_else(|e| e.into_inner()) = Some(name.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tests::start;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_members_session_shares_and_recalls_on_the_hub() {
        let hub = start();
        let m = RemoteMemory::new(&hub.url, &hub.ana, Some("shop".into()));
        assert_eq!(m.hello().await.unwrap().user, "ana");
        let names: Vec<&str> = m.tools().await.iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["share", "recall"]);
        m.set_client("claude-code");
        let said = m
            .call(
                "share",
                &json!({"title": "Ports", "text": "The shop listens on 8080."}),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(said.starts_with("Shared as "), "{said}");
        let shares = ensemble_knowledge::Vault::new(hub.tmp.0.join("vault")).shares();
        assert_eq!(shares.len(), 1);
        assert_eq!(
            (
                shares[0].shared_by.as_str(),
                shares[0].client.as_deref(),
                shares[0].project.as_deref()
            ),
            ("ana", Some("claude-code"), Some("shop"))
        );
        let notes = m
            .call("recall", &json!({"query": "deploy"}))
            .await
            .unwrap()
            .unwrap();
        assert!(notes.contains("Tuesdays"), "{notes}");
        assert!(m.call("delegate", &json!({})).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reader_is_offered_recall_only_and_a_bad_token_is_named() {
        let hub = start();
        let rex = RemoteMemory::new(&hub.url, &hub.rex, None);
        let names: Vec<&str> = rex.tools().await.iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["recall"]);
        assert!(!rex.instructions().await.contains("`share`"));

        let bad = RemoteMemory::new(&hub.url, "ens_wrong", None);
        let err = bad.hello().await.unwrap_err();
        assert!(err.contains("did not accept the token"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_link_is_opened_again_when_it_breaks() {
        let hub = start();
        let m = RemoteMemory::new(&hub.url, &hub.ana, None);
        m.hello().await.unwrap();
        // Break it from this side: the next request opens a new one.
        if let Some(link) = m.socket.lock().await.as_mut() {
            link.close().await;
        }
        let notes = m.call("recall", &json!({"query": "deploy"})).await.unwrap();
        assert!(notes.is_ok(), "{notes:?}");
    }
}

#[cfg(test)]
mod relay_tests {
    use super::*;
    use crate::protocol::{SUBSCRIBE, TASK_ACCEPT, TaskRelay};
    use crate::server::tests::start_with;

    /// Events for ana, and what nodes told it.
    #[derive(Default)]
    struct FakeRelay {
        events: Mutex<Vec<(String, Event)>>,
        heard: Mutex<Vec<(String, String, Value)>>,
    }

    impl TaskRelay for FakeRelay {
        fn pending(&self, user: &str) -> Vec<(String, Event)> {
            if user == "ana" {
                self.events.lock().unwrap().clone()
            } else {
                Vec::new()
            }
        }
        fn handle(&self, user: &str, method: &str, params: &Value) -> Result<Value, String> {
            self.heard
                .lock()
                .unwrap()
                .push((user.into(), method.into(), params.clone()));
            Ok(json!({ "ok": true }))
        }
    }

    fn event(id: &str) -> (String, Event) {
        (
            id.into(),
            Event {
                event: "task".into(),
                data: json!({ "delivery": id }),
            },
        )
    }

    /// The next event within `wait`, if one comes.
    async fn within(link: &mut Connection, wait: Duration) -> Option<Event> {
        tokio::time::timeout(wait, link.next_event())
            .await
            .ok()
            .map(Result::unwrap)
    }

    /// The deliveries of the events that come within `wait`.
    async fn deliveries(link: &mut Connection, wait: Duration) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(e) = within(link, wait).await {
            out.push(e.data["delivery"].as_str().unwrap().to_string());
        }
        out
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_subscribed_node_gets_each_event_once_and_again_after_reconnecting() {
        let relay: &'static FakeRelay = Box::leak(Box::default());
        relay.events.lock().unwrap().push(event("d1"));
        let hub = start_with(Some(relay));
        let wait = Duration::from_millis(1500);
        let mut link = Connection::open(&hub.url, &hub.ana).await.unwrap();
        assert!(
            within(&mut link, wait).await.is_none(),
            "nothing before subscribing"
        );
        link.call(SUBSCRIBE, json!({})).await.unwrap();
        assert_eq!(deliveries(&mut link, wait).await, vec!["d1"]);
        relay.events.lock().unwrap().push(event("d2"));
        assert_eq!(
            deliveries(&mut link, wait).await,
            vec!["d2"],
            "d1 is not sent twice on one connection"
        );
        link.call(TASK_ACCEPT, json!({"delivery": "d1", "task": "3"}))
            .await
            .unwrap();
        let heard = relay.heard.lock().unwrap().clone();
        assert_eq!(
            heard,
            vec![(
                "ana".to_string(),
                TASK_ACCEPT.to_string(),
                json!({"delivery": "d1", "task": "3"})
            )]
        );

        let mut again = Connection::open(&hub.url, &hub.ana).await.unwrap();
        again.call(SUBSCRIBE, json!({})).await.unwrap();
        let mut got = deliveries(&mut again, wait).await;
        got.sort();
        assert_eq!(got, vec!["d1", "d2"], "a new connection hears them again");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_token_without_the_tasks_permission_cannot_subscribe() {
        let relay: &'static FakeRelay = Box::leak(Box::default());
        let hub = start_with(Some(relay));
        let mut link = Connection::open(&hub.url, &hub.rex).await.unwrap();
        let err = link.call(SUBSCRIBE, json!({})).await.unwrap_err().message();
        assert!(err.contains("do not allow `tasks`"), "{err}");
        let err = link
            .call(TASK_ACCEPT, json!({"delivery": "d1", "task": "3"}))
            .await
            .unwrap_err()
            .message();
        assert!(err.contains("do not allow `tasks`"), "{err}");
        assert!(relay.heard.lock().unwrap().is_empty());
    }
}
