//! `ensemble-hub serve`: the hub as a server for a team. Each member's runner
//! opens a WebSocket, saying the link protocol it speaks and signed in with
//! its token, and asks for what its role allows; a runner without a token
//! may only sign in, with a password or a join code, for one. The hub's MCP
//! asks the same way for a client agent. Shares go into the organization
//! vault under the signed-in user's name, never a name the client sends.
//!
//! The same port serves the REST API under `/api/v1/` (`api`): a request that
//! asks for a WebSocket upgrade is the link, anything else is REST.
//!
//! The server speaks plain HTTP. For the internet, a TLS reverse proxy in front
//! (such as Caddy) turns `https://` and `wss://` into it; on a LAN or a VPN it
//! can be reached directly.

use crate::accounts::HubContext;
use crate::board::{Asker, TaskService};
use crate::http::{self, Replay};
use crate::runners::Routes;
use crate::users::Users;
use ensemble_config::Permission;
use ensemble_core::runtime::{self, blocking};
use ensemble_knowledge::Memory;
use ensemble_link::protocol::{
    Event, HELLO, JOIN, LOGIN, RECALL, RUNNER, RUNNERS, Reply, Request, SHARE, TASK_METHODS,
    VERSION, VERSION_HEADER,
};
use ensemble_ops::Caller;
use futures_util::SinkExt;
use futures_util::stream::{SplitSink, StreamExt};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as Upgrade, Response,
};
use tokio_tungstenite::tungstenite::{self, Message};

/// How long a connection may take to send its request's head.
const HEAD_WAIT: Duration = Duration::from_secs(10);

/// The sending half of a runner's link.
type Sender = SplitSink<WebSocketStream<http::Replay<TcpStream>>, Message>;

/// A hub server: its knowledge, its roles, how people join, and where its
/// users are kept.
pub struct Server<'a> {
    pub(crate) memory: &'a Memory,
    /// Its accounts and settings, as its operations reach them.
    pub(crate) hub: HubContext,
    /// The hub's operations, which its REST API runs.
    pub(crate) operations: ensemble_ops::Registry<HubContext>,
    /// The tasks it keeps for its accounts.
    pub(crate) tasks: Option<&'a dyn TaskService>,
    /// Whether it serves MCP at `/mcp`.
    mcp: bool,
    /// The connected runners.
    pub(crate) routes: Routes,
}

/// Who a connection signed in as.
pub(crate) struct Session {
    pub(crate) token: String,
    pub(crate) user: String,
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
            hub: {
                let roles = roles.clone();
                HubContext::new(&users, move || Ok((roles.clone(), settings)))
            },
            operations: crate::accounts::operations(),
            tasks: None,
            mcp: false,
            routes: Routes::default(),
        }
    }

    /// Keep its accounts' tasks in `tasks`.
    pub fn with_tasks(mut self, tasks: &'a dyn TaskService) -> Self {
        self.tasks = Some(tasks);
        self
    }

    /// Serve MCP to client agents at `/mcp`.
    pub fn with_mcp(mut self) -> Self {
        self.mcp = true;
        self
    }

    /// Accept runners on `listener` until `stop` is set, each connection a task
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

    /// Its operations, and what they are given: what its REST API runs, and
    /// the one who runs it, on its socket.
    pub fn operations(&self) -> (&ensemble_ops::Registry<HubContext>, &HubContext) {
        (&self.operations, &self.hub)
    }

    /// Its roles, as hub.yaml says them now.
    pub(crate) fn roles(&self) -> BTreeMap<String, Vec<Permission>> {
        self.hub.accounts().map(|a| a.roles()).unwrap_or_default()
    }

    pub(crate) fn users(&self) -> Result<Users, String> {
        let accounts = self.hub.accounts().map_err(|e| e.to_string())?;
        accounts.users().map_err(|e| e.to_string())
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
            if self.mcp && head.path == crate::mcp::PATH {
                return self.mcp(stream, &head, &read[end..], say).await;
            }
            return self.rest(stream, &head, &read[end..], say).await;
        }
        let mut session = None;
        // The handshake callback's error type is tungstenite's own response.
        #[allow(clippy::result_large_err)]
        let signed_in = |req: &Upgrade, resp: Response| {
            // A runner of another version would be misread: refused first.
            let version = req
                .headers()
                .get(VERSION_HEADER)
                .and_then(|v| v.to_str().ok()?.trim().parse::<u32>().ok());
            if version != Some(VERSION) {
                return Err(outdated(version));
            }
            match blocking(|| self.sign_in(req)) {
                Ok(s) => {
                    session = s;
                    Ok(resp)
                }
                Err(()) => Err(refused()),
            }
        };
        let ws = tokio_tungstenite::accept_hdr_async_with_config(
            Replay::new(read, stream),
            signed_in,
            Some(ensemble_link::protocol::socket_config()),
        )
        .await
        .map_err(|e| format!("refused or broken handshake: {e}"))?;
        let who = session
            .as_ref()
            .map_or("a runner signing in", |s| s.user.as_str())
            .to_string();
        say(&format!("{who} connected"));
        let (conn, outbox) = self.routes.connect();
        let (mut sender, receiver) = ws.split();
        let ended = self
            .link(
                &mut sender,
                receiver,
                session.as_ref(),
                conn,
                outbox,
                stop,
                say,
            )
            .await;
        self.routes.disconnect(conn);
        say(&format!("{who} disconnected"));
        let _ = sender.close().await;
        ended
    }

    /// Serve one runner's link until it closes or `stop` is set: as the
    /// account it signed in as, or, without a token, for signing in alone.
    #[allow(clippy::too_many_arguments)]
    async fn link(
        &self,
        sender: &mut Sender,
        mut receiver: futures_util::stream::SplitStream<WebSocketStream<Replay<TcpStream>>>,
        session: Option<&Session>,
        conn: u64,
        mut outbox: tokio::sync::mpsc::UnboundedReceiver<Event>,
        stop: &AtomicBool,
        say: &(dyn Fn(&str) + Sync),
    ) -> Result<(), String> {
        loop {
            let message = tokio::select! {
                () = runtime::until_set(stop) => return Ok(()),
                // What other connections have for this one.
                Some(event) = outbox.recv() => {
                    push(sender, &event).await?;
                    continue;
                }
                message = receiver.next() => message,
            };
            let text = match message {
                // A command that asked once and exited goes without a goodbye.
                None
                | Some(Err(
                    tungstenite::Error::ConnectionClosed
                    | tungstenite::Error::AlreadyClosed
                    | tungstenite::Error::Protocol(
                        tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
                    ),
                )) => return Ok(()),
                Some(Err(e)) => return Err(e.to_string()),
                Some(Ok(Message::Text(t))) => t.to_string(),
                Some(Ok(Message::Close(_))) => return Ok(()),
                Some(Ok(_)) => continue,
            };
            let reply = match serde_json::from_str::<Request>(&text) {
                Ok(req) => {
                    // The tasks a runner looks at every few seconds would
                    // drown the rest.
                    if !TASK_METHODS.contains(&req.method.as_str()) {
                        let who = session.map_or("a runner signing in", |s| s.user.as_str());
                        say(&format!("{who}: {}", req.method));
                    }
                    blocking(|| match session {
                        Some(session) => self.answer(session, conn, req),
                        None => self.unsigned(req),
                    })
                }
                Err(e) => Reply::err(0, format!("not a request: {e}")),
            };
            send(sender, &reply).await?;
        }
    }

    /// The user an upgrade request's bearer token signs in as; none without
    /// a token, and refused with one that is not valid.
    fn sign_in(&self, req: &Upgrade) -> Result<Option<Session>, ()> {
        let Some(header) = req.headers().get("authorization") else {
            return Ok(None);
        };
        let token = header
            .to_str()
            .ok()
            .and_then(|h| h.strip_prefix("Bearer "))
            .ok_or(())?;
        self.session(token.trim().to_string()).map(Some).ok_or(())
    }

    /// What a connection without a token may ask: to sign in, with a password
    /// or a join code, for an access token of the runner's own. Nothing else.
    fn unsigned(&self, req: Request) -> Reply {
        let operation = match req.method.as_str() {
            LOGIN => "runner.login",
            JOIN => "runner.join",
            other => {
                return Reply::err(
                    req.id,
                    format!(
                        "this link is not signed in, so it may ask only `{LOGIN}` or `{JOIN}`, not `{other}`; connect with a token"
                    ),
                );
            }
        };
        match self
            .operations
            .run(&self.hub, &Caller::Anyone, operation, req.params)
        {
            Ok(value) => Reply::ok(req.id, value),
            Err(e) => Reply::err(req.id, e.to_string()),
        }
    }

    /// The user `token` signs in as, when it is valid.
    pub(crate) fn session(&self, token: String) -> Option<Session> {
        let user = self
            .users()
            .ok()?
            .authenticate(&token, &self.roles())?
            .user
            .name;
        Some(Session { token, user })
    }

    /// What the token may do, its account's role narrowed by its scopes,
    /// checked again for every request, so a revoked or expired token stops
    /// working at once.
    pub(crate) fn permissions(&self, session: &Session) -> Result<Vec<Permission>, String> {
        let users = self.users()?;
        let signed = users
            .authenticate(&session.token, &self.roles())
            .filter(|s| s.user.name == session.user)
            .ok_or("your token was revoked or expired; make a new one")?;
        Ok(signed.permissions)
    }

    pub(crate) fn answer(&self, session: &Session, conn: u64, req: Request) -> Reply {
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
                    ensemble_knowledge::visibility_of(&req.params)?,
                    text("title")?,
                    text("text")?,
                )?;
                Ok(json!({ "id": share.id, "visibility": share.visibility.as_str() }))
            }),
            RECALL => may(Permission::Recall).and_then(|()| {
                Ok(json!({ "text": self.memory.recall_as(&session.user, text("query")?) }))
            }),
            method if TASK_METHODS.contains(&method) => {
                may(Permission::Tasks).and_then(|()| match self.tasks {
                    Some(tasks) => self.ask_tasks(
                        tasks,
                        session,
                        permissions.contains(&Permission::Host),
                        method,
                        &req.params,
                    ),
                    None => Err("this hub keeps no tasks".into()),
                })
            }
            RUNNER => self.routes.register(
                conn,
                &session.user,
                &req.params,
                permissions.contains(&Permission::Host),
            ),
            RUNNERS => Ok(self
                .routes
                .listed(&session.user, permissions.contains(&Permission::Compute))),
            other => Err(format!("no method `{other}`")),
        };
        match result {
            Ok(value) => Reply::ok(req.id, value),
            Err(e) => Reply::err(req.id, e),
        }
    }
}

impl Server<'_> {
    /// Pass a task method to `tasks`, asked by `session`'s account, with who
    /// is connected now: the runners that may take each account's tasks, and
    /// the runner the request names (`runner`, by its name or as
    /// `<account>/<name>`) as a lender, when it works for the hub and the
    /// token `may_host`. Whether an account's role has `compute` is read
    /// from the accounts as they are now.
    fn ask_tasks(
        &self,
        tasks: &dyn TaskService,
        session: &Session,
        may_host: bool,
        method: &str,
        params: &Value,
    ) -> Result<Value, String> {
        // The accounts, read once and only when a runner of another works
        // for the hub.
        let accounts = std::sync::OnceLock::new();
        let compute = |account: &str| {
            let read = accounts.get_or_init(|| self.users().ok().map(|u| (u, self.roles())));
            read.as_ref().is_some_and(|(users, roles)| {
                users.get(account).is_some_and(|u| {
                    u.status == crate::users::Status::Active
                        && roles
                            .get(&u.role)
                            .is_some_and(|p| p.contains(&Permission::Compute))
                })
            })
        };
        let runners = |account: &str| self.routes.present(account, || compute(account));
        let user = session.user.as_str();
        let lender = params
            .get("runner")
            .and_then(Value::as_str)
            .map(|r| {
                let r = r.trim();
                r.strip_prefix(user)
                    .and_then(|rest| rest.strip_prefix('/'))
                    .unwrap_or(r)
            })
            .filter(|_| may_host)
            .and_then(|name| self.routes.lender(user, name));
        let asker = Asker {
            user,
            lender,
            runners: &runners,
            compute: &compute,
        };
        tasks.handle(&asker, method, params)
    }
}

fn refused() -> ErrorResponse {
    let mut resp = ErrorResponse::new(Some(
        "the token is not valid: Authorization: Bearer <token>, or none to sign in".into(),
    ));
    *resp.status_mut() = tungstenite::http::StatusCode::UNAUTHORIZED;
    resp
}

/// The refusal of a runner that speaks `version` of the link, or named none:
/// both versions, and what to update.
fn outdated(version: Option<u32>) -> ErrorResponse {
    let mut resp = ErrorResponse::new(Some(ensemble_link::protocol::mismatch(VERSION, version)));
    *resp.status_mut() = tungstenite::http::StatusCode::UPGRADE_REQUIRED;
    let headers = resp.headers_mut();
    headers.insert(VERSION_HEADER, VERSION.into());
    headers.insert("upgrade", "websocket".parse().expect("a header"));
    resp
}

/// Tell the runner something unasked.
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

/// A hub server for tests.
#[cfg(test)]
pub mod testing {
    use super::*;
    use ensemble_core::workspace::testing::TempDir;
    use std::sync::atomic::Ordering;

    /// The password of every account of the test hub.
    pub const PASSWORD: &str = "correct horse battery";

    /// A hub server on loopback, with `ana` a member (with `tasks` and
    /// `compute`), `rex` a reader and `root` an admin (with `tasks` and
    /// `host`), each with a token and [`PASSWORD`]. It locks a name
    /// after 3 failed logins, takes 2 sign-ups at a time, and serves MCP.
    pub struct Running {
        pub tmp: TempDir,
        pub url: String,
        pub ana: String,
        pub rex: String,
        pub root: String,
        pub server: &'static Server<'static>,
        stop: &'static AtomicBool,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    pub fn start() -> Running {
        start_with(None)
    }

    /// The test hub, keeping its accounts' tasks in `tasks`.
    pub fn start_with_tasks(tasks: &'static dyn TaskService) -> Running {
        start_with(Some(tasks))
    }

    /// The test hub, keeping tasks in `tasks` when given.
    pub fn start_with(tasks: Option<&'static dyn TaskService>) -> Running {
        let tmp = TempDir::new();
        let yaml = format!(
            "version: 1\nowner: Acme\nllm:\n  direct:\n    backends:\n      local: {{ type: openai-compatible-api, base_url: \"http://127.0.0.1:1/v1\", timeout_seconds: 5 }}\nknowledge:\n  vault: \"{}\"\n  distill:\n    try:\n      - {{ backend: local, model: small }}\n    when_none: wait\n    max_tokens: 100\n  recall: {{ max_notes: 3, max_chars: 1000 }}\nserver: {{ listen: \"127.0.0.1:0\", registration: {{ type: approval, max_pending: 2 }}, sessions: {{ hours: 1 }}, lockout: {{ failures: 3, minutes: 1 }} }}\nroles:\n  admin: [admin, recall, tasks, host]\n  member: [share, recall, tasks, compute]\n  reader: [recall]\n",
            tmp.0.join("vault").display()
        );
        let hub =
            ensemble_config::parse_hub(&yaml, std::path::Path::new("hub.yaml"), &BTreeMap::new())
                .unwrap();
        let memory = Memory::new(&hub, None);
        memory.vault().init().unwrap();
        memory
            .vault()
            .write_note(&ensemble_knowledge::Note {
                slug: "deploy-day".into(),
                kind: "decision".into(),
                title: "Deploy day".into(),
                tags: Vec::new(),
                audience: ensemble_knowledge::Audience::Hub,
                sources: vec!["s".into()],
                shared_by: vec!["ana".into()],
                pipeline: 1,
                updated: "2026-11-11T00:00:00Z".into(),
                body: "Deploys go out on Tuesdays.".into(),
                related: Vec::new(),
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
        let mut server =
            Server::new(memory, &hub.roles, hub.server, tmp.0.join("state")).with_mcp();
        if let Some(tasks) = tasks {
            server = server.with_tasks(tasks);
        }
        let server: &'static Server<'static> = Box::leak(Box::new(server));
        runtime::shared().spawn(server.run(listener, stop, &|_| {}));
        Running {
            tmp,
            url,
            ana,
            rex,
            root,
            server,
            stop,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::testing::*;
    use super::*;
    use tungstenite::client::IntoClientRequest;

    type Client = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

    fn connect(url: &str, token: Option<&str>) -> Result<Client, tungstenite::Error> {
        connect_as(url, token, Some(&VERSION.to_string()))
    }

    /// A link that says it speaks `version`, or says nothing of it.
    fn connect_as(
        url: &str,
        token: Option<&str>,
        version: Option<&str>,
    ) -> Result<Client, tungstenite::Error> {
        let mut req = url.into_client_request().unwrap();
        if let Some(token) = token {
            req.headers_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
        }
        if let Some(version) = version {
            req.headers_mut()
                .insert(VERSION_HEADER, version.parse().unwrap());
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
    fn a_runner_with_a_token_that_is_not_valid_is_refused() {
        let hub = start();
        for token in ["ens_wrong", ""] {
            match connect(&hub.url, Some(token)) {
                Err(tungstenite::Error::Http(resp)) => assert_eq!(resp.status(), 401),
                other => panic!("{token:?}: {:?}", other.map(|_| ())),
            }
        }
    }

    #[test]
    fn a_link_without_a_token_may_only_sign_in() {
        let hub = start();
        let mut ws = connect(&hub.url, None).unwrap();
        for (id, method) in [(1, HELLO), (2, RECALL), (3, RUNNERS)] {
            let err = ask(&mut ws, id, method, json!({"query": "deploy"}))
                .error
                .unwrap();
            assert!(
                err.contains("not signed in") && err.contains("`login` or `join`"),
                "{method}: {err}"
            );
        }
        let err = ask(&mut ws, 4, JOIN, json!({"code": "join_nope"}))
            .error
            .unwrap();
        assert!(err.contains("`runner` is required"), "{err}");
    }

    #[test]
    fn a_runner_of_another_protocol_version_is_told_what_to_update() {
        let hub = start();
        let newer = (VERSION + 1).to_string();
        let older = (VERSION - 1).to_string();
        for (version, update) in [
            (Some(newer.as_str()), "update ensemble-hub"),
            (Some(older.as_str()), "update ensemble-runner"),
            (None, "named none: update ensemble-runner"),
            (Some("one"), "named none: update ensemble-runner"),
        ] {
            // Refused before the token is looked at.
            match connect_as(&hub.url, Some(&hub.ana), version) {
                Err(tungstenite::Error::Http(resp)) => {
                    assert_eq!(resp.status(), 426);
                    assert_eq!(resp.headers()[VERSION_HEADER], VERSION.to_string());
                    let said = String::from_utf8(resp.body().clone().unwrap()).unwrap();
                    assert!(
                        said.contains(&format!("this hub speaks link protocol {VERSION}"))
                            && said.contains(update),
                        "{version:?}: {said}"
                    );
                }
                other => panic!("{version:?}: {:?}", other.map(|_| ())),
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
        assert_eq!(shares[0].shared_by, "ana", "never a name the client sends");
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
