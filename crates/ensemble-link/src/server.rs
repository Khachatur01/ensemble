//! `ensemble hub serve`: the hub as a server for a team. Each member's node
//! opens one WebSocket, signed in with its token, and asks for what its role
//! allows. Shares go into the organization vault under the signed-in user's
//! name, never a name the node sends.
//!
//! The same port serves the REST API under `/api/v1/` (`api`): a request that
//! asks for a WebSocket upgrade is the link, anything else is REST.
//!
//! The server speaks plain HTTP. For the internet, a TLS reverse proxy in front
//! (such as Caddy) turns `https://` and `wss://` into it; on a LAN or a VPN it
//! can be reached directly.

use crate::http::{self, Replay};
use crate::nodes::Routes;
use crate::protocol::{
    ANSWER_TAKEN, Event, HELLO, MODEL_REPLY, NODE, NODES, RECALL, RUN, RUN_CANCEL, RUN_DONE,
    RUN_PROGRESS, Reply, Request, SHARE, SUBSCRIBE, TASK_ACCEPT, TASK_REFUSE, TASK_UPDATE,
    TaskRelay,
};
use crate::users::Users;
use ensemble_config::Permission;
use ensemble_core::runtime::{self, blocking};
use ensemble_knowledge::Memory;
use futures_util::SinkExt;
use futures_util::stream::{SplitSink, StreamExt};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as Upgrade, Response,
};
use tokio_tungstenite::tungstenite::{self, Message};

/// How long a connection may take to send its request's head.
const HEAD_WAIT: Duration = Duration::from_secs(10);
/// How often a subscribed node's waiting events are looked for.
const LOOK_EVERY: Duration = Duration::from_secs(1);

/// The sending half of a node's link.
type Sender = SplitSink<WebSocketStream<http::Replay<TcpStream>>, Message>;

/// A hub server: its knowledge, its roles, how people join, and where its
/// users are kept.
pub struct Server<'a> {
    pub(crate) memory: &'a Memory,
    pub(crate) roles: &'a BTreeMap<String, Vec<Permission>>,
    /// Where it listens, how people join, how long a login lasts, and when
    /// failed logins lock a name.
    pub(crate) settings: ensemble_config::Server,
    /// The hub's state directory, with `users.json`.
    users: PathBuf,
    /// Held while the users file is read, changed and written, so two
    /// requests at once cannot lose a change.
    pub(crate) changing: Mutex<()>,
    /// Failed logins by name: how many, and until when the name is refused.
    pub(crate) failures: Mutex<HashMap<String, (u32, Option<Instant>)>>,
    /// The task inbox, when the hub has one: what subscribed nodes are told,
    /// and who hears what they say about their tasks.
    relay: Option<&'a dyn TaskRelay>,
    /// The connected nodes, and the runs and model calls between them.
    pub(crate) routes: Routes,
}

/// Who a connection signed in as.
struct Session {
    token: String,
    user: String,
}

impl<'a> Server<'a> {
    pub fn new(
        memory: &'a Memory,
        roles: &'a BTreeMap<String, Vec<Permission>>,
        settings: ensemble_config::Server,
        users: PathBuf,
    ) -> Self {
        Server {
            memory,
            roles,
            settings,
            users,
            changing: Mutex::new(()),
            failures: Mutex::new(HashMap::new()),
            relay: None,
            routes: Routes::default(),
        }
    }

    /// Carry the tasks of `relay` to subscribed nodes.
    pub fn with_relay(mut self, relay: &'a dyn TaskRelay) -> Self {
        self.relay = Some(relay);
        self
    }

    /// Accept nodes on `listener` until `stop` is set, each connection a task
    /// of its own. `say` gets one line per connection and per request, never
    /// what was shared. The server lives as long as the process, so it is
    /// borrowed for good.
    pub async fn run(
        &'static self,
        listener: std::net::TcpListener,
        stop: &'static AtomicBool,
        say: &'static (dyn Fn(&str) + Sync),
    ) where
        'a: 'static,
    {
        let listener = match listener
            .set_nonblocking(true)
            .and_then(|()| TcpListener::from_std(listener))
        {
            Ok(listener) => listener,
            Err(e) => return say(&format!("cannot wait for connections: {e}")),
        };
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                () = runtime::until_set(stop) => return,
            };
            match accepted {
                Ok((stream, peer)) => {
                    tokio::spawn(async move {
                        if let Err(e) = self.connection(stream, stop, say).await {
                            say(&format!("{peer}: {e}"));
                        }
                    });
                }
                Err(e) => say(&format!("accept: {e}")),
            }
        }
    }

    pub(crate) fn users(&self) -> Result<Users, String> {
        Users::load(&self.users).map_err(String::from)
    }

    async fn connection(
        &self,
        mut stream: TcpStream,
        stop: &AtomicBool,
        say: &(dyn Fn(&str) + Sync),
    ) -> Result<(), String> {
        let (head, read, end) = tokio::time::timeout(HEAD_WAIT, http::read_head(&mut stream))
            .await
            .map_err(|_| "no request came in time".to_string())??;
        if !head.is_upgrade() {
            if head.path.starts_with("/nodes/v1/") {
                return self.model_request(stream, &head, &read[end..], say).await;
            }
            return self.rest(stream, &head, &read[end..], say).await;
        }
        let mut session = None;
        // The handshake callback's error type is tungstenite's own response.
        #[allow(clippy::result_large_err)]
        let signed_in = |req: &Upgrade, resp: Response| match blocking(|| self.sign_in(req)) {
            Some(s) => {
                session = Some(s);
                Ok(resp)
            }
            None => Err(refused()),
        };
        let ws = tokio_tungstenite::accept_hdr_async_with_config(
            Replay::new(read, stream),
            signed_in,
            Some(crate::protocol::socket_config()),
        )
        .await
        .map_err(|e| format!("refused or broken handshake: {e}"))?;
        let session = session.ok_or("no session after the handshake")?;
        say(&format!("{} connected", session.user));
        let (conn, outbox) = self.routes.connect();
        let (mut sender, receiver) = ws.split();
        let ended = self
            .link(&mut sender, receiver, &session, conn, outbox, stop, say)
            .await;
        self.routes.disconnect(conn);
        say(&format!("{} disconnected", session.user));
        let _ = sender.close().await;
        ended
    }

    /// Serve one node's link until it closes or `stop` is set.
    #[allow(clippy::too_many_arguments)]
    async fn link(
        &self,
        sender: &mut Sender,
        mut receiver: futures_util::stream::SplitStream<WebSocketStream<Replay<TcpStream>>>,
        session: &Session,
        conn: u64,
        mut outbox: tokio::sync::mpsc::UnboundedReceiver<Event>,
        stop: &AtomicBool,
        say: &(dyn Fn(&str) + Sync),
    ) -> Result<(), String> {
        // Once the node subscribes, the events waiting for its account, each
        // sent once on this connection, looked for about once a second.
        let mut subscribed = false;
        let mut sent: HashSet<String> = HashSet::new();
        let mut look = tokio::time::interval(LOOK_EVERY);
        loop {
            let message = tokio::select! {
                () = runtime::until_set(stop) => return Ok(()),
                // What other connections have for this one: runs, their
                // progress, model calls.
                Some(event) = outbox.recv() => {
                    push(sender, &event).await?;
                    continue;
                }
                _ = look.tick(), if subscribed && self.relay.is_some() => {
                    let pending = blocking(|| {
                        self.permissions(session)
                            .map(|_| self.relay.map(|r| r.pending(&session.user)))
                    });
                    let Ok(pending) = pending else {
                        return Ok(());
                    };
                    for (id, event) in pending.unwrap_or_default() {
                        if sent.insert(id) {
                            push(sender, &event).await?;
                        }
                    }
                    continue;
                }
                message = receiver.next() => message,
            };
            let text = match message {
                None
                | Some(Err(
                    tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed,
                )) => return Ok(()),
                Some(Err(e)) => return Err(e.to_string()),
                Some(Ok(Message::Text(t))) => t.to_string(),
                Some(Ok(Message::Close(_))) => return Ok(()),
                Some(Ok(_)) => continue,
            };
            let reply = match serde_json::from_str::<Request>(&text) {
                Ok(req) => {
                    // A run's progress and a model's chunks would drown the rest.
                    if !matches!(req.method.as_str(), RUN_PROGRESS | MODEL_REPLY) {
                        say(&format!("{}: {}", session.user, req.method));
                    }
                    let subscribing = req.method == SUBSCRIBE;
                    let reply = blocking(|| self.answer(session, conn, req));
                    if subscribing && reply.error.is_none() {
                        subscribed = true;
                        // The first look comes at once.
                        look.reset_immediately();
                    }
                    reply
                }
                Err(e) => Reply::err(0, format!("not a request: {e}")),
            };
            send(sender, &reply).await?;
        }
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
        let user = self
            .users()
            .ok()?
            .authenticate(&token, self.roles)?
            .user
            .name;
        Some(Session { token, user })
    }

    /// What the token may do, its account's role narrowed by its scopes,
    /// checked again for every request, so a revoked or expired token stops
    /// working at once.
    fn permissions(&self, session: &Session) -> Result<Vec<Permission>, String> {
        let users = self.users()?;
        let signed = users
            .authenticate(&session.token, self.roles)
            .filter(|s| s.user.name == session.user)
            .ok_or("your token was revoked or expired; make a new one")?;
        Ok(signed.permissions)
    }

    fn answer(&self, session: &Session, conn: u64, req: Request) -> Reply {
        let permissions = match self.permissions(session) {
            Ok(p) => p,
            Err(e) => return Reply::err(req.id, e),
        };
        let may = |p: Permission| -> Result<(), String> {
            if permissions.contains(&p) {
                Ok(())
            } else {
                Err(format!(
                    "your role or this token's scopes do not allow `{}`",
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
            SUBSCRIBE => may(Permission::Tasks).and_then(|()| match self.relay {
                Some(_) => Ok(json!({ "subscribed": true })),
                None => Err("this hub has no task inbox".into()),
            }),
            TASK_ACCEPT | TASK_REFUSE | TASK_UPDATE | ANSWER_TAKEN => may(Permission::Tasks)
                .and_then(|()| match self.relay {
                    Some(relay) => relay.handle(&session.user, &req.method, &req.params),
                    None => Err("this hub has no task inbox".into()),
                }),
            NODE => self.routes.register(
                conn,
                &session.user,
                &req.params,
                permissions.contains(&Permission::Host),
            ),
            NODES => Ok(self
                .routes
                .listed(&session.user, permissions.contains(&Permission::Compute))),
            RUN => self.routes.start_run(
                conn,
                &session.user,
                permissions.contains(&Permission::Compute),
                &req.params,
            ),
            RUN_CANCEL => self.routes.cancel_run(conn, &req.params),
            RUN_PROGRESS | RUN_DONE => self.routes.pass_on(conn, &req.method, &req.params),
            MODEL_REPLY => self.routes.model_reply(conn, &req.params),
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

/// Tell the node something unasked.
async fn push(sender: &mut Sender, event: &Event) -> Result<(), String> {
    let json = serde_json::to_string(event).map_err(|e| e.to_string())?;
    sender
        .send(Message::text(json))
        .await
        .map_err(|e| e.to_string())
}

async fn send(sender: &mut Sender, reply: &Reply) -> Result<(), String> {
    let json = serde_json::to_string(reply).map_err(|e| e.to_string())?;
    sender
        .send(Message::text(json))
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ensemble_core::workspace::testing::TempDir;
    use std::sync::atomic::Ordering;
    use tungstenite::client::IntoClientRequest;

    /// The password of every account of the test hub.
    pub(crate) const PASSWORD: &str = "correct horse battery";

    /// A hub server on loopback, with `ana` a member, `rex` a reader and
    /// `root` an admin, each with a token and [`PASSWORD`]. It locks a name
    /// after 3 failed logins, and takes 2 sign-ups at a time.
    pub(crate) struct Running {
        pub(crate) tmp: TempDir,
        pub(crate) url: String,
        pub(crate) ana: String,
        pub(crate) rex: String,
        pub(crate) root: String,
        stop: &'static AtomicBool,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn start() -> Running {
        start_with(None)
    }

    /// The test hub, carrying the tasks of `relay` when given.
    pub(crate) fn start_with(relay: Option<&'static dyn TaskRelay>) -> Running {
        let tmp = TempDir::new();
        let yaml = format!(
            "version: 1\nowner: Acme\nllm_backends:\n  local: {{ type: openai-compatible-api, base_url: \"http://127.0.0.1:1/v1\", timeout_seconds: 5 }}\nknowledge:\n  vault: \"{}\"\n  distill:\n    try:\n      - {{ backend: local, model: small }}\n    when_none: wait\n    max_tokens: 100\n  recall: {{ max_notes: 3, max_chars: 1000 }}\nserver: {{ listen: \"127.0.0.1:0\", registration: {{ type: approval, max_pending: 2 }}, sessions: {{ hours: 1 }}, lockout: {{ failures: 3, minutes: 1 }} }}\nroles:\n  admin: [admin, recall, host]\n  member: [share, recall, tasks, compute]\n  reader: [recall]\n",
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
        let root = users.add("root", "admin", &hub.roles).unwrap();
        for name in ["ana", "rex", "root"] {
            users.set_password(name, PASSWORD).unwrap();
        }
        users.save().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/", listener.local_addr().unwrap());
        let stop: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let memory: &'static Memory = Box::leak(Box::new(memory));
        let hub: &'static ensemble_config::Hub = Box::leak(Box::new(hub));
        let mut server = Server::new(memory, &hub.roles, hub.server.unwrap(), tmp.0.join("state"));
        if let Some(relay) = relay {
            server = server.with_relay(relay);
        }
        let server: &'static Server<'static> = Box::leak(Box::new(server));
        runtime::shared().spawn(server.run(listener, stop, &|_| {}));
        Running {
            tmp,
            url,
            ana,
            rex,
            root,
            stop,
        }
    }

    type Client = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

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
        assert!(reply.error.unwrap().contains("do not allow `share`"));
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
