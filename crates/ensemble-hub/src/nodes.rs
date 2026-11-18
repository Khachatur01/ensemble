//! The nodes connected to a hub server, and what passes between them.
//!
//! A node is a connection that said `node`: it has an owner (the account it
//! signed in as), a name, whom it works for (`private`: its owner;
//! `shared`: also everyone whose role has `compute`), the agents it runs for
//! others and the models it serves. The hub keeps no node when its
//! connection is gone; nodes are presence, not state.
//!
//! - A **run**: a connection asks for an agent on one of the nodes it names;
//!   the hub picks the first that is connected, may be used by that account
//!   and offers the agent, sends it a `run` event, and passes what the node
//!   says about it (`run_progress`, `run_done`) back to the connection that
//!   asked. When either side goes away, the other is told.
//! - A **model call**: an OpenAI-compatible request to `/nodes/v1/` on the
//!   hub's port, with an access token as the API key, goes to a node that
//!   serves the model as a `model_call` event, and what the node's server
//!   answers is streamed back as it comes.
//!
//! Every connection has an outbox here: events for it are put there by
//! whichever connection has them, and the connection's own task sends them.

use crate::http::{self, Head};
use crate::server::Server;
use ensemble_config::Permission;
use ensemble_core::runtime::blocking;
use ensemble_link::protocol::{Event, MODEL_CALL_EVENT, RUN_CANCEL_EVENT, RUN_DONE, RUN_EVENT};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc::{
    UnboundedReceiver as Receiver, UnboundedSender as Sender, unbounded_channel as channel,
};

/// The most a model request's body may be.
const MAX_MODEL_BODY: usize = 16 * 1024 * 1024;
/// How long a model call waits for its node when the node said nothing of
/// its own.
const MODEL_TIMEOUT: Duration = Duration::from_secs(600);

/// One connected node.
#[derive(Debug, Clone)]
struct Node {
    owner: String,
    name: String,
    shared: bool,
    /// The agents it runs, with the first line of each one's prompt.
    agents: Vec<(String, String)>,
    models: Vec<String>,
    model_timeout: Duration,
}

impl Node {
    fn id(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// May `user` use it? Its owner may; others when it is shared and their
    /// role has `compute`.
    fn serves(&self, user: &str, compute: bool) -> bool {
        self.owner == user || (self.shared && compute)
    }
}

struct Run {
    /// The connection that asked for it.
    caller: u64,
    /// The node's connection.
    node: u64,
}

struct Call {
    node: u64,
    reply: Sender<Value>,
}

#[derive(Default)]
struct Inner {
    outboxes: HashMap<u64, Sender<Event>>,
    nodes: HashMap<u64, Node>,
    runs: HashMap<String, Run>,
    calls: HashMap<String, Call>,
}

/// The connections of a hub server, the nodes among them, and the runs and
/// model calls going on between them.
#[derive(Default)]
pub(crate) struct Routes {
    inner: Mutex<Inner>,
    next: AtomicU64,
}

impl Routes {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fresh(&self, kind: &str) -> String {
        format!("{kind}-{}", self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// A new connection: its number, and the events for it.
    pub(crate) fn connect(&self) -> (u64, Receiver<Event>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = channel();
        self.lock().outboxes.insert(id, tx);
        (id, rx)
    }

    /// A connection ended: its node is gone, a run it asked for is stopped,
    /// and whoever waited on a run or a call it had is told.
    pub(crate) fn disconnect(&self, conn: u64) {
        let mut inner = self.lock();
        inner.outboxes.remove(&conn);
        let gone = inner.nodes.remove(&conn).map(|n| n.id());
        let ended: Vec<String> = inner
            .runs
            .iter()
            .filter(|(_, r)| r.caller == conn || r.node == conn)
            .map(|(id, _)| id.clone())
            .collect();
        for run in ended {
            let Some(route) = inner.runs.remove(&run) else {
                continue;
            };
            if route.node == conn {
                let why = format!(
                    "the node {} went away before the run ended",
                    gone.as_deref().unwrap_or("?")
                );
                tell(
                    &inner,
                    route.caller,
                    RUN_DONE,
                    json!({"run": run, "ok": false, "error": why}),
                );
            } else {
                tell(&inner, route.node, RUN_CANCEL_EVENT, json!({ "run": run }));
            }
        }
        let ended: Vec<String> = inner
            .calls
            .iter()
            .filter(|(_, c)| c.node == conn)
            .map(|(id, _)| id.clone())
            .collect();
        for call in ended.iter().filter_map(|id| inner.calls.remove(id)) {
            let _ = call
                .reply
                .send(json!({"error": "the node went away before it answered"}));
        }
    }

    /// Make `conn` a node of `owner`. Its name must not be taken by another
    /// connection of the same owner.
    pub(crate) fn register(
        &self,
        conn: u64,
        owner: &str,
        params: &Value,
        may_host: bool,
    ) -> Result<Value, String> {
        let name = params["name"].as_str().unwrap_or("").trim().to_string();
        if !ensemble_config::runner::valid_name(&name) {
            return Err(format!(
                "node: '{name}' is not a node name: letters, digits, '-', '_' or '.'"
            ));
        }
        let shared = match params["access"].as_str() {
            Some("shared") => true,
            Some("private") => false,
            other => {
                return Err(format!(
                    "node: `access` must be private or shared, not {other:?}"
                ));
            }
        };
        if shared && !may_host {
            return Err(
                "node: a shared node needs the `host` permission in your role; connect it as private, or ask an admin"
                    .into(),
            );
        }
        let texts = |key: &str| -> Vec<String> {
            params[key]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default()
        };
        let agents = params["agents"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| {
                        Some((
                            v["name"].as_str()?.to_string(),
                            v["about"].as_str().unwrap_or("").to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let node = Node {
            owner: owner.to_string(),
            name,
            shared,
            agents,
            models: texts("models"),
            model_timeout: params["model_timeout_seconds"]
                .as_u64()
                .filter(|s| *s > 0)
                .map_or(MODEL_TIMEOUT, Duration::from_secs),
        };
        let mut inner = self.lock();
        let id = node.id();
        if inner.nodes.iter().any(|(c, n)| *c != conn && n.id() == id) {
            return Err(format!(
                "node: {id} is connected already, from another process; give this one another name"
            ));
        }
        inner.nodes.insert(conn, node);
        Ok(json!({ "node": id }))
    }

    /// The nodes `user` may use.
    pub(crate) fn listed(&self, user: &str, compute: bool) -> Value {
        let inner = self.lock();
        let mut nodes: Vec<Value> = inner
            .nodes
            .iter()
            .filter(|(_, n)| n.serves(user, compute))
            .map(|(conn, n)| {
                let running = inner.runs.values().filter(|r| r.node == *conn).count();
                json!({
                    "node": n.id(),
                    "access": if n.shared { "shared" } else { "private" },
                    "agents": n.agents.iter().map(|(name, about)| json!({"name": name, "about": about})).collect::<Vec<_>>(),
                    "models": n.models,
                    "running": running,
                })
            })
            .collect();
        nodes.sort_by(|a, b| a["node"].as_str().cmp(&b["node"].as_str()));
        json!({ "nodes": nodes })
    }

    /// Send a run to the first of the nodes it names that `user` may use and
    /// that offers its agent; what the node says about it goes to `caller`.
    pub(crate) fn start_run(
        &self,
        caller: u64,
        user: &str,
        compute: bool,
        params: &Value,
    ) -> Result<Value, String> {
        let agent = params["agent"].as_str().unwrap_or("").trim();
        if agent.is_empty() {
            return Err("run: `agent` is required".into());
        }
        let wanted: Vec<&str> = params["nodes"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if wanted.is_empty() {
            return Err(
                "run: `nodes` is required: the nodes that may run it, as <owner>/<name>".into(),
            );
        }
        let mut inner = self.lock();
        let mut why = Vec::new();
        let mut chosen = None;
        for id in &wanted {
            match inner.nodes.iter().find(|(_, n)| n.id() == *id) {
                None => why.push(format!("{id} is not connected")),
                Some((_, n)) if !n.serves(user, compute) => why.push(format!(
                    "{id} is {}",
                    if n.shared {
                        "shared, but your role has no `compute`"
                    } else {
                        "private to its owner"
                    }
                )),
                Some((_, n)) if !n.agents.iter().any(|(a, _)| a == agent) => why.push(format!(
                    "{id} does not offer `{agent}`; it offers {}",
                    if n.agents.is_empty() {
                        "no agents".to_string()
                    } else {
                        n.agents
                            .iter()
                            .map(|(a, _)| a.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                )),
                Some((conn, _)) => {
                    chosen = Some((*conn, id.to_string()));
                    break;
                }
            }
        }
        let Some((node, id)) = chosen else {
            return Err(format!(
                "run: no node can run `{agent}`: {}",
                why.join("; ")
            ));
        };
        let run = self.fresh("run");
        let event = json!({
            "run": run,
            "from": user,
            "agent": agent,
            "task": params["task"],
            "context": params["context"],
            "files": params["files"],
        });
        inner.runs.insert(run.clone(), Run { caller, node });
        tell(&inner, node, RUN_EVENT, event);
        Ok(json!({ "run": run, "node": id }))
    }

    /// The caller stops a run it asked for.
    pub(crate) fn cancel_run(&self, caller: u64, params: &Value) -> Result<Value, String> {
        let run = params["run"].as_str().unwrap_or("");
        let mut inner = self.lock();
        match inner.runs.get(run) {
            Some(r) if r.caller == caller => {
                let node = r.node;
                inner.runs.remove(run);
                tell(&inner, node, RUN_CANCEL_EVENT, json!({ "run": run }));
                Ok(json!({ "cancelled": run }))
            }
            _ => Err(format!("run_cancel: no run '{run}' of yours is going on")),
        }
    }

    /// What the node running a run says about it, for its caller.
    pub(crate) fn pass_on(&self, node: u64, method: &str, params: &Value) -> Result<Value, String> {
        let run = params["run"].as_str().unwrap_or("").to_string();
        let mut inner = self.lock();
        let Some(route) = inner.runs.get(&run).filter(|r| r.node == node) else {
            return Err(format!("{method}: no run '{run}' is going on on this node"));
        };
        let caller = route.caller;
        if method == RUN_DONE {
            inner.runs.remove(&run);
        }
        tell(&inner, caller, method, params.clone());
        Ok(json!({}))
    }

    /// What a node's model server answered, for whoever waits on the call.
    pub(crate) fn model_reply(&self, node: u64, params: &Value) -> Result<Value, String> {
        let call = params["call"].as_str().unwrap_or("").to_string();
        let mut inner = self.lock();
        let Some(route) = inner.calls.get(&call).filter(|c| c.node == node) else {
            return Err(format!("model_reply: no call '{call}' waits on this node"));
        };
        let gone = route.reply.send(params.clone()).is_err();
        if gone || params.get("done").is_some() || params.get("error").is_some() {
            inner.calls.remove(&call);
        }
        Ok(json!({}))
    }

    /// The models `user` may call, each once, with a node that serves it.
    fn models(&self, user: &str, compute: bool) -> Vec<(String, String)> {
        let inner = self.lock();
        let mut out: Vec<(String, String)> = Vec::new();
        for n in inner.nodes.values().filter(|n| n.serves(user, compute)) {
            for m in &n.models {
                if !out.iter().any(|(name, _)| name == m) {
                    out.push((m.clone(), n.id()));
                }
            }
        }
        out.sort();
        out
    }

    /// Send a model request to the least busy node that serves `model` to
    /// `user`, preferring the user's own: its call id, what the node answers,
    /// and how long to wait for it.
    fn call_model(
        &self,
        user: &str,
        compute: bool,
        model: &str,
        path: &str,
        body: Value,
    ) -> Result<(String, Receiver<Value>, Duration), String> {
        let mut inner = self.lock();
        let busy =
            |conn: u64, inner: &Inner| inner.calls.values().filter(|c| c.node == conn).count();
        let node = inner
            .nodes
            .iter()
            .filter(|(_, n)| n.serves(user, compute) && n.models.iter().any(|m| m == model))
            .min_by_key(|(conn, n)| (n.owner != user, busy(**conn, &inner)))
            .map(|(conn, n)| (*conn, n.model_timeout));
        let Some((node, timeout)) = node else {
            return Err(format!(
                "no node you may use serves the model '{model}'; GET /nodes/v1/models lists those that do"
            ));
        };
        let call = self.fresh("call");
        let (tx, rx) = channel();
        inner.calls.insert(call.clone(), Call { node, reply: tx });
        tell(
            &inner,
            node,
            MODEL_CALL_EVENT,
            json!({"call": call, "from": user, "path": path, "body": body}),
        );
        Ok((call, rx, timeout))
    }

    /// Stop waiting on a call whose client went away.
    fn end_call(&self, call: &str) {
        self.lock().calls.remove(call);
    }
}

/// Put an event in a connection's outbox.
fn tell(inner: &Inner, conn: u64, event: &str, data: Value) {
    if let Some(outbox) = inner.outboxes.get(&conn) {
        let _ = outbox.send(Event {
            event: event.into(),
            data,
        });
    }
}

impl Server<'_> {
    /// Answer one request to `/nodes/v1/`, the models of the nodes, as an
    /// OpenAI-compatible endpoint, then close the connection.
    pub(crate) async fn model_request(
        &self,
        mut stream: TcpStream,
        head: &Head,
        already: &[u8],
        say: &(dyn Fn(&str) + Sync),
    ) -> Result<(), String> {
        let refuse = async |stream: &mut TcpStream, status: u16, why: String| {
            say(&format!("{} {} {status}", head.method, head.path));
            let body = json!({"error": {"message": why, "type": "ensemble_hub"}});
            http::respond(stream, status, &body)
                .await
                .map_err(|e| e.to_string())
        };
        let signed = blocking(|| {
            self.users()
                .ok()
                .and_then(|u| head.bearer().and_then(|t| u.authenticate(t, &self.roles())))
        });
        let Some(signed) = signed else {
            return refuse(
                &mut stream,
                401,
                "an access token of the hub is needed as the API key".into(),
            )
            .await;
        };
        let user = signed.user.name.clone();
        let compute = signed.permissions.contains(&Permission::Compute);
        let path = head.path.strip_prefix("/nodes/v1").unwrap_or("");
        match (head.method.as_str(), path) {
            ("GET", "/models") => {
                let data: Vec<Value> = self
                    .routes
                    .models(&user, compute)
                    .into_iter()
                    .map(|(m, node)| json!({"id": m, "object": "model", "owned_by": node}))
                    .collect();
                say(&format!("GET {} {user} 200", head.path));
                http::respond(&mut stream, 200, &json!({"object": "list", "data": data}))
                    .await
                    .map_err(|e| e.to_string())
            }
            ("POST", "/chat/completions" | "/completions") => {
                let body = http::read_body_up_to(&mut stream, head, already, MAX_MODEL_BODY)
                    .await
                    .and_then(|b| {
                        serde_json::from_slice::<Value>(&b)
                            .map_err(|e| format!("the body is not JSON: {e}"))
                    });
                let body = match body {
                    Ok(body) => body,
                    Err(e) => {
                        refuse(&mut stream, 400, e).await?;
                        http::drain(&mut stream).await;
                        return Ok(());
                    }
                };
                let model = body["model"].as_str().unwrap_or("").to_string();
                let (call, mut replies, timeout) =
                    match self.routes.call_model(&user, compute, &model, path, body) {
                        Ok(found) => found,
                        Err(e) => return refuse(&mut stream, 404, e).await,
                    };
                say(&format!("POST {} {user} {model} -> {call}", head.path));
                let result = stream_reply(&mut stream, &mut replies, timeout).await;
                self.routes.end_call(&call);
                if let Err(e) = &result {
                    say(&format!("{call}: {e}"));
                }
                Ok(())
            }
            _ => {
                refuse(
                    &mut stream,
                    404,
                    format!(
                        "no such path: {}; the nodes' models are at /nodes/v1/models and /nodes/v1/chat/completions",
                        head.path
                    ),
                )
                .await
            }
        }
    }
}

/// Write what the node answers for a call to the client as it comes: the
/// status and type first, then each chunk, until it is done.
async fn stream_reply(
    stream: &mut TcpStream,
    replies: &mut Receiver<Value>,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut next = async || -> Result<Value, String> {
        match tokio::time::timeout_at(deadline, replies.recv()).await {
            Ok(Some(v)) => Ok(v),
            Ok(None) => Err("the node went away".into()),
            Err(_) => Err(format!(
                "the node gave no answer within {}s",
                timeout.as_secs()
            )),
        }
    };
    let first = match next().await {
        Ok(v) => v,
        Err(e) => {
            let body = json!({"error": {"message": e, "type": "ensemble_hub"}});
            return http::respond(stream, 504, &body)
                .await
                .map_err(|e| e.to_string());
        }
    };
    if let Some(e) = first["error"].as_str() {
        let body = json!({"error": {"message": e, "type": "ensemble_hub"}});
        return http::respond(stream, 502, &body)
            .await
            .map_err(|e| e.to_string());
    }
    let status = first["status"].as_u64().unwrap_or(200);
    let kind = first["content_type"].as_str().unwrap_or("application/json");
    let head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: {kind}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n",
        if status < 400 { "OK" } else { "Error" }
    );
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut reply = first;
    loop {
        if let Some(chunk) = reply["chunk"].as_str() {
            let data = ensemble_core::snapshot::decode(chunk)?;
            stream
                .write_all(&data)
                .await
                .map_err(|e| format!("the client went away: {e}"))?;
        }
        if reply.get("done").is_some() {
            return Ok(());
        }
        if let Some(e) = reply["error"].as_str() {
            return Err(e.to_string());
        }
        reply = next().await?;
    }
}

#[cfg(test)]
mod tests {
    use crate::server::testing::start;
    use ensemble_core::runtime::blocking;
    use ensemble_link::client::Connection;
    use ensemble_link::protocol::*;
    use serde_json::{Value, json};
    use std::time::Duration;

    /// The next event of `event` kind on `link`, within a few seconds.
    async fn expect(link: &mut Connection, event: &str) -> Value {
        let wait = async {
            loop {
                let e = link.next_event().await.unwrap();
                if e.event == event {
                    return e.data;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait)
            .await
            .unwrap_or_else(|_| panic!("no `{event}` event came"))
    }

    async fn node(url: &str, token: &str, name: &str, access: &str) -> Connection {
        let mut link = Connection::open(url, token).await.unwrap();
        link.call(
            NODE,
            json!({"name": name, "access": access,
                   "agents": [{"name": "coder", "about": "Writes code."}],
                   "models": ["small"], "model_timeout_seconds": 5}),
        )
        .await
        .unwrap();
        link
    }

    async fn open(url: &str, token: &str) -> Connection {
        Connection::open(url, token).await.unwrap()
    }

    /// What a call is refused with.
    async fn refused(link: &mut Connection, method: &str, params: Value) -> String {
        link.call(method, params).await.unwrap_err().message()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_goes_to_a_node_that_offers_the_agent_and_its_end_comes_back() {
        let hub = start();
        // root may host a shared node; ana, a member, may not.
        let mut gpu = node(&hub.url, &hub.root, "gpu-box", "shared").await;
        let mut ana = open(&hub.url, &hub.ana).await;
        let said = refused(
            &mut ana,
            NODE,
            json!({"name": "laptop", "access": "shared"}),
        )
        .await;
        assert!(said.contains("`host`"), "{said}");
        let listed = ana.call(NODES, json!({})).await.unwrap();
        assert_eq!(listed["nodes"][0]["node"], "root/gpu-box");
        assert_eq!(listed["nodes"][0]["agents"][0]["about"], "Writes code.");

        let started = ana
            .call(
                RUN,
                json!({"agent": "coder", "nodes": ["ana/none", "root/gpu-box"],
                       "task": "fix it", "context": null,
                       "files": [{"path": "a.txt", "data": "aGk="}]}),
            )
            .await
            .unwrap();
        assert_eq!(started["node"], "root/gpu-box");
        let run = started["run"].as_str().unwrap().to_string();
        let got = expect(&mut gpu, RUN_EVENT).await;
        assert_eq!(
            (got["from"].as_str(), got["task"].as_str()),
            (Some("ana"), Some("fix it"))
        );
        assert_eq!(got["files"][0]["path"], "a.txt");
        gpu.send(RUN_PROGRESS, json!({"run": run, "line": "coder · reading"}))
            .await
            .unwrap();
        gpu.send(RUN_DONE, json!({"run": run, "ok": true, "text": "fixed"}))
            .await
            .unwrap();
        assert_eq!(
            expect(&mut ana, RUN_PROGRESS).await["line"],
            "coder · reading"
        );
        assert_eq!(expect(&mut ana, RUN_DONE).await["text"], "fixed");

        // A reader has no `compute`: the shared node is not theirs to use.
        let mut rex = open(&hub.url, &hub.rex).await;
        assert_eq!(
            rex.call(NODES, json!({})).await.unwrap()["nodes"],
            json!([])
        );
        let said = refused(
            &mut rex,
            RUN,
            json!({"agent": "coder", "nodes": ["root/gpu-box"], "task": "x"}),
        )
        .await;
        assert!(said.contains("no `compute`"), "{said}");
        let said = refused(
            &mut ana,
            RUN,
            json!({"agent": "writer", "nodes": ["root/gpu-box"], "task": "x"}),
        )
        .await;
        assert!(
            said.contains("does not offer `writer`; it offers coder"),
            "{said}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_private_node_serves_its_owner_alone_and_a_name_is_taken_once() {
        let hub = start();
        let _laptop = node(&hub.url, &hub.ana, "laptop", "private").await;
        let mut again = open(&hub.url, &hub.ana).await;
        let said = refused(
            &mut again,
            NODE,
            json!({"name": "laptop", "access": "private"}),
        )
        .await;
        assert!(said.contains("connected already"), "{said}");
        let mut root = open(&hub.url, &hub.root).await;
        let said = refused(
            &mut root,
            RUN,
            json!({"agent": "coder", "nodes": ["ana/laptop"], "task": "x"}),
        )
        .await;
        assert!(said.contains("private to its owner"), "{said}");
        assert!(
            again
                .call(
                    RUN,
                    json!({"agent": "coder", "nodes": ["ana/laptop"], "task": "x"})
                )
                .await
                .is_ok()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn when_one_side_of_a_run_goes_away_the_other_is_told() {
        let hub = start();
        let mut gpu = node(&hub.url, &hub.root, "gpu-box", "shared").await;
        let mut ana = open(&hub.url, &hub.ana).await;
        async fn run(ana: &mut Connection) -> String {
            ana.call(
                RUN,
                json!({"agent": "coder", "nodes": ["root/gpu-box"], "task": "x"}),
            )
            .await
            .unwrap()["run"]
                .as_str()
                .unwrap()
                .to_string()
        }
        // The caller cancels; then the caller goes away.
        let first = run(&mut ana).await;
        expect(&mut gpu, RUN_EVENT).await;
        ana.call(RUN_CANCEL, json!({ "run": first })).await.unwrap();
        assert_eq!(expect(&mut gpu, RUN_CANCEL_EVENT).await["run"], first);
        let second = run(&mut ana).await;
        expect(&mut gpu, RUN_EVENT).await;
        ana.close().await;
        drop(ana);
        assert_eq!(expect(&mut gpu, RUN_CANCEL_EVENT).await["run"], second);
        // The node goes away.
        let mut ana = open(&hub.url, &hub.ana).await;
        run(&mut ana).await;
        expect(&mut gpu, RUN_EVENT).await;
        gpu.close().await;
        drop(gpu);
        let done = expect(&mut ana, RUN_DONE).await;
        assert_eq!(done["ok"], false);
        assert!(
            done["error"].as_str().unwrap().contains("went away"),
            "{done}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_model_call_is_streamed_from_the_node_that_serves_the_model() {
        let hub = start();
        let mut gpu = node(&hub.url, &hub.root, "gpu-box", "shared").await;
        let http = hub.url.replace("ws://", "http://");
        let get = |token: &str| {
            blocking(|| {
                ureq::get(format!("{http}nodes/v1/models"))
                    .header("authorization", format!("Bearer {token}"))
                    .config()
                    .http_status_as_error(false)
                    .build()
                    .call()
                    .unwrap()
            })
        };
        let mut listed = get(&hub.ana);
        let models: Value =
            serde_json::from_str(&listed.body_mut().read_to_string().unwrap()).unwrap();
        assert_eq!(models["data"][0]["id"], "small");
        assert_eq!(models["data"][0]["owned_by"], "root/gpu-box");
        assert_eq!(get("ens_wrong").status(), 401);

        // The node answers in two chunks while the client reads.
        let node = tokio::spawn(async move {
            let call = expect(&mut gpu, MODEL_CALL_EVENT).await;
            assert_eq!(call["path"], "/chat/completions");
            assert_eq!(call["body"]["model"], "small");
            assert_eq!(call["from"], "ana");
            let id = call["call"].clone();
            gpu.send(
                MODEL_REPLY,
                json!({"call": id, "status": 200, "content_type": "text/event-stream"}),
            )
            .await
            .unwrap();
            for part in ["data: one\n\n", "data: [DONE]\n\n"] {
                let chunk = ensemble_core::snapshot::encode(part.as_bytes());
                gpu.send(MODEL_REPLY, json!({"call": id, "chunk": chunk}))
                    .await
                    .unwrap();
            }
            gpu.send(MODEL_REPLY, json!({"call": id, "done": true}))
                .await
                .unwrap();
            gpu
        });
        let (kind, body) = blocking(|| {
            let mut answer = ureq::post(format!("{http}nodes/v1/chat/completions"))
                .header("authorization", format!("Bearer {}", hub.ana))
                .send(json!({"model": "small", "stream": true, "messages": []}).to_string())
                .unwrap();
            let kind = answer.headers()["content-type"].clone();
            (kind, answer.body_mut().read_to_string().unwrap())
        });
        assert_eq!(kind, "text/event-stream");
        assert_eq!(body, "data: one\n\ndata: [DONE]\n\n");
        let _gpu = node.await.unwrap();

        // A model nobody serves, and a reader without `compute`.
        let missing = blocking(|| {
            ureq::post(format!("{http}nodes/v1/chat/completions"))
                .header("authorization", format!("Bearer {}", hub.ana))
                .config()
                .http_status_as_error(false)
                .build()
                .send(json!({"model": "huge", "messages": []}).to_string())
                .unwrap()
        });
        assert_eq!(missing.status(), 404);
        let mut rex = get(&hub.rex);
        let none: Value = serde_json::from_str(&rex.body_mut().read_to_string().unwrap()).unwrap();
        assert_eq!(none["data"], json!([]));
    }
}
