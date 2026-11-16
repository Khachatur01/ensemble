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
//! Each runs on a thread of its own. Only the connection's own thread uses
//! the socket: the others hand it what to send.
//!
//! A shared node works for other people, so an agent it offers must run on
//! models from `llm_backends`: an agent CLI's own login is its owner's, and
//! is never used for someone else's work.

use ensemble_agents::router::Ensemble;
use ensemble_config::{Access, Adapter, Config, Node, OpenCodeModel};
use ensemble_core::snapshot;
use ensemble_link::client::{Connection, Failure};
use ensemble_link::protocol::{
    MODEL_CALL_EVENT, MODEL_REPLY, NODE, RUN_CANCEL_EVENT, RUN_DONE, RUN_EVENT, RUN_PROGRESS,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long the connection waits for an event before it sends what its
/// threads handed it: what a model's streamed answer may lag by.
const TICK: Duration = Duration::from_millis(20);
/// The most of a model's answer sent in one chunk.
const CHUNK: usize = 16 * 1024;
/// The paths of a model server a call may ask for.
const MODEL_PATHS: [&str; 2] = ["/chat/completions", "/completions"];

/// What a thread hands the connection to send.
type Outbox = Sender<(&'static str, Value)>;

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
    freed: Condvar,
}

impl Worker {
    /// A worker for `node`, reaching the hub at `url` with `token`. `team` is
    /// its helper team, loaded; each agent offered
    /// must be in it, and on a shared node run on `llm_backends`.
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
                            "offers.runs: the team {} has no agent '{name}'. Its agents: {}.",
                            node.team.as_deref().unwrap_or(Path::new("?")).display(),
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
            freed: Condvar::new(),
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
    pub fn work(&self, stop: &AtomicBool, say: &(dyn Fn(&str) + Sync)) -> Result<(), String> {
        let mut backoff = Duration::from_secs(1);
        let mut said_down = false;
        while !stop.load(Ordering::Relaxed) {
            match self.session(stop, say) {
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
            let until = Instant::now() + backoff;
            while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
        Ok(())
    }

    /// One connection's life.
    fn session(&self, stop: &AtomicBool, say: &(dyn Fn(&str) + Sync)) -> Result<(), Failure> {
        let mut link = Connection::open(&self.url, &self.token).map_err(Failure::Broken)?;
        let me = link.call(NODE, self.hello())?;
        say(&format!(
            "node {} serving {} ({})",
            me["node"].as_str().unwrap_or("?"),
            self.url,
            self.node.access.as_str()
        ));
        let (outbox, sending) = channel::<(&'static str, Value)>();
        // The runs going on, so a cancel finds its run.
        let runs: Mutex<HashMap<String, Arc<AtomicBool>>> = Mutex::new(HashMap::new());
        let ended = AtomicBool::new(false);
        let result = std::thread::scope(|scope| {
            let result = (|| -> Result<(), Failure> {
                while !stop.load(Ordering::Relaxed) {
                    while let Ok((method, params)) = sending.try_recv() {
                        link.send(method, params).map_err(Failure::Broken)?;
                    }
                    let Some(event) = link.next_event(TICK).map_err(Failure::Broken)? else {
                        continue;
                    };
                    let data = event.data;
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
                            let (outbox, runs) = (outbox.clone(), &runs);
                            scope.spawn(move || {
                                let done = self.run(&data, &cancel, &outbox);
                                say(&format!("{run}: {done}"));
                                runs.lock().unwrap().remove(&run);
                            });
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
                            let (outbox, ended) = (outbox.clone(), &ended);
                            scope.spawn(move || self.model_call(&data, ended, &outbox));
                        }
                        _ => {}
                    }
                }
                Ok(())
            })();
            // Whatever is still going on has no one to tell any more.
            ended.store(true, Ordering::Relaxed);
            for flag in runs.lock().unwrap().values() {
                flag.store(true, Ordering::Relaxed);
            }
            result
        });
        link.close();
        result
    }

    /// Run one task for someone: the files it brought, the agent, what it
    /// changed. Returns a line for the log.
    fn run(&self, data: &Value, cancel: &AtomicBool, outbox: &Outbox) -> String {
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
        let _ = std::fs::remove_dir_all(&dir);
        if let Err(e) = std::fs::create_dir_all(&dir)
            .map_err(|e| e.to_string())
            .and_then(|()| snapshot::unpack(&files, &dir))
        {
            return fail(format!("cannot write the files here: {e}"), true);
        }
        let progress = |line: &str| {
            let _ = outbox.send((RUN_PROGRESS, json!({"run": run, "line": line})));
        };
        let Some(_slot) = self.slot(team, agent, cancel, &progress) else {
            let _ = std::fs::remove_dir_all(&dir);
            return fail("cancelled while it waited for a free slot".into(), false);
        };
        let task = data["task"].as_str().unwrap_or("");
        let context = data["context"].as_str();
        let outcome = ensemble_agents::runtime::block_on(
            team.delegate_in(agent, task, context, &dir, &progress, cancel),
        );
        let line = match outcome {
            Ok(out) => match snapshot::changes(&files, &dir) {
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
        let _ = std::fs::remove_dir_all(&dir);
        line
    }

    /// Wait for a free slot of `agent`, as its `max_concurrent` allows; none
    /// when cancelled first.
    fn slot<'w>(
        &'w self,
        team: &Ensemble,
        agent: &str,
        cancel: &AtomicBool,
        progress: &(dyn Fn(&str) + Sync),
    ) -> Option<Slot<'w>> {
        let limit = team.config().agent(agent).map_or(1, |a| a.max_concurrent);
        let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        let mut said = false;
        while busy.get(agent).copied().unwrap_or(0) >= limit {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            if !said {
                progress(&format!("{agent}: waiting for a free slot"));
                said = true;
            }
            busy = self
                .freed
                .wait_timeout(busy, Duration::from_millis(200))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        *busy.entry(agent.to_string()).or_default() += 1;
        Some(Slot {
            worker: self,
            agent: agent.to_string(),
        })
    }

    /// Answer one call for a model this node serves, chunk by chunk.
    fn model_call(&self, data: &Value, ended: &AtomicBool, outbox: &Outbox) {
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
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(models.timeout_seconds)))
            .http_status_as_error(false)
            .build()
            .into();
        let mut req = agent
            .post(format!("{}{path}", models.base_url))
            .header("content-type", "application/json");
        if let Some(key) = models
            .api_key_env
            .as_deref()
            .and_then(|v| std::env::var(v).ok())
            .filter(|k| !k.is_empty())
        {
            req = req.header("authorization", &format!("Bearer {key}"));
        }
        let resp = match req.send(data["body"].to_string()) {
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
        let mut body = resp.into_body().into_reader();
        let mut buf = vec![0u8; CHUNK];
        loop {
            if ended.load(Ordering::Relaxed) {
                return;
            }
            match body.read(&mut buf) {
                Ok(0) => return send(json!({"done": true})),
                Ok(n) => send(json!({ "chunk": snapshot::encode(&buf[..n]) })),
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
        self.worker.freed.notify_all();
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
