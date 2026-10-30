//! The Ensemble itself: the agent team, and where work goes.
//!
//! The harness names an **agent**. The router starts that agent's CLI in the
//! project with the task and returns what it did. Nothing a caller passes in
//! may name a model or a harness: the config decides those, and the result
//! reports them.

use crate::config::{Adapter, AgentConfig, CLAUDE_CODE_AGENT, Config, OPENCODE_AGENT};
use crate::external::AgentRun;
use crate::port::Error;
use crate::workspace::Changes;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// What a delegation produced, with enough provenance to audit it.
///
/// Where it ran and the model are *reported*, not accepted as input. A caller
/// cannot steer the work onto a particular model; a reader of the record can
/// still see which one did it.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub agent: String,
    /// The adapter type, and the LLM backend when there is one.
    pub runs_on: String,
    pub model: String,
    pub text: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub stats: AgentStats,
}

/// How much the agent did to get its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStats {
    pub steps: u32,
    pub tool_calls: u32,
    /// Files it changed, so the caller can review them.
    pub changes: Changes,
    /// Shell commands it ran.
    pub commands: Vec<String>,
    /// Another delegation was running at the same time and this agent can
    /// change files. Its changes are found by comparing the project, so
    /// `changes` may include the other run's files.
    pub overlapped: bool,
}

#[derive(Debug)]
pub enum RouteError {
    /// The agent is not in the config. Carries the ones that are, because the
    /// caller is usually a model that guessed a plausible name.
    UnknownAgent { asked: String, known: Vec<String> },
    /// The agent is configured but did not do the work.
    Agent(Error),
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteError::UnknownAgent { asked, known } => write!(
                f,
                "no agent '{asked}' is configured. Available agents: {}.",
                known.join(", ")
            ),
            RouteError::Agent(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RouteError {}

impl RouteError {
    pub fn is_retryable(&self) -> bool {
        match self {
            RouteError::UnknownAgent { .. } => false,
            RouteError::Agent(e) => e.is_retryable(),
        }
    }
}

pub struct Ensemble {
    config: Config,
    workdir: Option<PathBuf>,
    /// Agent runs in progress, and how many have ever started: together they
    /// tell a run whether another one overlapped it.
    active: AtomicUsize,
    started: AtomicU64,
}

impl Ensemble {
    pub fn new(config: Config, workdir: Option<PathBuf>) -> Self {
        Ensemble {
            config,
            workdir,
            active: AtomicUsize::new(0),
            started: AtomicU64::new(0),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn agents(&self) -> Vec<&str> {
        self.config.agent_names()
    }

    fn resolve(&self, name: &str) -> Result<&AgentConfig, RouteError> {
        self.config
            .agent(name)
            .ok_or_else(|| RouteError::UnknownAgent {
                asked: name.to_string(),
                known: self.agents().into_iter().map(str::to_string).collect(),
            })
    }

    /// Where an agent runs, and on which model. Used by `ensemble agents`;
    /// starts nothing.
    pub fn route_of(&self, name: &str) -> Result<(String, &str), RouteError> {
        let a = self.resolve(name)?;
        Ok((a.runs_on(), a.model()))
    }

    /// Send a task to an agent and return what it produced.
    pub fn delegate(
        &self,
        name: &str,
        task: &str,
        context: Option<&str>,
    ) -> Result<Outcome, RouteError> {
        self.delegate_reporting(name, task, context, &|_| {}, &AtomicBool::new(false))
    }

    /// [`Self::delegate`], telling `progress` what the agent is doing as it
    /// goes. Setting `cancel` stops the agent's process.
    pub fn delegate_reporting(
        &self,
        name: &str,
        task: &str,
        context: Option<&str>,
        progress: &dyn Fn(&str),
        cancel: &AtomicBool,
    ) -> Result<Outcome, RouteError> {
        let agent = self.resolve(name)?;
        match &agent.adapter {
            Adapter::ClaudeCode(cc) => self.run_tracked(agent, |dir| {
                crate::external::claude_code::run(agent, cc, task, context, dir, progress, cancel)
            }),
            Adapter::OpenCode(oc) => {
                let backend = oc
                    .llm_backend
                    .as_ref()
                    .map(|b| {
                        self.config.llm_backends.get(b).ok_or_else(|| {
                            RouteError::Agent(Error::Unsupported {
                                backend: b.clone(),
                                detail: format!("LLM backend for agent '{name}' is not defined"),
                            })
                        })
                    })
                    .transpose()?;
                self.run_tracked(agent, |dir| {
                    crate::external::opencode::run(
                        agent, oc, backend, task, context, dir, progress, cancel,
                    )
                })
            }
        }
    }

    /// Run an agent in the project: `run` gets the project directory, and the
    /// result says whether another delegation overlapped it.
    fn run_tracked(
        &self,
        agent: &AgentConfig,
        run: impl FnOnce(&std::path::Path) -> Result<AgentRun, Error>,
    ) -> Result<Outcome, RouteError> {
        let workdir = match &self.workdir {
            Some(dir) => dir.clone(),
            None => std::env::current_dir().map_err(|e| {
                RouteError::Agent(Error::Unsupported {
                    backend: agent.kind().to_string(),
                    detail: format!("no working directory for agent '{}': {e}", agent.name),
                })
            })?,
        };
        let me = self.started.fetch_add(1, Ordering::SeqCst) + 1;
        let others_running = self.active.fetch_add(1, Ordering::SeqCst) > 0;
        let run = run(&workdir);
        let overlapped = others_running || self.started.load(Ordering::SeqCst) != me;
        self.active.fetch_sub(1, Ordering::SeqCst);
        let run = run.map_err(RouteError::Agent)?;
        Ok(Outcome {
            agent: agent.name.clone(),
            runs_on: agent.runs_on(),
            model: run.model,
            text: run.text,
            prompt_tokens: run.usage.prompt_tokens,
            completion_tokens: run.usage.completion_tokens,
            stats: AgentStats {
                steps: run.steps,
                tool_calls: run.tool_calls,
                changes: run.changes,
                commands: run.commands,
                // Changes found by comparing the project can include another run's.
                overlapped: overlapped && agent.can_change_files(),
            },
        })
    }

    /// Check everything the agents run on: each CLI, and each LLM backend in
    /// use, which must answer and serve the models its agents name.
    ///
    /// Keyed by what is checked, not by agent: two agents on one endpoint are
    /// one thing to check and one thing to fix.
    pub fn health(&self) -> Vec<(String, Result<(), Error>)> {
        let mut commands: Vec<(&str, &str)> = Vec::new();
        // Each backend in use, with the models its agents name.
        let mut backends: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
        for a in self.config.agents.values() {
            match &a.adapter {
                Adapter::ClaudeCode(cc) => commands.push((CLAUDE_CODE_AGENT, &cc.command)),
                Adapter::OpenCode(oc) => commands.push((OPENCODE_AGENT, &oc.command)),
            }
            if let Some(b) = a.llm_backend() {
                backends.entry(b).or_default().push(a.model());
            }
        }
        commands.sort_unstable();
        commands.dedup();

        let mut out: Vec<(String, Result<(), Error>)> = commands
            .into_iter()
            .map(|(kind, command)| {
                let result = if kind == OPENCODE_AGENT {
                    crate::external::opencode::health(command)
                } else {
                    crate::external::claude_code::health(command)
                };
                (format!("{kind} ({command})"), result)
            })
            .collect();
        for (name, mut wanted) in backends {
            wanted.sort_unstable();
            wanted.dedup();
            let result = match self.config.llm_backends.get(name) {
                Some(b) => crate::llm_backend::models(b)
                    .and_then(|served| served_all(name, &wanted, &served)),
                None => Err(Error::Unsupported {
                    backend: name.into(),
                    detail: "not defined".into(),
                }),
            };
            out.push((name.to_string(), result));
        }
        out
    }
}

/// Every model in `wanted` is among those the backend `served`.
fn served_all(backend: &str, wanted: &[&str], served: &[String]) -> Result<(), Error> {
    let missing: Vec<&str> = wanted
        .iter()
        .copied()
        .filter(|w| !served.iter().any(|s| s == w))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(Error::Unsupported {
        backend: backend.into(),
        detail: format!(
            "does not serve {}. It serves: {}.",
            missing
                .iter()
                .map(|m| format!("'{m}'"))
                .collect::<Vec<_>>()
                .join(", "),
            if served.is_empty() {
                "(nothing)".to_string()
            } else {
                served.join(", ")
            }
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::workspace::testing::TempDir;
    use std::collections::BTreeMap;

    /// A config whose agents run `program`, a stand-in for the CLI.
    fn ensemble(program: &str, workdir: Option<PathBuf>) -> Ensemble {
        let yaml = format!(
            r#"
version: 1
llm_backends:
  local:
    type: openai-compatible-api
    base_url_env: L
    base_url_default: "http://127.0.0.1:9/v1"
agents:
  looker:
    adapter: {{ type: opencode, model: opencode/big-pickle, command: "{program}" }}
  doer:
    context_window: 32768
    adapter:
      type: opencode
      llm_backend: local
      model: qwen3.5-9b
      tools: [read, edit]
      command: "{program}"
  reviewer:
    adapter: {{ type: claude-code, model: sonnet, command: "{program}" }}
"#
        );
        let cfg =
            config::parse(&yaml, std::path::Path::new("config.yaml"), &BTreeMap::new()).unwrap();
        Ensemble::new(cfg, workdir)
    }

    #[test]
    fn route_of_reports_the_adapter_and_backend_without_starting_anything() {
        let e = ensemble("/nonexistent", None);
        assert_eq!(
            e.route_of("looker").unwrap(),
            ("opencode".into(), "opencode/big-pickle")
        );
        assert_eq!(
            e.route_of("doer").unwrap(),
            ("opencode on local".into(), "qwen3.5-9b")
        );
        assert_eq!(
            e.route_of("reviewer").unwrap(),
            ("claude-code".into(), "sonnet")
        );
    }

    #[test]
    fn an_unknown_agent_lists_the_real_ones_and_is_not_retryable() {
        let e = ensemble("/nonexistent", None);
        let err = e.delegate("nobody", "x", None).unwrap_err();
        assert!(!err.is_retryable());
        let msg = err.to_string();
        assert!(msg.contains("doer, looker, reviewer"), "{msg}");
    }

    #[test]
    fn a_cli_that_is_not_installed_is_reported_by_name() {
        let tmp = TempDir::new();
        let e = ensemble("/nonexistent/opencode", Some(tmp.0.clone()));
        let msg = e.delegate("looker", "x", None).unwrap_err().to_string();
        assert!(msg.contains("/nonexistent/opencode"), "{msg}");
    }

    #[test]
    fn health_checks_each_command_once_and_each_backend_in_use() {
        let e = ensemble("/nonexistent/cli", None);
        let checked: Vec<String> = e.health().into_iter().map(|(what, _)| what).collect();
        assert_eq!(
            checked,
            [
                "claude-code (/nonexistent/cli)",
                "opencode (/nonexistent/cli)",
                "local",
            ]
        );
    }

    #[test]
    fn a_model_the_backend_does_not_serve_is_named_with_what_it_does() {
        let served = vec!["a".to_string(), "b".to_string()];
        assert!(served_all("local", &["a"], &served).is_ok());
        let msg = served_all("local", &["a", "c"], &served)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("does not serve 'c'") && msg.contains("It serves: a, b"),
            "{msg}"
        );
    }
}
