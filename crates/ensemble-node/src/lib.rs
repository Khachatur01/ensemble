//! A node: a worker that gives this computer's power to the people of a hub
//! server. `ensemble node serve` runs one, from a `node.yaml`.
//!
//! It keeps one connection to the hub, says what it is (`node`), and then
//! does what comes:
//!
//! - a `run` event: the files it carries are written into a folder of the
//!   run's own, the agent it names (one this node offers, from the helper
//!   team in `offers.runs`) works there, and what the agent said and changed
//!   goes back; a `run_cancel` stops it;
//! - a `model_call` event: the request goes to the model server in
//!   `offers.models`, and its answer goes back in chunks as it comes.
//!
//! Each goes on beside the connection, in its task. Only the connection uses
//! the socket: the others hand it what to send.
//!
//! A shared node works for other people, so an agent it offers must run on
//! models from `llm_backends`: an agent CLI's own login is its owner's, and
//! is never used for someone else's work.

use ensemble_agents::router::Ensemble;
use ensemble_config::{Access, Adapter, Config, Node, OpenCodeModel};
use ensemble_core::runtime::{self, blocking};
use ensemble_core::snapshot;
use ensemble_link::client::{Connection, Failure};
use ensemble_link::protocol::{
    MODEL_CALL_EVENT, MODEL_REPLY, NODE, RUN_CANCEL_EVENT, RUN_DONE, RUN_EVENT, RUN_PROGRESS,
};
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

/// The most of a model's answer sent in one chunk.
const CHUNK: usize = 16 * 1024;
/// The paths of a model server a call may ask for.
const MODEL_PATHS: [&str; 2] = ["/chat/completions", "/completions"];

/// What a run or a model call hands the connection to send.
type Outbox = UnboundedSender<(&'static str, Value)>;

/// A node, ready to connect.
pub struct Worker {
    url: String,
    token: String,
    node: Node,
    /// The node's helper team, when it offers runs.
    team: Option<Ensemble>,
    /// Where runs work, each in a folder of its own.
    scratch: PathBuf,
    /// Runs going on, per agent, so an agent's `max_concurrent` holds.
    busy: Mutex<HashMap<String, u32>>,
    freed: Notify,
}

impl Worker {
    /// A worker for `node`, reaching the hub at `url` with `token`. `team` is
    /// its teams, loaded as one; each agent offered must be in it, and on a
    /// shared node run on `llm_backends`.
    pub fn new(
        url: &str,
        token: &str,
        node: Node,
        team: Option<Config>,
        scratch: PathBuf,
    ) -> Result<Worker, String> {
        let team = match (&node.offers.runs, team) {
            (Some(runs), Some(config)) => {
                for name in &runs.agents {
                    let Some(agent) = config.agent(name) else {
                        return Err(format!(
                            "offers.runs: no team of this node has the agent '{name}'. Its agents: {}.",
                            config.agent_names().join(", ")
                        ));
                    };
                    if node.access == Access::Shared && !on_llm_backends(&agent.adapter) {
                        return Err(format!(
                            "offers.runs: '{name}' runs on {}, which uses an agent CLI's own login or provider, and a shared node never lends that; offer an agent of type opencode on llm_backends, builtin, or direct",
                            agent.runs_on()
                        ));
                    }
                }
                Some(Ensemble::new(config, None).with_state(scratch.join("state")))
            }
            (Some(_), None) => {
                return Err("offers.runs: the helper team was not loaded".into());
            }
            (None, _) => None,
        };
        Ok(Worker {
            url: url.to_string(),
            token: token.to_string(),
            node,
            team,
            scratch,
            busy: Mutex::new(HashMap::new()),
            freed: Notify::new(),
        })
    }

    /// What this node says it is when it connects.
    fn hello(&self) -> Value {
        let agents: Vec<Value> = match (&self.node.offers.runs, &self.team) {
            (Some(runs), Some(team)) => runs
                .agents
                .iter()
                .map(|name| {
                    let about = team
                        .config()
                        .agent(name)
                        .and_then(|a| a.prompt.as_deref())
                        .and_then(|p| p.lines().map(str::trim).find(|l| !l.is_empty()))
                        .unwrap_or("");
                    json!({"name": name, "about": about})
                })
                .collect(),
            _ => Vec::new(),
        };
        let models = self.node.offers.models.as_ref();
        json!({
            "name": self.node.name,
            "access": self.node.access.as_str(),
            "agents": agents,
            "models": models.map(|m| m.names.clone()).unwrap_or_default(),
            "model_timeout_seconds": models.map(|m| m.timeout_seconds),
        })
    }

    /// Serve the hub until `stop` is set, connecting again when the link
    /// breaks. A hub that refuses the node (a shared node without `host`, a
    /// name taken) makes it return, saying why.
    pub async fn work(&self, stop: &AtomicBool, say: &(dyn Fn(&str) + Sync)) -> Result<(), String> {
        let mut backoff = Duration::from_secs(1);
        let mut said_down = false;
        while !stop.load(Ordering::Relaxed) {
            match self.session(stop, say).await {
                Ok(()) => return Ok(()),
                Err(Failure::Hub(e)) => return Err(e),
                Err(Failure::Broken(e)) => {
                    if !said_down {
                        say(&format!(
                            "the link to {} is down ({e}); trying again",
                            self.url
                        ));
                        said_down = true;
                    }
                }
            }
            tokio::select! {
                () = tokio::time::sleep(backoff) => {}
                () = runtime::until_set(stop) => {}
            }
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
        Ok(())
    }

    /// One connection's life. The runs and model calls it takes go on beside
    /// it, in its task, and hand it what to send.
    async fn session(&self, stop: &AtomicBool, say: &(dyn Fn(&str) + Sync)) -> Result<(), Failure> {
        let mut link = Connection::open(&self.url, &self.token)
            .await
            .map_err(Failure::Broken)?;
        let me = link.call(NODE, self.hello()).await?;
        say(&format!(
            "node {} serving {} ({})",
            me["node"].as_str().unwrap_or("?"),
            self.url,
            self.node.access.as_str()
        ));
        let (outbox, mut sending) = unbounded_channel::<(&'static str, Value)>();
        // The runs going on, so a cancel finds its run.
        let runs: Mutex<HashMap<String, Arc<AtomicBool>>> = Mutex::new(HashMap::new());
        let ended = AtomicBool::new(false);
        let (runs, ended) = (&runs, &ended);
        let mut work: FuturesUnordered<Pin<Box<dyn Future<Output = ()> + Send + '_>>> =
            FuturesUnordered::new();
        let result = loop {
            tokio::select! {
                () = runtime::until_set(stop) => break Ok(()),
                Some((method, params)) = sending.recv() => {
                    if let Err(e) = link.send(method, params).await {
                        break Err(Failure::Broken(e));
                    }
                }
                Some(()) = work.next(), if !work.is_empty() => {}
                event = link.next_event() => {
                    let event = match event {
                        Ok(event) => event,
                        Err(e) => break Err(Failure::Broken(e)),
                    };
                    let (data, outbox) = (event.data, outbox.clone());
                    match event.event.as_str() {
                        RUN_EVENT => {
                            let run = data["run"].as_str().unwrap_or("").to_string();
                            let cancel = Arc::new(AtomicBool::new(false));
                            runs.lock().unwrap().insert(run.clone(), cancel.clone());
                            say(&format!(
                                "{run}: {} for {}",
                                data["agent"].as_str().unwrap_or("?"),
                                data["from"].as_str().unwrap_or("?")
                            ));
                            work.push(Box::pin(async move {
                                let done = self.run(&data, &cancel, &outbox).await;
                                say(&format!("{run}: {done}"));
                                runs.lock().unwrap().remove(&run);
                            }));
                        }
                        RUN_CANCEL_EVENT => {
                            if let Some(flag) = data["run"]
                                .as_str()
                                .and_then(|r| runs.lock().unwrap().get(r).cloned())
                            {
                                flag.store(true, Ordering::Relaxed);
                            }
                        }
                        MODEL_CALL_EVENT => {
                            work.push(Box::pin(async move {
                                self.model_call(&data, ended, &outbox).await
                            }));
                        }
                        _ => {}
                    }
                }
            }
        };
        // Whatever is still going on has no one to tell any more.
        ended.store(true, Ordering::Relaxed);
        for flag in runs.lock().unwrap().values() {
            flag.store(true, Ordering::Relaxed);
        }
        while work.next().await.is_some() {}
        link.close().await;
        result
    }

    /// Run one task for someone: the files it brought, the agent, what it
    /// changed. Returns a line for the log.
    async fn run(&self, data: &Value, cancel: &AtomicBool, outbox: &Outbox) -> String {
        let run = data["run"].as_str().unwrap_or("").to_string();
        let fail = |why: String, retryable: bool| {
            let _ = outbox.send((
                RUN_DONE,
                json!({"run": run, "ok": false, "error": why, "retryable": retryable}),
            ));
            format!("failed: {why}")
        };
        let agent = data["agent"].as_str().unwrap_or("");
        let (Some(team), Some(runs)) = (&self.team, &self.node.offers.runs) else {
            return fail("this node offers no runs".into(), false);
        };
        if !runs.agents.iter().any(|a| a == agent) {
            return fail(format!("this node does not offer `{agent}`"), false);
        }
        let files: Vec<snapshot::File> = match serde_json::from_value(data["files"].clone()) {
            Ok(files) => files,
            Err(e) => return fail(format!("the files sent are unreadable: {e}"), false),
        };
        let dir = self.scratch.join("runs").join(&run);
        let written = blocking(|| {
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir)
                .map_err(|e| e.to_string())
                .and_then(|()| snapshot::unpack(&files, &dir))
        });
        if let Err(e) = written {
            return fail(format!("cannot write the files here: {e}"), true);
        }
        let progress = |line: &str| {
            let _ = outbox.send((RUN_PROGRESS, json!({"run": run, "line": line})));
        };
        let Some(_slot) = self.slot(team, agent, cancel, &progress).await else {
            let _ = std::fs::remove_dir_all(&dir);
            return fail("cancelled while it waited for a free slot".into(), false);
        };
        let task = data["task"].as_str().unwrap_or("");
        let context = data["context"].as_str();
        let outcome = team
            .delegate_in(agent, task, context, &dir, &progress, cancel)
            .await;
        let line = match outcome {
            Ok(out) => match blocking(|| snapshot::changes(&files, &dir)) {
                Ok(changes) => {
                    let line = format!("done, {} file(s) changed", changes.len());
                    let _ = outbox.send((
                        RUN_DONE,
                        json!({
                            "run": run,
                            "ok": true,
                            "text": out.text,
                            "model": out.model,
                            "prompt_tokens": out.prompt_tokens,
                            "completion_tokens": out.completion_tokens,
                            "steps": out.stats.steps,
                            "tool_calls": out.stats.tool_calls,
                            "commands": out.stats.commands,
                            "changes": changes,
                        }),
                    ));
                    line
                }
                Err(e) => fail(e, false),
            },
            Err(e) => fail(e.to_string(), e.is_retryable()),
        };
        blocking(|| std::fs::remove_dir_all(&dir)).ok();
        line
    }

    /// Wait for a free slot of `agent`, as its `max_concurrent` allows; none
    /// when cancelled first.
    async fn slot<'w>(
        &'w self,
        team: &Ensemble,
        agent: &str,
        cancel: &AtomicBool,
        progress: &(dyn Fn(&str) + Sync),
    ) -> Option<Slot<'w>> {
        let limit = team.config().agent(agent).map_or(1, |a| a.max_concurrent);
        let mut said = false;
        loop {
            let mut freed = Box::pin(self.freed.notified());
            freed.as_mut().enable();
            {
                let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
                if busy.get(agent).copied().unwrap_or(0) < limit {
                    *busy.entry(agent.to_string()).or_default() += 1;
                    return Some(Slot {
                        worker: self,
                        agent: agent.to_string(),
                    });
                }
            }
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            if !said {
                progress(&format!("{agent}: waiting for a free slot"));
                said = true;
            }
            tokio::select! {
                () = freed => {}
                () = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
        }
    }

    /// Answer one call for a model this node serves, chunk by chunk.
    async fn model_call(&self, data: &Value, ended: &AtomicBool, outbox: &Outbox) {
        let call = data["call"].clone();
        let send = |reply: Value| {
            let mut reply = reply;
            reply["call"] = call.clone();
            let _ = outbox.send((MODEL_REPLY, reply));
        };
        let Some(models) = &self.node.offers.models else {
            return send(json!({"error": "this node serves no models"}));
        };
        let path = data["path"].as_str().unwrap_or("");
        let model = data["body"]["model"].as_str().unwrap_or("");
        if !MODEL_PATHS.contains(&path) {
            return send(json!({ "error": format!("this node does not serve {path}") }));
        }
        if !models.names.iter().any(|n| n == model) {
            return send(
                json!({ "error": format!("this node does not serve the model '{model}'") }),
            );
        }
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(models.timeout_seconds))
            .build()
        {
            Ok(client) => client,
            Err(e) => return send(json!({ "error": format!("no HTTP client: {e}") })),
        };
        let mut req = client
            .post(format!("{}{path}", models.base_url))
            .header("content-type", "application/json")
            .body(data["body"].to_string());
        if let Some(key) = models
            .api_key_env
            .as_deref()
            .and_then(|v| std::env::var(v).ok())
            .filter(|k| !k.is_empty())
        {
            req = req.header("authorization", format!("Bearer {key}"));
        }
        let mut resp = match req.send().await {
            Ok(resp) => resp,
            Err(e) => {
                return send(
                    json!({ "error": format!("the model server at {} did not answer: {e}", models.base_url) }),
                );
            }
        };
        let kind = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json")
            .to_string();
        send(json!({"status": resp.status().as_u16(), "content_type": kind}));
        loop {
            if ended.load(Ordering::Relaxed) {
                return;
            }
            match resp.chunk().await {
                Ok(None) => return send(json!({"done": true})),
                Ok(Some(bytes)) => {
                    for piece in bytes.chunks(CHUNK) {
                        send(json!({ "chunk": snapshot::encode(piece) }));
                    }
                }
                Err(e) => {
                    return send(
                        json!({ "error": format!("the model server's answer broke off: {e}") }),
                    );
                }
            }
        }
    }
}

/// A run's hold on one of its agent's slots, given back when dropped.
struct Slot<'w> {
    worker: &'w Worker,
    agent: String,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut busy = self.worker.busy.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = busy.get_mut(&self.agent) {
            *n = n.saturating_sub(1);
        }
        drop(busy);
        self.worker.freed.notify_waiters();
    }
}

/// Does an agent run only on models from `llm_backends`, never on a login of
/// its CLI's own?
fn on_llm_backends(adapter: &Adapter) -> bool {
    match adapter {
        Adapter::Direct(_) | Adapter::Builtin(_) => true,
        Adapter::OpenCode(o) => matches!(o.model, OpenCodeModel::LlmBackend(_)),
        Adapter::ClaudeCode(_) | Adapter::Codex(_) | Adapter::Node(_) => false,
    }
}
