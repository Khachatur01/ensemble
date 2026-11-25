//! What a runner tells the hub server it names about itself.
//! `ensemble-runner serve` keeps it whenever its settings name a hub.
//!
//! It keeps one connection to the hub and says what the runner is
//! (`runner`): its name and its harnesses that may run, each with the models
//! it can drive. So the hub knows which runners are connected, and which may
//! take an account's tasks.

use ensemble_agents::means::{Busy, Means, Pair};
use ensemble_config::{HarnessKind, Runner};
use ensemble_core::runtime;
use ensemble_link::client::{Connection, Failure, Token};
use ensemble_link::protocol::{RUNNER, RUNNER_BUSY};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Why a connection ended when the token was replaced.
const REPLACED: &str = "signed in again with a new token";

/// A runner's link to its hub, ready to connect.
pub struct Worker {
    url: String,
    token: Token,
    runner: Runner,
    /// What it runs tasks on.
    means: Arc<Means>,
}

impl Worker {
    /// A worker for `runner`, running tasks on `means`, reaching the hub at
    /// `url` with `token`, and again with the new one when it is replaced.
    pub fn new(url: &str, token: Token, runner: Runner, means: Arc<Means>) -> Worker {
        Worker {
            url: url.to_string(),
            token,
            runner,
            means,
        }
    }

    /// What this runner says it is when it connects: each harness that may
    /// run with the models it can drive, and its providers' limits.
    fn hello(&self) -> Value {
        let limits: BTreeMap<HarnessKind, u32> = self
            .means
            .harnesses()
            .iter()
            .map(|h| (h.kind, h.max_concurrent))
            .collect();
        let harnesses: Vec<Value> = self
            .means
            .offers()
            .into_iter()
            .map(|(harness, pairs)| {
                json!({
                    "harness": harness.as_str(),
                    "max_concurrent": limits.get(&harness),
                    "models": pairs.iter().map(model).collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({
            "name": self.runner.name,
            "harnesses": harnesses,
            "providers": self.means.provider_limits(),
        })
    }

    /// Serve the hub until `stop` is set, connecting again when the link
    /// breaks. A hub that refuses the runner (a name taken) makes it return,
    /// saying why.
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

    /// One connection's life. It ends when the token is replaced, as
    /// [`REPLACED`].
    async fn session(&self, stop: &AtomicBool, say: &(dyn Fn(&str) + Sync)) -> Result<(), Failure> {
        let mut replaced = self.token.watch();
        replaced.mark_unchanged();
        let mut link = Connection::open(&self.url, &self.token.get())
            .await
            .map_err(Failure::Broken)?;
        let me = link.call(RUNNER, self.hello()).await?;
        let mut busy = self.means.busy();
        busy.mark_changed();
        say(&format!(
            "connected to {} as the runner {}",
            self.url,
            me["runner"].as_str().unwrap_or("?")
        ));
        let result = loop {
            tokio::select! {
                () = runtime::until_set(stop) => break Ok(()),
                Ok(()) = replaced.changed() => break Err(Failure::Broken(REPLACED.into())),
                Ok(()) = busy.changed() => {
                    let now = told(&busy.borrow_and_update());
                    if let Err(e) = link.send(RUNNER_BUSY, now).await {
                        break Err(Failure::Broken(e));
                    }
                }
                // The hub tells this link nothing unasked; it notices it went.
                event = link.next_event() => {
                    if let Err(e) = event {
                        break Err(Failure::Broken(e));
                    }
                }
            }
        };
        link.close().await;
        result
    }
}

/// A model a harness can drive, as the runner tells the hub.
fn model(pair: &Pair) -> Value {
    json!({
        "model": pair.model_name(),
        "provider": pair.provider,
        "tier": pair.tier.as_str(),
        "context_window": pair.context_window,
        "tools": pair.tools,
        "abilities": pair.abilities,
    })
}

/// How busy the runner is, as it tells the hub.
fn told(busy: &Busy) -> Value {
    let harnesses: BTreeMap<&str, u32> = busy
        .harnesses
        .iter()
        .map(|(h, n)| (h.as_str(), *n))
        .collect();
    json!({ "harnesses": harnesses, "providers": busy.providers })
}
