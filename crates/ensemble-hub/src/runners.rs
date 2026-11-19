//! The runners connected to a hub server, and the model calls between them.
//!
//! A runner is a connection that said `runner`: it has an owner (the account
//! it signed in as), a name, whom it works for (`private`: its owner;
//! `shared`: also everyone whose role has `compute`), every agent of its
//! teams, the agents it offers others and the models it serves. The hub keeps
//! no runner when its connection is gone; runners are presence, not state.
//! Who is connected says whether a task its runners gave back is blocked
//! (`board`).
//!
//! A **model call**: an OpenAI-compatible request to `/nodes/v1/` on the
//! hub's port, with an access token as the API key, goes to a runner that
//! serves the model as a `model_call` event, and what the runner's server
//! answers is streamed back as it comes.
//!
//! Every connection has an outbox here: events for it are put there by
//! whichever connection has them, and the connection's own task sends them.

use crate::board::Present;
use crate::http::{self, Head};
use crate::server::Server;
use ensemble_config::Permission;
use ensemble_core::runtime::blocking;
use ensemble_link::protocol::{Event, MODEL_CALL_EVENT};
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
/// How long a model call waits for its runner when the runner said nothing
/// of its own.
const MODEL_TIMEOUT: Duration = Duration::from_secs(600);

/// One connected runner.
#[derive(Debug, Clone)]
struct Runner {
    owner: String,
    name: String,
    shared: bool,
    /// Every agent of its teams, with the first line of each one's prompt.
    agents: Vec<(String, String)>,
    /// The agents it offers others.
    offers: Vec<String>,
    models: Vec<String>,
    model_timeout: Duration,
}

impl Runner {
    fn id(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// May `user` use it? Its owner may; others when it is shared and their
    /// role has `compute`.
    fn serves(&self, user: &str, compute: bool) -> bool {
        self.owner == user || (self.shared && compute)
    }

    /// Its agents `user` sees: all of them for its owner, those it offers
    /// for anyone else.
    fn agents_for(&self, user: &str) -> Vec<&(String, String)> {
        self.agents
            .iter()
            .filter(|(name, _)| self.owner == user || self.offers.contains(name))
            .collect()
    }
}

struct Call {
    runner: u64,
    reply: Sender<Value>,
}

#[derive(Default)]
struct Inner {
    outboxes: HashMap<u64, Sender<Event>>,
    runners: HashMap<u64, Runner>,
    calls: HashMap<String, Call>,
}

/// The connections of a hub server, the runners among them, and the model
/// calls going on between them.
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

    /// A connection ended: its runner is gone, and whoever waited on a call it
    /// had is told.
    pub(crate) fn disconnect(&self, conn: u64) {
        let mut inner = self.lock();
        inner.outboxes.remove(&conn);
        inner.runners.remove(&conn);
        let ended: Vec<String> = inner
            .calls
            .iter()
            .filter(|(_, c)| c.runner == conn)
            .map(|(id, _)| id.clone())
            .collect();
        for call in ended.iter().filter_map(|id| inner.calls.remove(id)) {
            let _ = call
                .reply
                .send(json!({"error": "the runner went away before it answered"}));
        }
    }

    /// Make `conn` a runner of `owner`, or say again what it has. Its name
    /// must not be taken by another connection of the same owner.
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
                "runner: '{name}' is not a runner name: letters, digits, '-', '_' or '.'"
            ));
        }
        let shared = match params["access"].as_str() {
            Some("shared") => true,
            Some("private") => false,
            other => {
                return Err(format!(
                    "runner: `access` must be private or shared, not {other:?}"
                ));
            }
        };
        if shared && !may_host {
            return Err(
                "runner: a shared runner needs the `host` permission in your role; connect it as private, or ask an admin"
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
        let runner = Runner {
            owner: owner.to_string(),
            name,
            shared,
            agents,
            offers: texts("offers"),
            models: texts("models"),
            model_timeout: params["model_timeout_seconds"]
                .as_u64()
                .filter(|s| *s > 0)
                .map_or(MODEL_TIMEOUT, Duration::from_secs),
        };
        let mut inner = self.lock();
        let id = runner.id();
        if inner
            .runners
            .iter()
            .any(|(c, r)| *c != conn && r.id() == id)
        {
            return Err(format!(
                "runner: {id} is connected already, from another process; give this one another name"
            ));
        }
        inner.runners.insert(conn, runner);
        Ok(json!({ "runner": id }))
    }

    /// The runners `user` may use, each with the agents they see of it.
    pub(crate) fn listed(&self, user: &str, compute: bool) -> Value {
        let inner = self.lock();
        let mut runners: Vec<Value> = inner
            .runners
            .values()
            .filter(|r| r.serves(user, compute))
            .map(|r| {
                json!({
                    "runner": r.id(),
                    "owner": r.owner,
                    "access": if r.shared { "shared" } else { "private" },
                    "agents": r.agents_for(user).iter().map(|(name, about)| json!({"name": name, "about": about})).collect::<Vec<_>>(),
                    "models": r.models,
                })
            })
            .collect();
        runners.sort_by(|a, b| a["runner"].as_str().cmp(&b["runner"].as_str()));
        json!({ "runners": runners })
    }

    /// `owner`'s runners connected now, and their agents.
    pub(crate) fn present(&self, owner: &str) -> Vec<Present> {
        let mut present: Vec<Present> = self
            .lock()
            .runners
            .values()
            .filter(|r| r.owner == owner)
            .map(|r| Present {
                name: r.name.clone(),
                agents: r.agents.iter().map(|(name, _)| name.clone()).collect(),
            })
            .collect();
        present.sort_by(|a, b| a.name.cmp(&b.name));
        present
    }

    /// What a runner's model server answered, for whoever waits on the call.
    pub(crate) fn model_reply(&self, runner: u64, params: &Value) -> Result<Value, String> {
        let call = params["call"].as_str().unwrap_or("").to_string();
        let mut inner = self.lock();
        let Some(route) = inner.calls.get(&call).filter(|c| c.runner == runner) else {
            return Err(format!(
                "model_reply: no call '{call}' waits on this runner"
            ));
        };
        let gone = route.reply.send(params.clone()).is_err();
        if gone || params.get("done").is_some() || params.get("error").is_some() {
            inner.calls.remove(&call);
        }
        Ok(json!({}))
    }

    /// The models `user` may call, each once, with a runner that serves it.
    fn models(&self, user: &str, compute: bool) -> Vec<(String, String)> {
        let inner = self.lock();
        let mut out: Vec<(String, String)> = Vec::new();
        for r in inner.runners.values().filter(|r| r.serves(user, compute)) {
            for m in &r.models {
                if !out.iter().any(|(name, _)| name == m) {
                    out.push((m.clone(), r.id()));
                }
            }
        }
        out.sort();
        out
    }

    /// Send a model request to the least busy runner that serves `model` to
    /// `user`, preferring the user's own: its call id, what the runner
    /// answers, and how long to wait for it.
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
            |conn: u64, inner: &Inner| inner.calls.values().filter(|c| c.runner == conn).count();
        let runner = inner
            .runners
            .iter()
            .filter(|(_, r)| r.serves(user, compute) && r.models.iter().any(|m| m == model))
            .min_by_key(|(conn, r)| (r.owner != user, busy(**conn, &inner)))
            .map(|(conn, r)| (*conn, r.model_timeout));
        let Some((runner, timeout)) = runner else {
            return Err(format!(
                "no runner you may use serves the model '{model}'; GET /nodes/v1/models lists those that do"
            ));
        };
        let call = self.fresh("call");
        let (tx, rx) = channel();
        inner.calls.insert(call.clone(), Call { runner, reply: tx });
        tell(
            &inner,
            runner,
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
    /// Answer one request to `/nodes/v1/`, the models of the runners, as an
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
                    .map(|(m, runner)| json!({"id": m, "object": "model", "owned_by": runner}))
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
                        "no such path: {}; the runners' models are at /nodes/v1/models and /nodes/v1/chat/completions",
                        head.path
                    ),
                )
                .await
            }
        }
    }
}

/// Write what the runner answers for a call to the client as it comes: the
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
            Ok(None) => Err("the runner went away".into()),
            Err(_) => Err(format!(
                "the runner gave no answer within {}s",
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
            let data = ensemble_link::base64::decode(chunk)?;
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

    async fn runner(url: &str, token: &str, name: &str, access: &str) -> Connection {
        let mut link = Connection::open(url, token).await.unwrap();
        link.call(
            RUNNER,
            json!({"name": name, "access": access,
                   "agents": [{"name": "web/coder", "about": "Writes code."},
                              {"name": "web/reviewer", "about": "Reviews it."}],
                   "offers": ["web/reviewer"],
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

    /// The names of the agents of the runner listed `at`.
    fn agents(listed: &Value, at: usize) -> Vec<&str> {
        listed["runners"][at]["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_runner_is_listed_to_its_owner_with_all_its_agents_and_to_others_with_what_it_offers()
     {
        let hub = start();
        // root may connect a shared runner; ana, a member, may not.
        let _gpu = runner(&hub.url, &hub.root, "gpu-box", "shared").await;
        let mut ana = open(&hub.url, &hub.ana).await;
        let said = refused(
            &mut ana,
            RUNNER,
            json!({"name": "laptop", "access": "shared"}),
        )
        .await;
        assert!(said.contains("`host`"), "{said}");
        // A runner that offers nothing is listed to its owner all the same.
        ana.call(
            RUNNER,
            json!({"name": "laptop", "access": "private",
                   "agents": [{"name": "web/coder", "about": ""}]}),
        )
        .await
        .unwrap();
        let listed = ana.call(RUNNERS, json!({})).await.unwrap();
        assert_eq!(listed["runners"][0]["runner"], "ana/laptop");
        assert_eq!(listed["runners"][0]["owner"], "ana");
        assert_eq!(agents(&listed, 0), ["web/coder"]);
        assert_eq!(listed["runners"][1]["runner"], "root/gpu-box");
        assert_eq!(agents(&listed, 1), ["web/reviewer"], "what it offers ana");
        assert_eq!(listed["runners"][1]["agents"][0]["about"], "Reviews it.");
        let mut root = open(&hub.url, &hub.root).await;
        let listed = root.call(RUNNERS, json!({})).await.unwrap();
        assert_eq!(
            listed["runners"].as_array().unwrap().len(),
            1,
            "ana's is private"
        );
        assert_eq!(agents(&listed, 0), ["web/coder", "web/reviewer"]);
        // Said again, as when its teams change: what it has now.
        ana.call(
            RUNNER,
            json!({"name": "laptop", "access": "private", "agents": []}),
        )
        .await
        .unwrap();
        let listed = ana.call(RUNNERS, json!({})).await.unwrap();
        assert!(agents(&listed, 0).is_empty());

        // A reader has no `compute`: the shared runner is not theirs to use.
        let mut rex = open(&hub.url, &hub.rex).await;
        assert_eq!(
            rex.call(RUNNERS, json!({})).await.unwrap()["runners"],
            json!([])
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_runners_name_is_taken_once_and_it_is_present_while_connected() {
        let hub = start();
        let laptop = runner(&hub.url, &hub.ana, "laptop", "private").await;
        let mut again = open(&hub.url, &hub.ana).await;
        let said = refused(
            &mut again,
            RUNNER,
            json!({"name": "laptop", "access": "private"}),
        )
        .await;
        assert!(said.contains("connected already"), "{said}");
        let present = hub.server.routes.present("ana");
        assert_eq!(present.len(), 1);
        assert_eq!(
            (present[0].name.as_str(), present[0].agents.clone()),
            (
                "laptop",
                vec!["web/coder".to_string(), "web/reviewer".into()]
            )
        );
        assert!(hub.server.routes.present("root").is_empty());
        let mut laptop = laptop;
        laptop.close().await;
        drop(laptop);
        let gone = async {
            while !hub.server.routes.present("ana").is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), gone)
            .await
            .expect("a runner that went away is not present");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_model_call_is_streamed_from_the_runner_that_serves_the_model() {
        let hub = start();
        let mut gpu = runner(&hub.url, &hub.root, "gpu-box", "shared").await;
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

        // The runner answers in two chunks while the client reads.
        let serving = tokio::spawn(async move {
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
                let chunk = ensemble_link::base64::encode(part.as_bytes());
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
        let _gpu = serving.await.unwrap();

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
