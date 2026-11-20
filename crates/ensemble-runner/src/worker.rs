//! What a runner tells the hub server it names about itself.
//! `ensemble-runner serve` keeps it whenever its `runner.yaml` names a hub.
//!
//! It keeps one connection to the hub and says what the runner is
//! (`runner`): its name, whom it works for (`works_for`), every agent of its
//! teams and the agents it lends other accounts, and says it again when its
//! teams are reloaded. So the hub knows which runners are connected, with
//! which agents, and which may take an account's tasks.
//!
//! A runner that works for the hub works for other people, so the agents it
//! lends are those of its teams that run on models of LLM backends
//! ([`ensemble_config::AgentConfig::lendable`]), worked out from its teams
//! each time it says what it is, never listed by hand: an agent CLI's own
//! login is its owner's, and is never used for someone else's work.

use ensemble_config::{Config, Runner, WorksFor};
use ensemble_core::runtime;
use ensemble_link::client::{Connection, Failure, Token};
use ensemble_link::protocol::RUNNER;
use serde_json::{Value, json};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

/// Why a connection ended when the token was replaced.
const REPLACED: &str = "signed in again with a new token";

/// The agents `runner` lends other accounts, of its teams `config`: those on
/// LLM backends when it works for the hub, and none when it works for its
/// owner.
pub fn lends(runner: &Runner, config: &Config) -> Vec<String> {
    match runner.works_for {
        WorksFor::Hub => config
            .lendable_agents()
            .into_iter()
            .map(String::from)
            .collect(),
        WorksFor::Owner => Vec::new(),
    }
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
    /// loaded as one.
    pub fn new(url: &str, token: Token, runner: Runner, team: Config) -> Worker {
        Worker {
            url: url.to_string(),
            token,
            runner,
            team: RwLock::new(team),
            reloaded: Notify::new(),
        }
    }

    /// Use `config`, the runner's teams read again, and tell the hub what it
    /// has now.
    pub fn reload(&self, config: &Config) {
        *self.team.write().unwrap_or_else(|e| e.into_inner()) = config.clone();
        self.reloaded.notify_one();
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
        json!({
            "name": self.runner.name,
            "works_for": self.runner.works_for.as_str(),
            "agents": agents,
            "lends": lends(&self.runner, &team),
        })
    }

    /// Serve the hub until `stop` is set, connecting again when the link
    /// breaks. A hub that refuses the runner (one that works for the hub
    /// without `host`, a name taken) makes it return, saying why.
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
        say(&format!(
            "connected to {} as the runner {} (works for {})",
            self.url,
            me["runner"].as_str().unwrap_or("?"),
            self.runner.works_for.as_str()
        ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn a_runner_lends_its_agents_on_llm_backends_only_when_it_works_for_the_hub() {
        let team =
            ensemble_config::parse(ensemble_config::SAMPLE, Path::new("config.yaml")).unwrap();
        let runner = |works_for: &str| {
            let mut settings = ensemble_config::RunnerSettings::starting("box", "/code");
            settings.hub = Some("ws://hub".into());
            settings.works_for = serde_json::from_value(serde_json::json!(works_for)).unwrap();
            settings
                .resolve(
                    &ensemble_core::state::RunnerDir::new(""),
                    &std::collections::BTreeMap::new(),
                )
                .unwrap()
        };
        // Its agents on OpenCode with a provider, Codex and Claude Code use
        // their CLI's own login: never lent.
        assert_eq!(
            lends(&runner("hub"), &team),
            ["coder", "patcher", "summariser"]
        );
        assert!(lends(&runner("owner"), &team).is_empty());
    }
}
