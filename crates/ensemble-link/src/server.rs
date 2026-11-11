//! `ensemble hub serve`: the hub as a server for a team. Each member's node
//! opens one WebSocket, signed in with its token, and asks for what its role
//! allows. Shares go into the organization vault under the signed-in user's
//! name, never a name the node sends.
//!
//! The server speaks plain WebSocket. For the internet, a TLS reverse proxy in
//! front (such as Caddy) turns `wss://` into it; on a LAN or a VPN it can be
//! reached directly.

use crate::protocol::{HELLO, RECALL, Reply, Request, SHARE};
use crate::users::Users;
use ensemble_config::Permission;
use ensemble_knowledge::Memory;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tungstenite::handshake::server::{ErrorResponse, Request as Upgrade, Response};
use tungstenite::{Message, WebSocket};

/// How often a waiting connection looks at the stop flag.
const TICK: Duration = Duration::from_millis(200);

/// A hub server: its knowledge, its roles, and where its users are kept.
pub struct Server<'a> {
    pub memory: &'a Memory,
    pub roles: &'a BTreeMap<String, Vec<Permission>>,
    /// The hub's state directory, with `users.json`.
    pub users: PathBuf,
}

/// Who a connection signed in as.
struct Session {
    token: String,
    user: String,
}

impl Server<'_> {
    /// Accept nodes on `listener` until `stop` is set, each on its own thread.
    /// `say` gets one line per connection and per request, never what was
    /// shared.
    pub fn run(&self, listener: TcpListener, stop: &AtomicBool, say: &(dyn Fn(&str) + Sync)) {
        if listener.set_nonblocking(true).is_err() {
            say("cannot wait for connections without blocking");
            return;
        }
        std::thread::scope(|scope| {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        scope.spawn(move || {
                            if let Err(e) = self.connection(stream, stop, say) {
                                say(&format!("{peer}: {e}"));
                            }
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(TICK)
                    }
                    Err(e) => say(&format!("accept: {e}")),
                }
            }
        });
    }

    fn users(&self) -> Result<Users, String> {
        Users::load(&self.users).map_err(String::from)
    }

    // The handshake callback's error type is tungstenite's own response.
    #[allow(clippy::result_large_err)]
    fn connection(
        &self,
        stream: TcpStream,
        stop: &AtomicBool,
        say: &(dyn Fn(&str) + Sync),
    ) -> Result<(), String> {
        stream.set_nonblocking(false).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        let mut session = None;
        let mut ws = tungstenite::accept_hdr(stream, |req: &Upgrade, resp: Response| {
            match self.sign_in(req) {
                Some(s) => {
                    session = Some(s);
                    Ok(resp)
                }
                None => Err(refused()),
            }
        })
        .map_err(|e| format!("refused or broken handshake: {e}"))?;
        let session = session.ok_or("no session after the handshake")?;
        say(&format!("{} connected", session.user));
        ws.get_ref()
            .set_read_timeout(Some(TICK))
            .map_err(|e| e.to_string())?;
        while !stop.load(Ordering::Relaxed) {
            let message = match ws.read() {
                Ok(m) => m,
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    break;
                }
                Err(e) => return Err(e.to_string()),
            };
            let text = match message {
                Message::Text(t) => t.to_string(),
                Message::Close(_) => break,
                _ => continue,
            };
            let reply = match serde_json::from_str::<Request>(&text) {
                Ok(req) => {
                    say(&format!("{}: {}", session.user, req.method));
                    self.answer(&session, req)
                }
                Err(e) => Reply::err(0, format!("not a request: {e}")),
            };
            send(&mut ws, &reply)?;
        }
        say(&format!("{} disconnected", session.user));
        let _ = ws.close(None);
        Ok(())
    }

    /// The user an upgrade request's bearer token signs in as.
    fn sign_in(&self, req: &Upgrade) -> Option<Session> {
        let token = req
            .headers()
            .get("authorization")?
            .to_str()
            .ok()?
            .strip_prefix("Bearer ")?
            .trim()
            .to_string();
        let user = self.users().ok()?.authenticate(&token)?.name.clone();
        Some(Session { token, user })
    }

    /// What the user may do, checked again for every request, so a revoked
    /// token stops working at once.
    fn permissions(&self, session: &Session) -> Result<Vec<Permission>, String> {
        let users = self.users()?;
        let user = users
            .authenticate(&session.token)
            .filter(|u| u.name == session.user)
            .ok_or("your token was revoked; ask the hub's admin for a new one")?;
        Ok(self.roles.get(&user.role).cloned().unwrap_or_default())
    }

    fn answer(&self, session: &Session, req: Request) -> Reply {
        let permissions = match self.permissions(session) {
            Ok(p) => p,
            Err(e) => return Reply::err(req.id, e),
        };
        let may = |p: Permission| -> Result<(), String> {
            if permissions.contains(&p) {
                Ok(())
            } else {
                Err(format!(
                    "your role on this hub does not allow `{}`",
                    p.as_str()
                ))
            }
        };
        let text = |key: &str| -> Result<&str, String> {
            req.params
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| format!("{}: `{key}` is required", req.method))
        };
        let optional = |key: &str| req.params.get(key).and_then(Value::as_str);
        let result = match req.method.as_str() {
            HELLO => {
                let tools: Vec<&str> = permissions.iter().map(|p| p.as_str()).collect();
                let role = self
                    .users()
                    .ok()
                    .and_then(|u| u.get(&session.user).map(|u| u.role.clone()))
                    .unwrap_or_default();
                let offered: Vec<Value> = self
                    .memory
                    .tools()
                    .into_iter()
                    .filter(|t| tools.contains(&t.name))
                    .map(|t| json!({"name": t.name, "description": t.description, "parameters": t.parameters}))
                    .collect();
                Ok(json!({
                    "user": session.user,
                    "role": role,
                    "permissions": tools,
                    "tools": offered,
                    "instructions": ensemble_knowledge::instructions_for(&tools),
                }))
            }
            SHARE => may(Permission::Share).and_then(|()| {
                let share = self.memory.share_as(
                    &session.user,
                    optional("client"),
                    optional("project"),
                    text("title")?,
                    text("text")?,
                )?;
                Ok(json!({ "id": share.id }))
            }),
            RECALL => may(Permission::Recall)
                .and_then(|()| Ok(json!({ "text": self.memory.recall(text("query")?) }))),
            other => Err(format!("no method `{other}`")),
        };
        match result {
            Ok(value) => Reply::ok(req.id, value),
            Err(e) => Reply::err(req.id, e),
        }
    }
}

fn refused() -> ErrorResponse {
    let mut resp = ErrorResponse::new(Some(
        "a valid token is needed: Authorization: Bearer <token>".into(),
    ));
    *resp.status_mut() = tungstenite::http::StatusCode::UNAUTHORIZED;
    resp
}

fn send(ws: &mut WebSocket<TcpStream>, reply: &Reply) -> Result<(), String> {
    let json = serde_json::to_string(reply).map_err(|e| e.to_string())?;
    ws.send(Message::text(json)).map_err(|e| e.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ensemble_core::workspace::testing::TempDir;
    use tungstenite::client::IntoClientRequest;

    /// A hub server on loopback, with `ana` a member and `rex` a reader.
    pub(crate) struct Running {
        pub(crate) tmp: TempDir,
        pub(crate) url: String,
        pub(crate) ana: String,
        pub(crate) rex: String,
        stop: std::sync::Arc<AtomicBool>,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn start() -> Running {
        let tmp = TempDir::new();
        let yaml = format!(
            "version: 1\nowner: Acme\nllm_backends:\n  local: {{ type: openai-compatible-api, base_url: \"http://127.0.0.1:1/v1\", timeout_seconds: 5 }}\nknowledge:\n  vault: \"{}\"\n  distill:\n    try:\n      - {{ backend: local, model: small }}\n    when_none: wait\n    max_tokens: 100\n  recall: {{ max_notes: 3, max_chars: 1000 }}\nserver: {{ listen: \"127.0.0.1:0\", registration: {{ type: approval, max_pending: 2 }} }}\nroles:\n  member: [share, recall]\n  reader: [recall]\n",
            tmp.0.join("vault").display()
        );
        let hub =
            ensemble_config::parse_hub(&yaml, std::path::Path::new("hub.yaml"), &BTreeMap::new())
                .unwrap();
        let memory = Memory::new(&hub, None).unwrap();
        memory.vault().init().unwrap();
        memory
            .vault()
            .write_note(&ensemble_knowledge::Note {
                slug: "deploy-day".into(),
                kind: "decision".into(),
                title: "Deploy day".into(),
                tags: Vec::new(),
                scope: "personal".into(),
                sources: vec!["s".into()],
                shared_by: vec!["ana".into()],
                pipeline: 1,
                updated: "2026-11-11T00:00:00Z".into(),
                body: "Deploys go out on Tuesdays.".into(),
            })
            .unwrap();
        let mut users = Users::load(&tmp.0.join("state")).unwrap();
        let ana = users.add("ana", "member", &hub.roles).unwrap();
        let rex = users.add("rex", "reader", &hub.roles).unwrap();
        users.save().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/", listener.local_addr().unwrap());
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (state, flag) = (tmp.0.join("state"), stop.clone());
        std::thread::spawn(move || {
            let server = Server {
                memory: &memory,
                roles: &hub.roles,
                users: state,
            };
            server.run(listener, &flag, &|_| {});
        });
        Running {
            tmp,
            url,
            ana,
            rex,
            stop,
        }
    }

    type Client = WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>;

    fn connect(url: &str, token: Option<&str>) -> Result<Client, tungstenite::Error> {
        let mut req = url.into_client_request().unwrap();
        if let Some(token) = token {
            req.headers_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
        }
        tungstenite::connect(req).map(|(ws, _)| ws)
    }

    fn ask(ws: &mut Client, id: u64, method: &str, params: Value) -> Reply {
        let req = Request {
            id,
            method: method.into(),
            params,
        };
        ws.send(Message::text(serde_json::to_string(&req).unwrap()))
            .unwrap();
        loop {
            if let Message::Text(t) = ws.read().unwrap() {
                return serde_json::from_str(&t).unwrap();
            }
        }
    }

    #[test]
    fn a_node_without_a_valid_token_is_refused() {
        let hub = start();
        for token in [None, Some("ens_wrong")] {
            match connect(&hub.url, token) {
                Err(tungstenite::Error::Http(resp)) => assert_eq!(resp.status(), 401),
                other => panic!("{token:?}: {:?}", other.map(|_| ())),
            }
        }
    }

    #[test]
    fn a_member_shares_under_their_own_name_and_a_reader_only_recalls() {
        let hub = start();
        let mut ana = connect(&hub.url, Some(&hub.ana)).unwrap();
        let hello = ask(&mut ana, 1, HELLO, json!({})).result.unwrap();
        assert_eq!(
            (hello["user"].as_str(), hello["role"].as_str()),
            (Some("ana"), Some("member"))
        );
        assert_eq!(hello["tools"].as_array().unwrap().len(), 2);
        let reply = ask(
            &mut ana,
            2,
            SHARE,
            json!({"title": "Ports", "text": "The shop listens on 8080.", "client": "claude-code", "project": "shop", "shared_by": "mallory"}),
        );
        assert!(reply.error.is_none(), "{reply:?}");
        let shares = ensemble_knowledge::Vault::new(hub.tmp.0.join("vault")).shares();
        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].shared_by, "ana", "never a name the node sends");
        assert_eq!(shares[0].project.as_deref(), Some("shop"));

        let mut rex = connect(&hub.url, Some(&hub.rex)).unwrap();
        let hello = ask(&mut rex, 1, HELLO, json!({})).result.unwrap();
        let tools: Vec<&str> = hello["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(tools, vec!["recall"]);
        assert!(!hello["instructions"].as_str().unwrap().contains("`share`"));
        let reply = ask(&mut rex, 2, SHARE, json!({"title": "x", "text": "y"}));
        assert!(reply.error.unwrap().contains("does not allow `share`"));
        let reply = ask(&mut rex, 3, RECALL, json!({"query": "deploy"}));
        let text = reply.result.unwrap()["text"].as_str().unwrap().to_string();
        assert!(
            text.contains("reference data") && text.contains("Tuesdays"),
            "{text}"
        );
    }

    #[test]
    fn a_revoked_token_stops_working_on_an_open_connection() {
        let hub = start();
        let mut ana = connect(&hub.url, Some(&hub.ana)).unwrap();
        assert!(ask(&mut ana, 1, HELLO, json!({})).error.is_none());
        let mut users = Users::load(&hub.tmp.0.join("state")).unwrap();
        users.revoke("ana").unwrap();
        users.save().unwrap();
        let reply = ask(&mut ana, 2, RECALL, json!({"query": "deploy"}));
        assert!(reply.error.unwrap().contains("revoked"));
    }
}
