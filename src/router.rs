//! The Ensemble itself: the agent team, and where work goes.
//!
//! The harness names a **role**. The router turns that into a backend, a model
//! and a prompt, and returns the answer. That indirection is the product
//! requirement — "users should not need to select individual models or know
//! which backends are being used" — so nothing a caller passes in may name a
//! model or a backend, and nothing the router returns has to be interpreted
//! differently depending on which one answered.

use crate::config::{Assignment, Config};
use crate::port::{
    Capabilities, Error, Message, ModelPort, Request, Response, ToolCall, ToolChoice, ToolDef,
    Usage,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// One model call for a role, with the caller's own conversation.
///
/// This is the counterpart to [`Ensemble::delegate`]. `delegate` assembles a
/// conversation from a task and some context, because the caller has a job and
/// no transcript. `Turn` is for the opposite case: `ensemble serve`, where a
/// harness owns the transcript and Ensemble must send it as it stands rather
/// than rewriting it.
///
/// Neither names a backend or a model. The role still decides that.
#[derive(Debug, Clone, Default)]
pub struct Turn {
    pub messages: Vec<Message>,
    /// Tools the *caller* will run. See the note on `port::Request`.
    pub tools: Vec<ToolDef>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

/// What one [`Turn`] produced.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub role: String,
    pub backend: String,
    pub model: String,
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub finish_reason: Option<String>,
}

/// What a delegation produced, with enough provenance to audit it.
///
/// The backend and model are *reported*, not accepted as input. A caller cannot
/// steer the work onto a particular model; a reader of the record can still see
/// which one did it.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub role: String,
    pub backend: String,
    pub model: String,
    pub text: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Set when the role ran as an agent: how much it did to get there.
    pub agent: Option<AgentStats>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentStats {
    pub steps: u32,
    pub tool_calls: u32,
}

#[derive(Debug)]
pub enum RouteError {
    /// The role is not in the config. Carries the ones that are, because the
    /// caller is usually a model that guessed a plausible name.
    UnknownRole { asked: String, known: Vec<String> },
    /// The role is configured but its backend could not be built.
    Backend(Error),
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteError::UnknownRole { asked, known } => write!(
                f,
                "no role '{asked}' is configured. Available roles: {}.",
                known.join(", ")
            ),
            RouteError::Backend(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RouteError {}

impl RouteError {
    pub fn is_retryable(&self) -> bool {
        match self {
            RouteError::UnknownRole { .. } => false,
            RouteError::Backend(e) => e.is_retryable(),
        }
    }
}

/// The throwaway tool [`Ensemble::probe_tool_calling`] offers. Named so it
/// cannot collide with the ensemble's real tools and is recognisable in a
/// server log.
pub const PROBE_TOOL: &str = "ensemble_probe";

/// What a tool-calling probe found. An `Err` from the probe is neither of
/// these: the endpoint did not answer, which says nothing about tool calling.
#[derive(Debug, Clone)]
pub enum ToolCallProbe {
    /// A structured tool call came back. This model on this server can drive a
    /// caller-supplied schema.
    Works,
    /// The endpoint answered and the call was not in `tool_calls`. `text` is
    /// whatever came instead — usually the written-out call, which names the
    /// format the server has no parser for.
    NotParsed {
        text: String,
        finish_reason: Option<String>,
    },
}

pub struct Ensemble {
    config: Config,
    /// One adapter per *backend*, shared by every role assigned to it. Building
    /// one per role would mean several HTTP agents against the same endpoint
    /// and several copies of the same settings drifting apart.
    ports: BTreeMap<String, Arc<dyn ModelPort>>,
    workdir: Option<PathBuf>,
}

impl Ensemble {
    /// Build every backend up front.
    ///
    /// Eagerly, so a misconfigured backend is a startup failure rather than a
    /// surprise on the first delegation — by which point the harness has
    /// already told the user the work is under way.
    pub fn new(config: Config, workdir: Option<PathBuf>) -> Result<Self, Error> {
        let mut ports = BTreeMap::new();
        for (name, backend) in &config.backends {
            ports.insert(
                name.clone(),
                crate::backend::build(backend, workdir.clone())?,
            );
        }
        Ok(Ensemble {
            config,
            ports,
            workdir,
        })
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
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn roles(&self) -> Vec<&str> {
        self.config.role_names()
    }

    fn resolve(&self, role: &str) -> Result<(&Assignment, &Arc<dyn ModelPort>), RouteError> {
        let assignment = self
            .config
            .assignment(role)
            .ok_or_else(|| RouteError::UnknownRole {
                asked: role.to_string(),
                known: self.roles().into_iter().map(str::to_string).collect(),
            })?;

        // A role whose backend is missing from `ports` cannot happen through
        // `new`, which builds every declared backend, and config validation
        // rejects a role naming an undeclared one. It can happen through
        // `with_ports`, so it is an error rather than an unwrap.
        let port = self.ports.get(&assignment.backend).ok_or_else(|| {
            RouteError::Backend(Error::Unsupported {
                backend: assignment.backend.clone(),
                detail: format!("no adapter was built for this backend (role '{role}')"),
            })
        })?;

        Ok((assignment, port))
    }

    /// Which backend and model a role would use. Used by `ensemble roles` and
    /// by the health check; does not spend a model call.
    pub fn route_of(&self, role: &str) -> Result<(&str, &str), RouteError> {
        let (a, _) = self.resolve(role)?;
        Ok((a.backend.as_str(), a.model.as_str()))
    }

    /// Send a task to a role and return what it produced.
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
        let (assignment, port) = self.resolve(role)?;
        if !assignment.tools.is_empty() {
            return self.delegate_agent(assignment, port.as_ref(), task, context, &|_| {});
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
        request.workdir = self.workdir.as_ref().map(|p| p.display().to_string());
        request.temperature = assignment.temperature;

        let response: Response = port.complete(&request).map_err(RouteError::Backend)?;

        Ok(Outcome {
            role: assignment.role.clone(),
            backend: assignment.backend.clone(),
            model: response.model,
            text: response.text,
            prompt_tokens: response.usage.prompt_tokens,
            completion_tokens: response.usage.completion_tokens,
            agent: None,
        })
    }

    /// Like [`delegate`](Self::delegate), but `on_delta` is called with each
    /// fragment of the role's text as it arrives.
    pub fn delegate_streaming(
        &self,
        role: &str,
        task: &str,
        context: Option<&str>,
        on_delta: &dyn Fn(&str),
    ) -> Result<Outcome, RouteError> {
        let (assignment, port) = self.resolve(role)?;
        if !assignment.tools.is_empty() {
            return self.delegate_agent(assignment, port.as_ref(), task, context, on_delta);
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
        request.workdir = self.workdir.as_ref().map(|p| p.display().to_string());
        request.temperature = assignment.temperature;

        let response: Response = port
            .complete_streaming(&request, on_delta)
            .map_err(RouteError::Backend)?;

        Ok(Outcome {
            role: assignment.role.clone(),
            backend: assignment.backend.clone(),
            model: response.model,
            text: response.text,
            prompt_tokens: response.usage.prompt_tokens,
            completion_tokens: response.usage.completion_tokens,
            agent: None,
        })
    }

    /// A role with tools is an agent: it loops over its tools inside the
    /// project directory instead of answering in one call.
    fn delegate_agent(
        &self,
        assignment: &Assignment,
        port: &dyn ModelPort,
        task: &str,
        context: Option<&str>,
        on_delta: &dyn Fn(&str),
    ) -> Result<Outcome, RouteError> {
        let workdir = match &self.workdir {
            Some(dir) => dir.clone(),
            None => std::env::current_dir().map_err(|e| {
                RouteError::Backend(Error::Unsupported {
                    backend: assignment.backend.clone(),
                    detail: format!("no working directory for role '{}': {e}", assignment.role),
                })
            })?,
        };
        let run = crate::agent::run(port, assignment, task, context, &workdir, on_delta)
            .map_err(RouteError::Backend)?;
        Ok(Outcome {
            role: assignment.role.clone(),
            backend: assignment.backend.clone(),
            model: run.model,
            text: run.text,
            prompt_tokens: run.usage.prompt_tokens,
            completion_tokens: run.usage.completion_tokens,
            agent: Some(AgentStats {
                steps: run.steps,
                tool_calls: run.tool_calls,
            }),
        })
    }

    /// Send one turn to a role and return what the model said, tool calls and
    /// all. Nothing is executed here; running a tool is the caller's business.
    ///
    /// A role whose backend cannot drive a caller-supplied tool schema refuses
    /// the request inside its own adapter, naming the backend and the fix. That
    /// check is not duplicated here: an adapter is the only thing that knows
    /// what it can do, and two copies of a rule is one copy too many.
    pub fn turn(&self, role: &str, turn: &Turn) -> Result<TurnOutcome, RouteError> {
        let (assignment, port) = self.resolve(role)?;

        let mut request = Request::new(assignment.model.clone(), turn.messages.clone());
        request.tools = turn.tools.clone();
        request.max_tokens = turn.max_tokens;
        // The role's own setting wins: it exists to correct a backend's
        // default sampling (Ollama's Modelfile temperature, say), and a
        // caller's generic request-level value has no say over that.
        request.temperature = assignment.temperature.or(turn.temperature);
        request.workdir = self.workdir.as_ref().map(|p| p.display().to_string());

        let response = port.complete(&request).map_err(RouteError::Backend)?;

        Ok(TurnOutcome {
            role: assignment.role.clone(),
            backend: assignment.backend.clone(),
            model: response.model,
            text: response.text,
            tool_calls: response.tool_calls,
            usage: response.usage,
            finish_reason: response.finish_reason,
        })
    }

    /// Like [`turn`](Self::turn), but `on_delta` is called with each fragment
    /// of the model's text as it arrives.
    pub fn turn_streaming(
        &self,
        role: &str,
        turn: &Turn,
        on_delta: &dyn Fn(&str),
    ) -> Result<TurnOutcome, RouteError> {
        let (assignment, port) = self.resolve(role)?;

        let mut request = Request::new(assignment.model.clone(), turn.messages.clone());
        request.tools = turn.tools.clone();
        request.max_tokens = turn.max_tokens;
        request.temperature = assignment.temperature.or(turn.temperature);
        request.workdir = self.workdir.as_ref().map(|p| p.display().to_string());

        let response = port
            .complete_streaming(&request, on_delta)
            .map_err(RouteError::Backend)?;

        Ok(TurnOutcome {
            role: assignment.role.clone(),
            backend: assignment.backend.clone(),
            model: response.model,
            text: response.text,
            tool_calls: response.tool_calls,
            usage: response.usage,
            finish_reason: response.finish_reason,
        })
    }

    /// What the backend behind a role can do. Declared by the adapter, never
    /// inferred. `ensemble serve` reads this before it binds.
    pub fn capabilities_of(&self, role: &str) -> Result<Capabilities, RouteError> {
        let (_, port) = self.resolve(role)?;
        Ok(port.capabilities())
    }

    /// Ask a role's backend for one tool call and find out whether it arrives
    /// as one.
    ///
    /// [`Capabilities::tool_calling`] is a statement about a backend *type*:
    /// `openai-compatible-api` says yes because the protocol has the field.
    /// Whether *this* server, started with *these* flags, will parse *this*
    /// model's tool call into that field is a different question, and nothing
    /// in the config can answer it — llama-server without `--jinja`, vLLM
    /// without a `--tool-call-parser`, and an Ollama template with no `.Tools`
    /// branch all hand the model's call back as ordinary text.
    ///
    /// `Required`, not `Auto`: with `Auto` a model that simply chose to answer
    /// is indistinguishable from a server that dropped the call it made, and
    /// only the second is a fault.
    ///
    /// Spends one small model call. The caller decides whether that is worth
    /// it; `route_of` and `capabilities_of` remain free.
    pub fn probe_tool_calling(&self, role: &str) -> Result<ToolCallProbe, RouteError> {
        let (assignment, port) = self.resolve(role)?;

        let mut request = Request::new(
            assignment.model.clone(),
            vec![
                Message::system(
                    "You are a conformance probe. Reply only by calling the tool you are given.",
                ),
                Message::user(format!("Call `{PROBE_TOOL}` with `ok` set to \"yes\".")),
            ],
        );
        request.tools = vec![ToolDef {
            name: PROBE_TOOL.to_string(),
            description: "Answer the probe. Call this and write nothing else.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "ok": { "type": "string" } },
                "required": ["ok"],
                "additionalProperties": false
            }),
        }];
        request.tool_choice = ToolChoice::Required;
        request.temperature = Some(0.0);
        // Enough for a call plus a short preamble, bounded so a model that
        // ignores the instruction and starts writing cannot cost real money.
        request.max_tokens = Some(256);
        // No workdir on purpose: this spends a model call and must touch
        // nothing in the project.

        let response = port.complete(&request).map_err(RouteError::Backend)?;
        if response.tool_calls.is_empty() {
            Ok(ToolCallProbe::NotParsed {
                text: response.text,
                finish_reason: response.finish_reason,
            })
        } else {
            Ok(ToolCallProbe::Works)
        }
    }

    /// The configured prompt for a role, if it has one.
    pub fn prompt_of(&self, role: &str) -> Option<&str> {
        self.config
            .assignment(role)
            .and_then(|a| a.prompt.as_deref())
            .filter(|p| !p.trim().is_empty())
    }

    /// Every role the manager may delegate *to* — the team without the manager.
    ///
    /// Over MCP the caller is outside the ensemble and the manager is a fair
    /// target. Over the chat API the caller *is* the manager, and offering it
    /// itself is offering it a way to recurse.
    pub fn delegable_roles(&self) -> Vec<&str> {
        self.config.roles.keys().map(String::as_str).collect()
    }

    /// Check every backend a role actually uses.
    ///
    /// Keyed by backend, not by role: two roles on one endpoint are one thing
    /// to check and one thing to fix.
    pub fn health(&self) -> Vec<(String, Result<(), Error>)> {
        let mut used: Vec<&str> = self
            .roles()
            .iter()
            .filter_map(|r| self.config.assignment(r))
            .map(|a| a.backend.as_str())
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
backends:
  claude:
    type: claude-code-cli
  openai-local:
    type: openai-compatible-api
    base_url_env: L
    base_url_default: "http://127.0.0.1:8000/v1"
  openai-remote:
    type: openai-compatible-api
    base_url_env: R
    base_url_default: "http://10.10.10.5:8000/v1"
manager:
  backend: openai-local
  model: qwen3-coder
  prompt: "You are the manager."
roles:
  planner:
    backend: openai-local
    model: qwen3-coder
  coder:
    backend: openai-remote
    model: qwen3-coder
  reviewer:
    backend: claude
    model: claude-sonnet
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
        let fakes: BTreeMap<String, Arc<FakePort>> = ["claude", "openai-local", "openai-remote"]
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
            e.delegate("manager", "t", None).unwrap().backend,
            "openai-local"
        );
        assert_eq!(
            e.delegate("planner", "t", None).unwrap().backend,
            "openai-local"
        );
        assert_eq!(
            e.delegate("coder", "t", None).unwrap().backend,
            "openai-remote"
        );
        assert_eq!(e.delegate("reviewer", "t", None).unwrap().backend, "claude");

        // And the calls landed where the names say, not merely got labelled so.
        assert_eq!(fakes["openai-local"].call_count(), 2);
        assert_eq!(fakes["openai-remote"].call_count(), 1);
        assert_eq!(fakes["claude"].call_count(), 1);
    }

    #[test]
    fn the_model_sent_is_the_one_the_role_is_assigned() {
        let (e, fakes) = ensemble();
        e.delegate("reviewer", "review this", None).unwrap();
        assert_eq!(fakes["claude"].last().unwrap().model, "claude-sonnet");
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
        assert_eq!(
            e.route_of("manager").unwrap(),
            ("openai-local", "qwen3-coder")
        );
        assert_eq!(fakes["openai-remote"].call_count(), 0);
    }

    #[test]
    fn an_unknown_role_lists_the_real_ones_and_is_not_retryable() {
        let (e, _) = ensemble();
        let err = e.delegate("architect", "t", None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("architect"), "{msg}");
        assert!(msg.contains("manager") && msg.contains("coder"), "{msg}");
        assert!(!err.is_retryable());
    }

    #[test]
    fn the_role_prompt_is_sent_as_a_system_turn() {
        let (e, fakes) = ensemble();
        e.delegate("manager", "do the thing", None).unwrap();
        let req = fakes["openai-local"].last().unwrap();
        assert_eq!(req.system_text(), "You are the manager.");
        // And the task is the last turn, so it is what the model answers.
        assert_eq!(req.messages.last().unwrap().content, "do the thing");
    }

    #[test]
    fn a_configured_temperature_is_sent_with_a_delegation() {
        const YAML_WITH_TEMP: &str = r#"
version: 1
backends:
  openai-local:
    type: openai-compatible-api
    base_url_env: L
    base_url_default: "http://127.0.0.1:8000/v1"
manager:
  backend: openai-local
  model: qwen3.5:9b
  temperature: 0.2
roles:
  scout:
    backend: openai-local
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

        e.delegate("manager", "t", None).unwrap();
        assert_eq!(fake.last().unwrap().temperature, Some(0.2));

        // No override configured: the backend's own default still applies.
        e.delegate("scout", "t", None).unwrap();
        assert_eq!(fake.last().unwrap().temperature, None);
    }

    #[test]
    fn a_configured_temperature_overrides_what_the_caller_asked_for_in_a_turn() {
        const YAML_WITH_TEMP: &str = r#"
version: 1
backends:
  openai-local:
    type: openai-compatible-api
    base_url_env: L
    base_url_default: "http://127.0.0.1:8000/v1"
manager:
  backend: openai-local
  model: qwen3.5:9b
  temperature: 0.2
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

        let turn = Turn {
            messages: vec![Message::user("hi")],
            temperature: Some(1.0),
            ..Default::default()
        };
        e.turn("manager", &turn).unwrap();
        assert_eq!(fake.last().unwrap().temperature, Some(0.2));
    }

    #[test]
    fn a_role_without_a_prompt_sends_no_empty_system_turn() {
        let (e, fakes) = ensemble();
        e.delegate("planner", "plan it", None).unwrap();
        let req = fakes["openai-local"].last().unwrap();
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
    fn the_project_directory_travels_with_every_request() {
        let (e, fakes) = ensemble();
        e.delegate("reviewer", "review", None).unwrap();
        assert_eq!(
            fakes["claude"].last().unwrap().workdir.as_deref(),
            Some("/proj")
        );
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
        ports.insert("claude".into(), Arc::new(FakePort::ok("claude", "ok")));
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
        // Three backends, four roles, two of which share one backend.
        assert_eq!(names, vec!["claude", "openai-local", "openai-remote"]);
        assert!(results.iter().all(|(_, r)| r.is_ok()));
    }

    // --- the tool-calling probe -----------------------------------------

    /// An ensemble whose manager backend answers with `script`.
    fn with_manager_script(
        script: Vec<crate::port::testing::Scripted>,
    ) -> (Ensemble, Arc<FakePort>) {
        let mgr = Arc::new(FakePort::scripted("openai-local", "plain text", script));
        let mut ports: BTreeMap<String, Arc<dyn ModelPort>> = BTreeMap::new();
        ports.insert("openai-local".into(), mgr.clone() as Arc<dyn ModelPort>);
        ports.insert(
            "openai-remote".into(),
            Arc::new(FakePort::ok("openai-remote", "x")),
        );
        ports.insert("claude".into(), Arc::new(FakePort::ok("claude", "x")));
        (
            Ensemble::with_ports(cfg(), ports, Some("/proj".into())),
            mgr,
        )
    }

    #[test]
    fn a_probe_that_gets_a_structured_call_back_reports_that_it_works() {
        let (e, mgr) = with_manager_script(vec![crate::port::testing::Scripted::call(
            "p1",
            PROBE_TOOL,
            r#"{"ok":"yes"}"#,
        )]);
        assert!(matches!(
            e.probe_tool_calling(Config::MANAGER).unwrap(),
            ToolCallProbe::Works
        ));
        assert_eq!(mgr.call_count(), 1, "exactly one model call");
    }

    #[test]
    fn a_probe_answered_in_prose_reports_what_came_instead() {
        // The whole fault this exists for: the server answers 200 and the call
        // is not in `tool_calls`.
        let (e, _) = with_manager_script(vec![crate::port::testing::Scripted::text(
            "ensemble_probe(ok='yes')",
        )]);
        let ToolCallProbe::NotParsed { text, .. } = e.probe_tool_calling(Config::MANAGER).unwrap()
        else {
            panic!("prose is not a tool call");
        };
        assert_eq!(text, "ensemble_probe(ok='yes')");
    }

    #[test]
    fn the_probe_requires_a_call_offers_one_throwaway_tool_and_touches_no_project() {
        let (e, mgr) = with_manager_script(vec![crate::port::testing::Scripted::call(
            "p1", PROBE_TOOL, "{}",
        )]);
        e.probe_tool_calling(Config::MANAGER).unwrap();
        let sent = mgr.last().unwrap();
        // `auto` cannot tell "chose to answer" from "the call was dropped".
        assert_eq!(sent.tool_choice, crate::port::ToolChoice::Required);
        assert_eq!(sent.tools.len(), 1);
        assert_eq!(sent.tools[0].name, PROBE_TOOL);
        // Not one of the ensemble's real tools, so it cannot be confused for
        // one in a server log or in `is_ensemble_tool`.
        assert!(!crate::harness::delegation::is_ensemble_tool(PROBE_TOOL));
        assert_eq!(sent.model, "qwen3-coder", "the role's model, not a guess");
        assert!(sent.workdir.is_none(), "a probe acts on nothing");
        assert!(sent.max_tokens.is_some(), "bounded: {:?}", sent.max_tokens);
    }

    #[test]
    fn a_probe_against_an_unreachable_backend_is_an_error_not_a_verdict() {
        let mut ports: BTreeMap<String, Arc<dyn ModelPort>> = BTreeMap::new();
        for n in ["openai-local", "openai-remote", "claude"] {
            ports.insert(
                n.into(),
                Arc::new(FakePort::failing(n, "Connection refused")) as Arc<dyn ModelPort>,
            );
        }
        let e = Ensemble::with_ports(cfg(), ports, None);
        assert!(e.probe_tool_calling(Config::MANAGER).is_err());
    }

    #[test]
    fn the_manager_is_always_a_role_and_is_listed_first() {
        let (e, _) = ensemble();
        assert_eq!(e.roles()[0], "manager");
        assert!(e.delegate("manager", "t", None).is_ok());
    }
}
