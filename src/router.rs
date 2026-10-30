//! The Ensemble itself: the agent team, and where work goes.
//!
//! The harness names a **agent**. The router turns that into a backend, a model
//! and a prompt, and returns the answer. That indirection is the product
//! requirement — "users should not need to select individual models or know
//! which backends are being used" — so nothing a caller passes in may name a
//! model or a backend, and nothing the router returns has to be interpreted
//! differently depending on which one answered.

use crate::config::{AgentConfig, AgentTool, Config};
use crate::port::{Error, Message, ModelPort, Request, Response};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// What a delegation produced, with enough provenance to audit it.
///
/// The backend and model are *reported*, not accepted as input. A caller cannot
/// steer the work onto a particular model; a reader of the record can still see
/// which one did it.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub agent: String,
    pub backend: String,
    pub model: String,
    pub text: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Set when the agent ran as an agent: how much it did to get there.
    pub stats: Option<AgentStats>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStats {
    pub steps: u32,
    pub tool_calls: u32,
    /// Old tool results the agent lost to stay within its context budget.
    pub trimmed: u32,
    /// Files it changed, so the caller can review them.
    pub changes: crate::agent::tools::Changes,
    /// `bash` commands it ran.
    pub commands: Vec<String>,
    /// Another delegation was running at the same time and this agent has
    /// `bash`. Its changes are found by comparing the project, so `changes`
    /// may include the other run's files.
    pub overlapped: bool,
}

#[derive(Debug)]
pub enum RouteError {
    /// The agent is not in the config. Carries the ones that are, because the
    /// caller is usually a model that guessed a plausible name.
    UnknownAgent { asked: String, known: Vec<String> },
    /// The agent is configured but its backend could not be built.
    LlmBackend(Error),
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteError::UnknownAgent { asked, known } => write!(
                f,
                "no agent '{asked}' is configured. Available agents: {}.",
                known.join(", ")
            ),
            RouteError::LlmBackend(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RouteError {}

impl RouteError {
    pub fn is_retryable(&self) -> bool {
        match self {
            RouteError::UnknownAgent { .. } => false,
            RouteError::LlmBackend(e) => e.is_retryable(),
        }
    }
}

pub struct Ensemble {
    config: Config,
    /// One adapter per *backend*, shared by every agent assigned to it. Building
    /// one per agent would mean several HTTP agents against the same endpoint
    /// and several copies of the same settings drifting apart.
    ports: BTreeMap<String, Arc<dyn ModelPort>>,
    workdir: Option<PathBuf>,
    /// Agent runs in progress, and how many have ever started: together they
    /// tell a run whether another one overlapped it.
    active: AtomicUsize,
    started: AtomicU64,
}

impl Ensemble {
    /// Build every backend up front.
    ///
    /// Eagerly, so a misconfigured backend is a startup failure rather than a
    /// surprise on the first delegation — by which point the harness has
    /// already told the user the work is under way.
    pub fn new(config: Config, workdir: Option<PathBuf>) -> Result<Self, Error> {
        let mut ports = BTreeMap::new();
        for (name, backend) in &config.llm_backends {
            ports.insert(name.clone(), crate::backend::build(backend)?);
        }
        Ok(Self::with_ports(config, ports, workdir))
    }

    /// For tests and for the HTTP surface, which assemble their own ports.
    pub fn with_ports(
        config: Config,
        ports: BTreeMap<String, Arc<dyn ModelPort>>,
        workdir: Option<PathBuf>,
    ) -> Self {
        Ensemble {
            config,
            ports,
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

    fn resolve(&self, role: &str) -> Result<(&AgentConfig, &Arc<dyn ModelPort>), RouteError> {
        let assignment = self
            .config
            .agent(role)
            .ok_or_else(|| RouteError::UnknownAgent {
                asked: role.to_string(),
                known: self.agents().into_iter().map(str::to_string).collect(),
            })?;

        // An agent whose backend is missing from `ports` cannot happen through
        // `new`, which builds every declared backend, and config validation
        // rejects an agent naming an undeclared one. It can happen through
        // `with_ports`, so it is an error rather than an unwrap.
        let port = self.ports.get(&assignment.llm_backend).ok_or_else(|| {
            RouteError::LlmBackend(Error::Unsupported {
                backend: assignment.llm_backend.clone(),
                detail: format!("no adapter was built for this backend (agent '{role}')"),
            })
        })?;

        Ok((assignment, port))
    }

    /// Which backend and model an agent would use. Used by `ensemble agents` and
    /// by the health check; does not spend a model call.
    pub fn route_of(&self, role: &str) -> Result<(&str, &str), RouteError> {
        let (a, _) = self.resolve(role)?;
        Ok((a.llm_backend.as_str(), a.model.as_str()))
    }

    /// Send a task to an agent and return what it produced.
    ///
    /// `context` is appended to the task as a separate user turn rather than
    /// concatenated into it, so a backend that distinguishes turns keeps the
    /// distinction and one that does not loses nothing.
    pub fn delegate(
        &self,
        role: &str,
        task: &str,
        context: Option<&str>,
    ) -> Result<Outcome, RouteError> {
        self.delegate_reporting(role, task, context, &|_| {}, &AtomicBool::new(false))
    }

    /// [`Self::delegate`], telling `progress` what an agent is doing as it
    /// goes. A think-only agent makes one call and reports nothing. Setting
    /// `cancel` stops an agent at its next step.
    pub fn delegate_reporting(
        &self,
        role: &str,
        task: &str,
        context: Option<&str>,
        progress: &dyn Fn(&str),
        cancel: &AtomicBool,
    ) -> Result<Outcome, RouteError> {
        let (assignment, port) = self.resolve(role)?;
        if !assignment.tools.is_empty() {
            return self.delegate_agent(assignment, port.as_ref(), task, context, progress, cancel);
        }

        let mut messages = Vec::new();
        if let Some(prompt) = assignment
            .prompt
            .as_deref()
            .filter(|p| !p.trim().is_empty())
        {
            messages.push(Message::system(prompt));
        }
        if let Some(ctx) = context.filter(|c| !c.trim().is_empty()) {
            messages.push(Message::user(format!("Context:\n{ctx}")));
        }
        messages.push(Message::user(task.to_string()));

        let mut request = Request::new(assignment.model.clone(), messages);
        request.temperature = assignment.temperature;
        request.reasoning_effort = assignment.reasoning_effort.clone();

        let response: Response = crate::port::complete_retrying(
            port.as_ref(),
            &request,
            &|| cancel.load(Ordering::Relaxed),
            &|l| progress(&format!("{} · {l}", assignment.name)),
        )
        .map_err(RouteError::LlmBackend)?;

        Ok(Outcome {
            agent: assignment.name.clone(),
            backend: assignment.llm_backend.clone(),
            model: response.model,
            text: response.text,
            prompt_tokens: response.usage.prompt_tokens,
            completion_tokens: response.usage.completion_tokens,
            stats: None,
        })
    }

    /// An agent with tools is an agent: it loops over its tools inside the
    /// project directory instead of answering in one call.
    fn delegate_agent(
        &self,
        assignment: &AgentConfig,
        port: &dyn ModelPort,
        task: &str,
        context: Option<&str>,
        progress: &dyn Fn(&str),
        cancel: &AtomicBool,
    ) -> Result<Outcome, RouteError> {
        let workdir = match &self.workdir {
            Some(dir) => dir.clone(),
            None => std::env::current_dir().map_err(|e| {
                RouteError::LlmBackend(Error::Unsupported {
                    backend: assignment.llm_backend.clone(),
                    detail: format!("no working directory for agent '{}': {e}", assignment.name),
                })
            })?,
        };
        let me = self.started.fetch_add(1, Ordering::SeqCst) + 1;
        let others_running = self.active.fetch_add(1, Ordering::SeqCst) > 0;
        let run = crate::agent::run(port, assignment, task, context, &workdir, progress, cancel);
        let overlapped = others_running || self.started.load(Ordering::SeqCst) != me;
        self.active.fetch_sub(1, Ordering::SeqCst);
        let run = run.map_err(RouteError::LlmBackend)?;
        Ok(Outcome {
            agent: assignment.name.clone(),
            backend: assignment.llm_backend.clone(),
            model: run.model,
            text: run.text,
            prompt_tokens: run.usage.prompt_tokens,
            completion_tokens: run.usage.completion_tokens,
            stats: Some(AgentStats {
                steps: run.steps,
                tool_calls: run.tool_calls,
                trimmed: run.trimmed,
                changes: run.changes,
                commands: run.commands,
                overlapped: overlapped && assignment.tools.contains(&AgentTool::Bash),
            }),
        })
    }

    /// The configured prompt for an agent, if it has one.
    pub fn prompt_of(&self, role: &str) -> Option<&str> {
        self.config
            .agent(role)
            .and_then(|a| a.prompt.as_deref())
            .filter(|p| !p.trim().is_empty())
    }

    /// Every agent a caller may delegate to.
    pub fn delegable_agents(&self) -> Vec<&str> {
        self.config.agents.keys().map(String::as_str).collect()
    }

    /// Check every backend an agent actually uses.
    ///
    /// Keyed by backend, not by agent: two agents on one endpoint are one thing
    /// to check and one thing to fix.
    pub fn health(&self) -> Vec<(String, Result<(), Error>)> {
        let mut used: Vec<&str> = self
            .agents()
            .iter()
            .filter_map(|r| self.config.agent(r))
            .map(|a| a.llm_backend.as_str())
            .collect();
        used.sort_unstable();
        used.dedup();

        used.into_iter()
            .map(|name| {
                let result = match self.ports.get(name) {
                    Some(p) => p.health(),
                    None => Err(Error::Unsupported {
                        backend: name.into(),
                        detail: "no adapter built".into(),
                    }),
                };
                (name.to_string(), result)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::port::testing::FakePort;
    use std::collections::BTreeMap;

    const YAML: &str = r#"
version: 1
llm_backends:
  hosted:
    type: openai-compatible-api
    base_url_env: H
    base_url_default: "http://127.0.0.1:9000/v1"
  openai-local:
    type: openai-compatible-api
    base_url_env: L
    base_url_default: "http://127.0.0.1:8000/v1"
  openai-remote:
    type: openai-compatible-api
    base_url_env: R
    base_url_default: "http://10.10.10.5:8000/v1"
agents:
  planner:
    prompt: "You plan."
    adapter:
      type: ensemble
      llm_backend: openai-local
      model: qwen3-coder
  coder:
    adapter:
      type: ensemble
      llm_backend: openai-remote
      model: qwen3-coder
  reviewer:
    adapter:
      type: ensemble
      llm_backend: hosted
      model: hosted-model
"#;

    fn cfg() -> Config {
        config::parse(
            YAML,
            std::path::Path::new("ensemble.yaml"),
            &BTreeMap::new(),
        )
        .unwrap()
    }

    /// An ensemble whose three backends are fakes, each answering with its own
    /// name so routing is observable from the result alone.
    fn ensemble() -> (Ensemble, BTreeMap<String, Arc<FakePort>>) {
        let fakes: BTreeMap<String, Arc<FakePort>> = ["hosted", "openai-local", "openai-remote"]
            .into_iter()
            .map(|n| {
                (
                    n.to_string(),
                    Arc::new(FakePort::ok(n, &format!("answered by {n}"))),
                )
            })
            .collect();
        let ports: BTreeMap<String, Arc<dyn ModelPort>> = fakes
            .iter()
            .map(|(k, v)| (k.clone(), v.clone() as Arc<dyn ModelPort>))
            .collect();
        (
            Ensemble::with_ports(cfg(), ports, Some("/proj".into())),
            fakes,
        )
    }

    #[test]
    fn each_role_reaches_the_backend_its_config_names() {
        let (e, fakes) = ensemble();

        assert_eq!(
            e.delegate("planner", "t", None).unwrap().backend,
            "openai-local"
        );
        assert_eq!(
            e.delegate("coder", "t", None).unwrap().backend,
            "openai-remote"
        );
        assert_eq!(e.delegate("reviewer", "t", None).unwrap().backend, "hosted");

        // And the calls landed where the names say, not merely got labelled so.
        assert_eq!(fakes["openai-local"].call_count(), 1);
        assert_eq!(fakes["openai-remote"].call_count(), 1);
        assert_eq!(fakes["hosted"].call_count(), 1);
    }

    #[test]
    fn the_model_sent_is_the_one_the_role_is_assigned() {
        let (e, fakes) = ensemble();
        e.delegate("reviewer", "review this", None).unwrap();
        assert_eq!(fakes["hosted"].last().unwrap().model, "hosted-model");
        e.delegate("coder", "write this", None).unwrap();
        assert_eq!(fakes["openai-remote"].last().unwrap().model, "qwen3-coder");
    }

    #[test]
    fn route_of_reports_the_assignment_without_spending_a_call() {
        let (e, fakes) = ensemble();
        assert_eq!(
            e.route_of("coder").unwrap(),
            ("openai-remote", "qwen3-coder")
        );
        assert_eq!(fakes["openai-remote"].call_count(), 0);
    }

    #[test]
    fn an_unknown_role_lists_the_real_ones_and_is_not_retryable() {
        let (e, _) = ensemble();
        let err = e.delegate("architect", "t", None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("architect"), "{msg}");
        assert!(msg.contains("planner") && msg.contains("coder"), "{msg}");
        assert!(!err.is_retryable());
    }

    #[test]
    fn the_role_prompt_is_sent_as_a_system_turn() {
        let (e, fakes) = ensemble();
        e.delegate("planner", "do the thing", None).unwrap();
        let req = fakes["openai-local"].last().unwrap();
        assert_eq!(req.system_text(), "You plan.");
        // And the task is the last turn, so it is what the model answers.
        assert_eq!(req.messages.last().unwrap().content, "do the thing");
    }

    #[test]
    fn a_configured_temperature_is_sent_with_a_delegation() {
        const YAML_WITH_TEMP: &str = r#"
version: 1
llm_backends:
  openai-local:
    type: openai-compatible-api
    base_url_env: L
    base_url_default: "http://127.0.0.1:8000/v1"
agents:
  scout:
    adapter:
      type: ensemble
      llm_backend: openai-local
      model: qwen3.5:9b
      temperature: 0.2
  plain:
    adapter:
      type: ensemble
      llm_backend: openai-local
      model: qwen3.5:9b
"#;
        let cfg = config::parse(
            YAML_WITH_TEMP,
            std::path::Path::new("ensemble.yaml"),
            &BTreeMap::new(),
        )
        .unwrap();
        let fake = Arc::new(FakePort::ok("openai-local", "ok"));
        let ports: BTreeMap<String, Arc<dyn ModelPort>> = [(
            "openai-local".to_string(),
            fake.clone() as Arc<dyn ModelPort>,
        )]
        .into_iter()
        .collect();
        let e = Ensemble::with_ports(cfg, ports, None);

        e.delegate("scout", "t", None).unwrap();
        assert_eq!(fake.last().unwrap().temperature, Some(0.2));

        // No override configured: the backend's own default still applies.
        e.delegate("plain", "t", None).unwrap();
        assert_eq!(fake.last().unwrap().temperature, None);
    }

    #[test]
    fn a_role_without_a_prompt_sends_no_empty_system_turn() {
        let (e, fakes) = ensemble();
        e.delegate("coder", "plan it", None).unwrap();
        let req = fakes["openai-remote"].last().unwrap();
        assert!(req.system_text().is_empty());
        assert_eq!(req.messages.len(), 1, "just the task: {:?}", req.messages);
    }

    #[test]
    fn context_is_a_separate_turn_before_the_task() {
        let (e, fakes) = ensemble();
        e.delegate("coder", "implement step 2", Some("step 1 output"))
            .unwrap();
        let req = fakes["openai-remote"].last().unwrap();
        let contents: Vec<&str> = req.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents.len(), 2);
        assert!(contents[0].contains("step 1 output"), "{contents:?}");
        assert_eq!(contents[1], "implement step 2");
    }

    #[test]
    fn blank_context_is_dropped_rather_than_sent_as_an_empty_turn() {
        let (e, fakes) = ensemble();
        e.delegate("coder", "task", Some("   ")).unwrap();
        assert_eq!(fakes["openai-remote"].last().unwrap().messages.len(), 1);
    }

    #[test]
    fn a_backend_failure_is_reported_with_its_reason_and_its_retryability() {
        let mut ports: BTreeMap<String, Arc<dyn ModelPort>> = BTreeMap::new();
        ports.insert(
            "openai-local".into(),
            Arc::new(FakePort::ok("openai-local", "ok")),
        );
        ports.insert(
            "openai-remote".into(),
            Arc::new(FakePort::failing("openai-remote", "Connection refused")),
        );
        ports.insert("hosted".into(), Arc::new(FakePort::ok("hosted", "ok")));
        let e = Ensemble::with_ports(cfg(), ports, None);

        let err = e.delegate("coder", "t", None).unwrap_err();
        assert!(err.to_string().contains("Connection refused"), "{err}");
        assert!(err.is_retryable(), "a refused connection is worth retrying");
        // One backend being down does not take the others with it.
        assert!(e.delegate("planner", "t", None).is_ok());
    }

    #[test]
    fn usage_is_carried_through_for_the_record() {
        let (e, _) = ensemble();
        let out = e.delegate("planner", "t", None).unwrap();
        assert_eq!(out.prompt_tokens, 1);
        assert_eq!(out.completion_tokens, 2);
        assert_eq!(out.text, "answered by openai-local");
    }

    #[test]
    fn health_checks_each_backend_once_not_each_role() {
        let (e, _) = ensemble();
        let results = e.health();
        let names: Vec<&str> = results.iter().map(|(n, _)| n.as_str()).collect();
        // Three backends, one agent on each.
        assert_eq!(names, vec!["hosted", "openai-local", "openai-remote"]);
        assert!(results.iter().all(|(_, r)| r.is_ok()));
    }
}
