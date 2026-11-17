//! The Ensemble itself: the helper team, and where work goes.
//!
//! The harness names an **agent**. The router starts that agent's CLI in the
//! project with the task and returns what it did. Nothing a caller passes in
//! may name a model or a harness: the config decides those, and the result
//! reports them.

use crate::external::AgentRun;
use ensemble_backends::resolve::{Refusal, Resolver};
use ensemble_config::{
    Adapter, AgentConfig, CLAUDE_CODE_AGENT, Config, LlmBackend, OPENCODE_AGENT, WhenNone,
};
use ensemble_core::port::Error;
use ensemble_core::workspace::Changes;
use std::borrow::Cow;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

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
    /// None of the models in the agent's `try` list can be used now.
    NoBackend {
        agent: String,
        refusals: Vec<Refusal>,
        /// Its `when_none` is `wait`: its tasks wait instead of failing.
        waits: bool,
    },
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
            RouteError::NoBackend {
                agent,
                refusals,
                waits,
            } => write!(
                f,
                "agent '{agent}' has no model it can run on now: {}.{} \
                 `ensemble health` shows what each backend answers.",
                refusals
                    .iter()
                    .map(|r| format!("{} ({}): {}", r.backend, r.model, r.why))
                    .collect::<Vec<_>>()
                    .join("; "),
                if *waits {
                    " Its tasks wait and are tried again."
                } else {
                    ""
                }
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
            // A backend that is off now may be on later.
            RouteError::NoBackend { .. } => true,
            RouteError::Agent(e) => e.is_retryable(),
        }
    }
}

pub struct Ensemble {
    /// The helper team, which a node replaces when its teams change; each run
    /// keeps the one it started with.
    config: RwLock<Arc<Config>>,
    workdir: Option<PathBuf>,
    /// Where tasks, run logs and worktrees go; the per-user state directory
    /// of the working directory unless set.
    state: Option<PathBuf>,
    usage: Option<PathBuf>,
    /// Agent runs in progress, and how many have ever started: together they
    /// tell a run whether another one overlapped it.
    active: AtomicUsize,
    started: AtomicU64,
    /// Picks the model of an agent with a `try` list of more than one.
    resolver: Resolver,
}

impl Ensemble {
    pub fn new(config: Config, workdir: Option<PathBuf>) -> Self {
        Ensemble {
            config: RwLock::new(Arc::new(config)),
            workdir,
            state: None,
            usage: None,
            active: AtomicUsize::new(0),
            started: AtomicU64::new(0),
            resolver: Resolver::default(),
        }
    }

    /// The backends and models of `agent`'s `try` list, in order. A backend
    /// the config does not define is left out; validation refuses one anyway.
    fn candidates<'a>(
        config: &'a Config,
        agent: &'a AgentConfig,
    ) -> Vec<(&'a LlmBackend, &'a str)> {
        agent
            .llm_backends()
            .into_iter()
            .filter_map(|(b, m)| config.llm_backends.get(b).map(|b| (b, m)))
            .collect()
    }

    /// `agent` bound to the model a run uses now, and a note when that is not
    /// its first model.
    fn bind<'a>(
        &self,
        config: &'a Config,
        agent: &'a AgentConfig,
    ) -> Result<(Cow<'a, AgentConfig>, Option<String>), RouteError> {
        let Some(fallback) = agent.fallback() else {
            return Ok((Cow::Borrowed(agent), None));
        };
        let candidates = Self::candidates(config, agent);
        match self.resolver.choose(&candidates) {
            Ok(0) => Ok((Cow::Owned(agent.bound_to(0)), None)),
            Ok(i) => Ok((
                Cow::Owned(agent.bound_to(i)),
                Some(format!("fallback {} of {}", i + 1, candidates.len())),
            )),
            Err(refusals) => Err(RouteError::NoBackend {
                agent: agent.name.clone(),
                refusals,
                waits: fallback.when_none == WhenNone::Wait,
            }),
        }
    }

    /// Can a task of `name` start now? `false` only for an agent whose `try`
    /// list has no usable model and whose `when_none` is `wait`: such a task
    /// waits. Asks the backends, at most once a minute each.
    pub fn backend_ready(&self, name: &str) -> Result<(), String> {
        let config = self.config();
        let Some(agent) = config.agent(name) else {
            return Ok(());
        };
        if agent
            .fallback()
            .is_none_or(|f| f.when_none != WhenNone::Wait || f.candidates.len() < 2)
        {
            return Ok(());
        }
        match self.bind(&config, agent) {
            Err(e @ RouteError::NoBackend { .. }) => Err(e.to_string()),
            _ => Ok(()),
        }
    }

    /// `Some` with a short note when `name`'s first model was found unusable
    /// the last time its backends were asked; asks none of them now.
    pub fn fallback_note(&self, name: &str) -> Option<&'static str> {
        let config = self.config();
        let agent = config.agent(name)?;
        agent.fallback().filter(|f| f.candidates.len() > 1)?;
        match self.resolver.known(&Self::candidates(&config, agent))? {
            Ok(0) => None,
            Ok(_) => Some("on a fallback model"),
            Err(()) => Some("no model reachable"),
        }
    }

    /// The index of the model a run of `name` would use now, asking its
    /// backends, or why there is none. For `ensemble show --probe`.
    pub fn probe(&self, name: &str) -> Result<usize, RouteError> {
        let config = self.config();
        let agent = Self::resolve(&config, name)?;
        self.resolver
            .choose(&Self::candidates(&config, agent))
            .map_err(|refusals| RouteError::NoBackend {
                agent: agent.name.clone(),
                refusals,
                waits: agent
                    .fallback()
                    .is_some_and(|f| f.when_none == WhenNone::Wait),
            })
    }

    /// Keep state in `dir` instead of the per-user state directory.
    pub fn with_state(mut self, dir: PathBuf) -> Self {
        self.state = Some(dir);
        self
    }

    /// Keep usage counters in `dir` instead of the helper team's per-user
    /// directory.
    pub fn with_usage_dir(mut self, dir: PathBuf) -> Self {
        self.usage = Some(dir);
        self
    }

    /// Where usage counters are kept, when not beside the runs: shared by
    /// every project that uses the helper team.
    pub fn usage_dir(&self) -> Option<PathBuf> {
        match &self.usage {
            Some(dir) => Some(dir.clone()),
            None => self
                .config()
                .source
                .exists()
                .then(|| ensemble_core::state::team_dir(&self.config().source).ok())
                .flatten(),
        }
    }

    /// Where tasks, run logs and worktrees are kept.
    pub fn state_dir(&self) -> std::io::Result<PathBuf> {
        match &self.state {
            Some(dir) => Ok(dir.clone()),
            None => ensemble_core::state::dir_for(&self.workdir()?).map_err(std::io::Error::other),
        }
    }

    /// The project directory agents run in.
    pub fn workdir(&self) -> std::io::Result<PathBuf> {
        match &self.workdir {
            Some(dir) => Ok(dir.clone()),
            None => std::env::current_dir(),
        }
    }

    /// The helper team now. A run that started before a change keeps the one
    /// it had.
    pub fn config(&self) -> Arc<Config> {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Replace the helper team: what a node does when its teams change. Runs
    /// going on finish with the one they had; what starts after uses this.
    pub fn set_config(&self, config: Config) {
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(config);
    }

    pub fn agents(&self) -> Vec<String> {
        self.config()
            .agent_names()
            .into_iter()
            .map(String::from)
            .collect()
    }

    fn resolve<'a>(config: &'a Config, name: &str) -> Result<&'a AgentConfig, RouteError> {
        config.agent(name).ok_or_else(|| RouteError::UnknownAgent {
            asked: name.to_string(),
            known: config.agent_names().into_iter().map(String::from).collect(),
        })
    }

    /// Where an agent runs, and on which model. Used by `ensemble agents`;
    /// starts nothing.
    pub fn route_of(&self, name: &str) -> Result<(String, String), RouteError> {
        let config = self.config();
        let a = Self::resolve(&config, name)?;
        Ok((a.runs_on(), a.model().to_string()))
    }

    /// Send a task to an agent and return what it produced.
    pub async fn delegate(
        &self,
        name: &str,
        task: &str,
        context: Option<&str>,
    ) -> Result<Outcome, RouteError> {
        self.delegate_reporting(name, task, context, &|_| {}, &AtomicBool::new(false))
            .await
    }

    /// [`Self::delegate`], telling `progress` what the agent is doing as it
    /// goes. Setting `cancel` stops the agent's process.
    pub async fn delegate_reporting(
        &self,
        name: &str,
        task: &str,
        context: Option<&str>,
        progress: &(dyn Fn(&str) + Sync),
        cancel: &AtomicBool,
    ) -> Result<Outcome, RouteError> {
        let config = self.config();
        let agent = Self::resolve(&config, name)?;
        let workdir = self.workdir().map_err(|e| {
            RouteError::Agent(Error::Unsupported {
                backend: agent.kind().to_string(),
                detail: format!("no working directory for agent '{}': {e}", agent.name),
            })
        })?;
        self.delegate_in(name, task, context, &workdir, progress, cancel)
            .await
    }

    /// [`Self::delegate_reporting`], with the agent working in `workdir`
    /// instead of the project directory, such as a task's git worktree.
    pub async fn delegate_in(
        &self,
        name: &str,
        task: &str,
        context: Option<&str>,
        workdir: &std::path::Path,
        progress: &(dyn Fn(&str) + Sync),
        cancel: &AtomicBool,
    ) -> Result<Outcome, RouteError> {
        let config = self.config();
        let (bound, note) = self.bind(&config, Self::resolve(&config, name)?)?;
        let mut outcome = self
            .run_on(
                &config, &bound, name, task, context, workdir, progress, cancel,
            )
            .await?;
        if let Some(note) = note {
            outcome.runs_on = format!("{} ({note})", outcome.runs_on);
        }
        Ok(outcome)
    }

    /// The LLM backend an agent bound to one model runs on.
    fn backend_of<'a>(
        config: &'a Config,
        name: &str,
        backend: &str,
    ) -> Result<&'a LlmBackend, RouteError> {
        config.llm_backends.get(backend).ok_or_else(|| {
            RouteError::Agent(Error::Unsupported {
                backend: backend.to_string(),
                detail: format!("LLM backend for agent '{name}' is not defined"),
            })
        })
    }

    /// Run `agent`, already bound to one model, once.
    #[allow(clippy::too_many_arguments)]
    async fn run_on(
        &self,
        config: &Config,
        agent: &AgentConfig,
        name: &str,
        task: &str,
        context: Option<&str>,
        workdir: &std::path::Path,
        progress: &(dyn Fn(&str) + Sync),
        cancel: &AtomicBool,
    ) -> Result<Outcome, RouteError> {
        use crate::external::{builtin, claude_code, codex, direct, node, opencode};
        let run = |run| self.run_tracked(agent, workdir, run);
        match &agent.adapter {
            Adapter::ClaudeCode(cc) => {
                run(Box::pin(claude_code::run(
                    agent, cc, task, context, workdir, progress, cancel,
                )))
                .await
            }
            Adapter::Codex(cx) => {
                run(Box::pin(codex::run(
                    agent, cx, task, context, workdir, progress, cancel,
                )))
                .await
            }
            Adapter::Direct(la) => {
                let backend = Self::backend_of(config, name, la.backend())?;
                run(Box::pin(direct::run(
                    agent, la, backend, task, context, progress, cancel,
                )))
                .await
            }
            Adapter::Builtin(ba) => {
                let backend = Self::backend_of(config, name, ba.backend())?;
                run(Box::pin(builtin::run(
                    agent, ba, backend, task, context, workdir, progress, cancel,
                )))
                .await
            }
            Adapter::OpenCode(oc) => {
                let backend = oc
                    .llm_backend()
                    .map(|b| Self::backend_of(config, name, b))
                    .transpose()?;
                run(Box::pin(opencode::run(
                    agent, oc, backend, task, context, workdir, progress, cancel,
                )))
                .await
            }
            Adapter::Node(na) => {
                run(Box::pin(node::run(
                    agent, na, task, context, workdir, progress, cancel,
                )))
                .await
            }
        }
    }

    /// Run an agent: `run` is its run, in `workdir`; the result says whether
    /// another delegation in the project overlapped it.
    async fn run_tracked(
        &self,
        agent: &AgentConfig,
        workdir: &std::path::Path,
        run: Pin<Box<dyn Future<Output = Result<AgentRun, Error>> + Send + '_>>,
    ) -> Result<Outcome, RouteError> {
        // Only runs in the project itself can see each other's changes; one in
        // a worktree of its own neither counts nor is counted.
        let in_project = self.workdir().is_ok_and(|dir| dir == workdir);
        let started = std::time::SystemTime::now();
        let clock = std::time::Instant::now();
        let run = if in_project {
            let me = self.started.fetch_add(1, Ordering::SeqCst) + 1;
            let others_running = self.active.fetch_add(1, Ordering::SeqCst) > 0;
            let run = run.await;
            let overlapped = others_running || self.started.load(Ordering::SeqCst) != me;
            self.active.fetch_sub(1, Ordering::SeqCst);
            run.map(|r| (r, overlapped))
        } else {
            run.await.map(|r| (r, false))
        };
        let seconds = clock.elapsed().as_secs_f64();
        let started = started
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let (run, overlapped) = match run {
            Ok(done) => done,
            Err(e) => {
                self.record(ensemble_core::records::Record {
                    agent: agent.name.clone(),
                    model: Some(agent.model().to_string()),
                    started,
                    seconds,
                    prompt_tokens: None,
                    completion_tokens: None,
                    steps: 0,
                    tool_calls: 0,
                    ok: false,
                    error: Some(e.to_string().chars().take(200).collect()),
                });
                return Err(RouteError::Agent(e));
            }
        };
        // No tokens reported is not the same as none spent.
        let reported = run.usage.prompt_tokens > 0 || run.usage.completion_tokens > 0;
        self.record(ensemble_core::records::Record {
            agent: agent.name.clone(),
            model: Some(run.model.clone()).filter(|m| !m.is_empty()),
            started,
            seconds,
            prompt_tokens: reported.then_some(u64::from(run.usage.prompt_tokens)),
            completion_tokens: reported.then_some(u64::from(run.usage.completion_tokens)),
            steps: run.steps,
            tool_calls: run.tool_calls,
            ok: true,
            error: None,
        });
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

    /// Where records of runs are kept: with the usage counters, shared by every
    /// project that uses the helper team, else in the state directory.
    pub fn records_dir(&self) -> Option<PathBuf> {
        self.usage_dir().or_else(|| self.state_dir().ok())
    }

    fn record(&self, record: ensemble_core::records::Record) {
        if let Some(dir) = self.records_dir() {
            ensemble_core::records::append(&dir, &record);
        }
    }

    /// Check everything the agents run on: each CLI, and each LLM backend in
    /// use, which must answer and serve the models its agents name.
    ///
    /// Keyed by what is checked, not by agent: two agents on one endpoint are
    /// one thing to check and one thing to fix.
    pub fn health(&self) -> Vec<(String, Result<(), Error>)> {
        let config = self.config();
        let mut commands: Vec<(&str, &str)> = Vec::new();
        // Each backend in use, with the models its agents name.
        let mut backends: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
        for a in config.agents.values() {
            match &a.adapter {
                Adapter::ClaudeCode(cc) => commands.push((CLAUDE_CODE_AGENT, &cc.command)),
                Adapter::OpenCode(oc) => commands.push((OPENCODE_AGENT, &oc.command)),
                Adapter::Codex(cx) => commands.push((ensemble_config::CODEX_AGENT, &cx.command)),
                // No program: its backend, checked below, is all it needs.
                Adapter::Direct(_) | Adapter::Builtin(_) => {}
                // Checked below, against its hub.
                Adapter::Node(_) => {}
            }
            for (b, model) in a.llm_backends() {
                backends.entry(b).or_default().push(model);
            }
        }
        commands.sort_unstable();
        commands.dedup();

        let mut out: Vec<(String, Result<(), Error>)> = commands
            .into_iter()
            .map(|(kind, command)| {
                let result = if kind == OPENCODE_AGENT {
                    crate::external::opencode::health(command)
                } else if kind == ensemble_config::CODEX_AGENT {
                    crate::external::codex::health(command)
                } else {
                    crate::external::claude_code::health(command)
                };
                (format!("{kind} ({command})"), result)
            })
            .collect();
        for (name, mut wanted) in backends {
            wanted.sort_unstable();
            wanted.dedup();
            let result = match config.llm_backends.get(name) {
                Some(b) => ensemble_backends::models::models(b)
                    .and_then(|served| served_all(name, &wanted, &served)),
                None => Err(Error::Unsupported {
                    backend: name.into(),
                    detail: "not defined".into(),
                }),
            };
            out.push((name.to_string(), result));
        }
        for a in config.agents.values() {
            if let Adapter::Node(na) = &a.adapter {
                out.push((
                    format!("{} ({})", a.runs_on(), na.agent),
                    crate::external::node::health(na),
                ));
            }
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
    use ensemble_config as config;
    use ensemble_core::workspace::testing::TempDir;
    use std::collections::BTreeMap;

    /// A config whose agents run `program`, a stand-in for the CLI.
    fn ensemble(program: &str, workdir: Option<PathBuf>) -> Ensemble {
        let yaml = format!(
            r#"
version: 1
llm_backends:
  local:
    type: openai-compatible-api
    base_url: "http://127.0.0.1:9/v1"
    timeout_seconds: 600
limits: {{}}
gates: {{}}
agents:
  looker:
    max_seconds: 1800
    max_concurrent: 2
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: opencode
      model: {{ type: provider, name: opencode/big-pickle }}
      tools: [read, glob, grep, list]
      system_prompt: append
      max_steps: 30
      command: "{program}"
  doer:
    max_seconds: 1800
    max_concurrent: 2
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: opencode
      model: {{ type: llm_backend, try: [{{ backend: local, name: qwen3.5-9b, context_window: 32768 }}], when_none: fail }}
      tools: [read, edit]
      system_prompt: append
      max_steps: 30
      command: "{program}"
  reviewer:
    max_seconds: 1800
    max_concurrent: 2
    isolation: shared
    budget:
      type: unlimited
    adapter:
      type: claude-code
      model: sonnet
      tools: [Read, Grep, Glob]
      permission_mode: dontAsk
      max_turns: 30
      command: "{program}"
"#
        );
        let cfg =
            config::parse(&yaml, std::path::Path::new("config.yaml"), &BTreeMap::new()).unwrap();
        Ensemble::new(cfg, workdir)
    }

    /// A model server that serves `models` and answers every chat request
    /// with `answer`, as many times as it is asked.
    fn model_server(models: &[&str], answer: &str) -> String {
        ensemble_backends::testing::model_server(models, &[answer]).url
    }

    use ensemble_backends::testing::nowhere;

    /// A `direct` agent that tries `home`, then `central`, in a fresh folder.
    fn with_fallback(home: &str, central: &str, when_none: &str) -> (Ensemble, TempDir) {
        let yaml = format!(
            "version: 1\nllm_backends:\n  home: {{ type: openai-compatible-api, base_url: \"{home}\", timeout_seconds: 5 }}\n  central: {{ type: openai-compatible-api, base_url: \"{central}\", timeout_seconds: 5 }}\nlimits: {{}}\ngates: {{}}\nagents:\n  summariser:\n    max_seconds: 30\n    max_concurrent: 1\n    isolation: shared\n    budget: {{ type: unlimited }}\n    adapter:\n      type: direct\n      try:\n        - {{ backend: home, model: small }}\n        - {{ backend: central, model: big }}\n      when_none: {when_none}\n      max_tokens: 10\n"
        );
        let cfg =
            config::parse(&yaml, std::path::Path::new("config.yaml"), &BTreeMap::new()).unwrap();
        let tmp = TempDir::new();
        let e = Ensemble::new(cfg, Some(tmp.0.clone())).with_state(tmp.0.join("state"));
        (e, tmp)
    }

    #[test]
    fn a_run_falls_back_to_the_next_model_and_says_so() {
        let (e, _tmp) = with_fallback(&nowhere(), &model_server(&["big"], "from central"), "fail");
        let out = crate::runtime::block_on(e.delegate("summariser", "sum it", None)).unwrap();
        assert_eq!(out.text, "from central");
        assert_eq!(out.model, "big");
        assert_eq!(out.runs_on, "direct on central (fallback 2 of 2)");
        assert_eq!(e.fallback_note("summariser"), Some("on a fallback model"));
        assert_eq!(e.probe("summariser").unwrap(), 1);
    }

    #[test]
    fn the_first_model_is_used_when_its_backend_serves_it() {
        let home = model_server(&["small"], "from home");
        let (e, _tmp) = with_fallback(&home, &nowhere(), "fail");
        let out = crate::runtime::block_on(e.delegate("summariser", "sum it", None)).unwrap();
        assert_eq!(
            (out.text.as_str(), out.runs_on.as_str()),
            ("from home", "direct on home")
        );
        assert_eq!(e.fallback_note("summariser"), None);
    }

    #[test]
    fn with_no_usable_model_a_run_fails_or_its_tasks_wait() {
        let not_served = model_server(&["other"], "x");
        let (e, _tmp) = with_fallback(&nowhere(), &not_served, "fail");
        let err = crate::runtime::block_on(e.delegate("summariser", "sum it", None)).unwrap_err();
        let RouteError::NoBackend {
            refusals, waits, ..
        } = &err
        else {
            panic!("{err:?}");
        };
        assert!(!waits);
        assert_eq!(refusals.len(), 2);
        let said = err.to_string();
        assert!(
            said.contains("home (small)") && said.contains("central (big): does not serve it"),
            "{said}"
        );
        assert!(err.is_retryable());
        assert!(
            e.backend_ready("summariser").is_ok(),
            "a failing agent's tasks run and fail"
        );

        let (e, _tmp) = with_fallback(&nowhere(), &not_served, "wait");
        let why = e.backend_ready("summariser").unwrap_err();
        assert!(why.contains("Its tasks wait"), "{why}");
        assert_eq!(e.fallback_note("summariser"), Some("no model reachable"));
    }

    #[test]
    fn route_of_reports_the_adapter_and_backend_without_starting_anything() {
        let e = ensemble("/nonexistent", None);
        assert_eq!(
            e.route_of("looker").unwrap(),
            ("opencode".into(), "opencode/big-pickle".into())
        );
        assert_eq!(
            e.route_of("doer").unwrap(),
            ("opencode on local".into(), "qwen3.5-9b".into())
        );
        assert_eq!(
            e.route_of("reviewer").unwrap(),
            ("claude-code".into(), "sonnet".into())
        );
    }

    #[test]
    fn an_unknown_agent_lists_the_real_ones_and_is_not_retryable() {
        let e = ensemble("/nonexistent", None);
        let err = crate::runtime::block_on(e.delegate("nobody", "x", None)).unwrap_err();
        assert!(!err.is_retryable());
        let msg = err.to_string();
        assert!(msg.contains("doer, looker, reviewer"), "{msg}");
    }

    #[test]
    fn a_cli_that_is_not_installed_is_reported_by_name() {
        let tmp = TempDir::new();
        let e = ensemble("/nonexistent/opencode", Some(tmp.0.clone()));
        let msg = crate::runtime::block_on(e.delegate("looker", "x", None))
            .unwrap_err()
            .to_string();
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
