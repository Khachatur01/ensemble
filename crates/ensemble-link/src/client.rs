//! The node's side of the link: a hub server's knowledge, used by an agent
//! session as if the hub were embedded. One WebSocket, opened when it is first
//! needed and opened again once when it breaks.

use crate::protocol::{HELLO, RECALL, Reply, Request, SHARE};
use ensemble_knowledge::{KnowledgeAccess, Tool};
use serde_json::{Value, json};
use std::net::TcpStream;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// How long one request may wait for its answer.
const TIMEOUT: Duration = Duration::from_secs(30);

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

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
    socket: Mutex<Option<Socket>>,
    hello: Mutex<Option<Hello>>,
    next: AtomicU64,
}

impl RemoteMemory {
    pub fn new(url: &str, token: &str, project: Option<String>) -> Self {
        RemoteMemory {
            url: crate::tokens::normal(url),
            token: token.trim().to_string(),
            project,
            client: Mutex::new(None),
            socket: Mutex::new(None),
            hello: Mutex::new(None),
            next: AtomicU64::new(1),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Who the node is on the hub; asked once, then remembered.
    pub fn hello(&self) -> Result<Hello, String> {
        if let Some(hello) = self.hello.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Ok(hello);
        }
        let v = self.request(HELLO, json!({}))?;
        let text = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let hello = Hello {
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
        };
        *self.hello.lock().unwrap_or_else(|e| e.into_inner()) = Some(hello.clone());
        Ok(hello)
    }

    fn connect(&self) -> Result<Socket, String> {
        let mut req = self
            .url
            .as_str()
            .into_client_request()
            .map_err(|e| format!("{}: {e}", self.url))?;
        let bearer = format!("Bearer {}", self.token)
            .parse()
            .map_err(|_| "the token has characters a header cannot carry")?;
        req.headers_mut().insert("authorization", bearer);
        let (socket, _) = tungstenite::connect(req).map_err(|e| match e {
            tungstenite::Error::Http(resp) if resp.status() == 401 => format!(
                "the hub {} did not accept the token; `ensemble login {}` with a new one",
                self.url, self.url
            ),
            e => format!("cannot reach the hub {}: {e}", self.url),
        })?;
        let tcp = match socket.get_ref() {
            MaybeTlsStream::Plain(s) => s,
            MaybeTlsStream::Rustls(s) => s.get_ref(),
            _ => return Ok(socket),
        };
        tcp.set_read_timeout(Some(TIMEOUT))
            .map_err(|e| e.to_string())?;
        Ok(socket)
    }

    /// Ask the hub, opening the link when it is not open, and once more when
    /// it broke since.
    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut socket = self.socket.lock().unwrap_or_else(|e| e.into_inner());
        for attempt in 0..2 {
            if socket.is_none() {
                *socket = Some(self.connect()?);
            }
            let ws = socket.as_mut().expect("just opened");
            match exchange(
                ws,
                self.next.fetch_add(1, Ordering::Relaxed),
                method,
                &params,
            ) {
                Ok(reply) => {
                    return match (reply.result, reply.error) {
                        (_, Some(e)) => Err(e),
                        (Some(v), None) => Ok(v),
                        (None, None) => Err(format!("the hub answered `{method}` with nothing")),
                    };
                }
                Err(e) => {
                    *socket = None;
                    if attempt == 1 {
                        return Err(format!("the link to {} broke: {e}", self.url));
                    }
                }
            }
        }
        unreachable!("the loop returns")
    }

    fn may(&self, name: &str) -> bool {
        // A hub that cannot be asked yet gets every tool; the call says why
        // it fails.
        self.hello()
            .map(|h| h.permissions.iter().any(|p| p == name))
            .unwrap_or(true)
    }
}

/// Send one request and read its reply, skipping events and pings.
fn exchange(ws: &mut Socket, id: u64, method: &str, params: &Value) -> Result<Reply, String> {
    let req = Request {
        id,
        method: method.into(),
        params: params.clone(),
    };
    let json = serde_json::to_string(&req).map_err(|e| e.to_string())?;
    ws.send(Message::text(json)).map_err(|e| e.to_string())?;
    loop {
        match ws.read().map_err(|e| e.to_string())? {
            Message::Text(t) => {
                if let Ok(reply) = serde_json::from_str::<Reply>(&t)
                    && reply.id == id
                {
                    return Ok(reply);
                }
            }
            Message::Close(_) => return Err("the hub closed the link".into()),
            _ => {}
        }
    }
}

impl KnowledgeAccess for RemoteMemory {
    fn tools(&self) -> Vec<Tool> {
        ensemble_knowledge::tool_definitions()
            .into_iter()
            .filter(|t| self.may(t.name))
            .collect()
    }

    fn instructions(&self) -> String {
        match self.hello() {
            Ok(h) => h.instructions,
            Err(_) => ensemble_knowledge::instructions_for(&[SHARE, RECALL]),
        }
    }

    fn call(&self, name: &str, args: &Value) -> Option<Result<String, String>> {
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
                    self.request(
                        SHARE,
                        json!({
                            "title": text("title"),
                            "text": text("text"),
                            "client": client,
                            "project": self.project,
                        }),
                    )
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
                self.request(RECALL, json!({ "query": text("query") }))
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

    #[test]
    fn a_members_session_shares_and_recalls_on_the_hub() {
        let hub = start();
        let m = RemoteMemory::new(&hub.url, &hub.ana, Some("shop".into()));
        assert_eq!(m.hello().unwrap().user, "ana");
        let names: Vec<&str> = m.tools().iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["share", "recall"]);
        m.set_client("claude-code");
        let said = m
            .call(
                "share",
                &json!({"title": "Ports", "text": "The shop listens on 8080."}),
            )
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
            .unwrap()
            .unwrap();
        assert!(notes.contains("Tuesdays"), "{notes}");
        assert!(m.call("delegate", &json!({})).is_none());
    }

    #[test]
    fn a_reader_is_offered_recall_only_and_a_bad_token_is_named() {
        let hub = start();
        let rex = RemoteMemory::new(&hub.url, &hub.rex, None);
        let names: Vec<&str> = rex.tools().iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["recall"]);
        assert!(!rex.instructions().contains("`share`"));

        let bad = RemoteMemory::new(&hub.url, "ens_wrong", None);
        let err = bad.hello().unwrap_err();
        assert!(err.contains("did not accept the token"), "{err}");
    }

    #[test]
    fn the_link_is_opened_again_when_it_breaks() {
        let hub = start();
        let m = RemoteMemory::new(&hub.url, &hub.ana, None);
        m.hello().unwrap();
        // Break it from this side: the next request opens a new one.
        if let Some(ws) = m.socket.lock().unwrap().as_mut() {
            let _ = ws.close(None);
            let _ = ws.flush();
        }
        let notes = m.call("recall", &json!({"query": "deploy"})).unwrap();
        assert!(notes.is_ok(), "{notes:?}");
    }
}
