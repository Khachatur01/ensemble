//! What a runner tells the hub server it names about itself, and the model
//! calls it answers for the people of the hub. `ensemble-runner serve` keeps
//! it whenever its `runner.yaml` names a hub.
//!
//! It keeps one connection to the hub and says what the runner is
//! (`runner`): its name, whom it works for, every agent of its teams, the
//! agents it lends others (`offers.runs`) and the models it serves
//! (`offers.models`), and says it again when its teams are reloaded. So the
//! hub knows which of an account's runners are connected, with which agents.
//! Then it does what comes:
//!
//! - a `model_call` event: the request goes to the model server in
//!   `offers.models`, and its answer goes back in chunks as it comes.
//!
//! Each call goes on beside the connection, in its task. Only the connection
//! uses the socket: the others hand it what to send.
//!
//! A shared runner works for other people, so an agent it lends must run on
//! models from `llm_backends`: an agent CLI's own login is its owner's, and
//! is never used for someone else's work.

use ensemble_config::{Access, Adapter, Config, OpenCodeModel, Runner};
use ensemble_core::runtime;
use ensemble_link::client::{Connection, Failure, Token};
use ensemble_link::protocol::{MODEL_CALL_EVENT, MODEL_REPLY, RUNNER};
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

/// The most of a model's answer sent in one chunk.
const CHUNK: usize = 16 * 1024;
/// The paths of a model server a call may ask for.
const MODEL_PATHS: [&str; 2] = ["/chat/completions", "/completions"];
/// Why a connection ended when the token was replaced.
const REPLACED: &str = "signed in again with a new token";

/// What a model call hands the connection to send.
type Outbox = UnboundedSender<(&'static str, Value)>;

/// Every agent `runner` lends is in `config`, and a shared runner lends only
/// agents on `llm_backends`.
fn check_offers(runner: &Runner, config: &Config) -> Result<(), String> {
    let Some(runs) = &runner.offers.runs else {
        return Ok(());
    };
    for name in &runs.agents {
        let Some(agent) = config.agent(name) else {
            return Err(format!(
                "offers.runs: no team of this runner has the agent '{name}'. Its agents: {}.",
                config.agent_names().join(", ")
            ));
        };
        if runner.access == Access::Shared && !on_llm_backends(&agent.adapter) {
            return Err(format!(
                "offers.runs: '{name}' runs on {}, which uses an agent CLI's own login or provider, and a shared runner never lends that; offer an agent of type opencode on llm_backends, builtin, or direct",
                agent.runs_on()
            ));
        }
    }
    Ok(())
}

/// A runner's link to its hub, ready to connect.
pub struct Worker {
    url: String,
    token: Token,
    runner: Runner,
    /// The runner's teams, as one: the agents it says it has. Replaced when
    /// they are reloaded.
    team: RwLock<Config>,
    /// Told when the teams are reloaded, to say again what the runner has.
    reloaded: Notify,
}

impl Worker {
    /// A worker for `runner`, reaching the hub at `url` with `token`, and
    /// again with the new one when it is replaced. `team` is its teams,
    /// loaded as one; each agent it lends must be in it, and on a shared
    /// runner run on `llm_backends`.
    pub fn new(url: &str, token: Token, runner: Runner, team: Config) -> Result<Worker, String> {
        check_offers(&runner, &team)?;
        Ok(Worker {
            url: url.to_string(),
            token,
            runner,
            team: RwLock::new(team),
            reloaded: Notify::new(),
        })
    }

    /// Use `config`, the runner's teams read again, and tell the hub what it
    /// has now; refused, and nothing changes, when it no longer has an agent
    /// the runner lends.
    pub fn reload(&self, config: &Config) -> Result<(), String> {
        check_offers(&self.runner, config)?;
        *self.team.write().unwrap_or_else(|e| e.into_inner()) = config.clone();
        self.reloaded.notify_one();
        Ok(())
    }

    /// What this runner says it is when it connects, and when its teams
    /// change.
    fn hello(&self) -> Value {
        let team = self.team.read().unwrap_or_else(|e| e.into_inner());
        let agents: Vec<Value> = team
            .agents
            .values()
            .map(|agent| {
                let about = agent
                    .prompt
                    .as_deref()
                    .and_then(|p| p.lines().map(str::trim).find(|l| !l.is_empty()))
                    .unwrap_or("");
                json!({"name": agent.name, "about": about})
            })
            .collect();
        let offers = self.runner.offers.runs.as_ref();
        let models = self.runner.offers.models.as_ref();
        json!({
            "name": self.runner.name,
            "access": self.runner.access.as_str(),
            "agents": agents,
            "offers": offers.map(|r| r.agents.clone()).unwrap_or_default(),
            "models": models.map(|m| m.names.clone()).unwrap_or_default(),
            "model_timeout_seconds": models.map(|m| m.timeout_seconds),
        })
    }

    /// Serve the hub until `stop` is set, connecting again when the link
    /// breaks. A hub that refuses the runner (a shared runner without `host`,
    /// a name taken) makes it return, saying why.
    pub async fn work(&self, stop: &AtomicBool, say: &(dyn Fn(&str) + Sync)) -> Result<(), String> {
        let mut backoff = Duration::from_secs(1);
        let mut said_down = false;
        let mut replaced = self.token.watch();
        while !stop.load(Ordering::Relaxed) {
            replaced.mark_unchanged();
            match self.session(stop, say).await {
                Ok(()) => return Ok(()),
                Err(Failure::Hub(e)) => return Err(e),
                Err(Failure::Broken(e)) if e == REPLACED => {
                    say(&format!("{REPLACED}: connecting to {} with it", self.url));
                    (backoff, said_down) = (Duration::from_secs(1), false);
                    continue;
                }
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
                // Signed in again: try the new token at once.
                _ = replaced.changed() => {
                    backoff = Duration::from_secs(1);
                    continue;
                }
            }
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
        Ok(())
    }

    /// One connection's life. The model calls it takes go on beside it, in
    /// its task, and hand it what to send. It ends when the token is
    /// replaced, as [`REPLACED`].
    async fn session(&self, stop: &AtomicBool, say: &(dyn Fn(&str) + Sync)) -> Result<(), Failure> {
        let mut replaced = self.token.watch();
        replaced.mark_unchanged();
        let mut link = Connection::open(&self.url, &self.token.get())
            .await
            .map_err(Failure::Broken)?;
        let me = link.call(RUNNER, self.hello()).await?;
        say(&format!(
            "connected to {} as the runner {} ({})",
            self.url,
            me["runner"].as_str().unwrap_or("?"),
            self.runner.access.as_str()
        ));
        let (outbox, mut sending) = unbounded_channel::<(&'static str, Value)>();
        let ended = AtomicBool::new(false);
        let ended = &ended;
        let mut work: FuturesUnordered<Pin<Box<dyn Future<Output = ()> + Send + '_>>> =
            FuturesUnordered::new();
        let result = loop {
            tokio::select! {
                () = runtime::until_set(stop) => break Ok(()),
                Ok(()) = replaced.changed() => break Err(Failure::Broken(REPLACED.into())),
                // Its teams changed: the hub hears what it has now.
                () = self.reloaded.notified() => {
                    if let Err(e) = link.call(RUNNER, self.hello()).await {
                        break Err(e);
                    }
                }
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
                    if event.event == MODEL_CALL_EVENT {
                        let (data, outbox) = (event.data, outbox.clone());
                        work.push(Box::pin(async move {
                            self.model_call(&data, ended, &outbox).await
                        }));
                    }
                }
            }
        };
        // Whatever is still going on has no one to tell any more.
        ended.store(true, Ordering::Relaxed);
        while work.next().await.is_some() {}
        link.close().await;
        result
    }

    /// Answer one call for a model this runner serves, chunk by chunk.
    async fn model_call(&self, data: &Value, ended: &AtomicBool, outbox: &Outbox) {
        let call = data["call"].clone();
        let send = |reply: Value| {
            let mut reply = reply;
            reply["call"] = call.clone();
            let _ = outbox.send((MODEL_REPLY, reply));
        };
        let Some(models) = &self.runner.offers.models else {
            return send(json!({"error": "this runner serves no models"}));
        };
        let path = data["path"].as_str().unwrap_or("");
        let model = data["body"]["model"].as_str().unwrap_or("");
        if !MODEL_PATHS.contains(&path) {
            return send(json!({ "error": format!("this runner does not serve {path}") }));
        }
        if !models.names.iter().any(|n| n == model) {
            return send(
                json!({ "error": format!("this runner does not serve the model '{model}'") }),
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
                        send(json!({ "chunk": ensemble_link::base64::encode(piece) }));
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

/// Does an agent run only on models from `llm_backends`, never on a login of
/// its CLI's own?
fn on_llm_backends(adapter: &Adapter) -> bool {
    match adapter {
        Adapter::Direct(_) | Adapter::Builtin(_) => true,
        Adapter::OpenCode(o) => matches!(o.model, OpenCodeModel::LlmBackend(_)),
        Adapter::ClaudeCode(_) | Adapter::Codex(_) => false,
    }
}
