//! `ensemble.yaml`: load, resolve the environment, validate.
//!
//! Validation is deliberately loud and early. Every check here corresponds to a
//! failure that would otherwise surface much later and much less clearly — as a
//! 404 on the first request to a role, as a role that quietly runs with
//! default settings, or as an empty base URL that turns into a connection refused.
//!
//! Secrets never live in this file. An API key is named by the *variable* that
//! holds it (`api_key_env`), and the value is read from the environment at load
//! time. There is no `api_key` field; a config that tries one is rejected by
//! name so the mistake is caught before the file is committed.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

pub const CONFIG_FILENAME: &str = "ensemble.yaml";
pub const SUPPORTED_VERSION: u32 = 1;

/// The backend types that ship. Adding one is adding an adapter and a variant.
pub const CLAUDE_CODE_CLI: &str = "claude-code-cli";
pub const OPENAI_COMPATIBLE_API: &str = "openai-compatible-api";

// ---------------------------------------------------------------------------
// The document, as written
// ---------------------------------------------------------------------------

/// `deny_unknown_fields` throughout: a typo'd key is a key that does nothing,
/// and a role silently running with default settings is the failure mode this
/// whole module exists to prevent. `reviewr:` should be an error, not a no-op.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    version: u32,
    #[serde(default)]
    backends: BTreeMap<String, RawBackend>,
    /// Removed. Kept only so an old config gets a pointed error instead of
    /// "unknown field".
    #[serde(default)]
    manager: Option<serde_norway::Value>,
    #[serde(default)]
    roles: BTreeMap<String, RawAssignment>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBackend {
    #[serde(rename = "type")]
    type_id: String,

    // --- openai-compatible-api ---
    base_url_env: Option<String>,
    base_url_default: Option<String>,
    api_key_env: Option<String>,

    // --- claude-code-cli ---
    /// Path to the CLI. Defaults to `claude` on PATH.
    command: Option<String>,
    /// Tools Claude Code may run on its own. Read-only by default: a role that
    /// should edit files has to say so.
    allowed_tools: Option<String>,
    permission_mode: Option<String>,
    max_turns: Option<u32>,

    // --- both ---
    timeout_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAssignment {
    backend: String,
    model: String,
    /// Prepended to every request sent to this role. Optional: a role with no
    /// prompt is a model with a name, which is a legitimate thing to want.
    #[serde(default)]
    prompt: Option<String>,
    /// Overrides the backend's own default for this role. Needed when a
    /// model's baked-in sampling (Ollama's `temperature`/`presence_penalty`
    /// Modelfile defaults, for instance) is too hot for reliable tool calling.
    /// Takes precedence over whatever a caller asked for: a chat client has no
    /// business tuning a role's reliability-critical sampling.
    #[serde(default)]
    temperature: Option<f32>,
    /// What the role may do on its own, inside the project directory. Empty or
    /// absent is a think-only role: one model call, text back. Anything listed
    /// turns the role into an agent that loops over these tools; write access
    /// has to be asked for by name.
    #[serde(default)]
    tools: Vec<String>,
    /// Cap on model calls within one delegation. Defaults to
    /// [`DEFAULT_MAX_STEPS`].
    #[serde(default)]
    max_steps: Option<u32>,
    /// Most tokens the agent's conversation may hold in one model call.
    /// Defaults to [`DEFAULT_CONTEXT_TOKENS`]. Keep it at or below the context
    /// the server really runs the model with.
    #[serde(default)]
    context_tokens: Option<u32>,
    /// Let the role's `bash` commands open outbound TCP connections. Off by
    /// default: a command that can read the filesystem and reach the network
    /// can send what it reads anywhere.
    #[serde(default)]
    network: Option<bool>,
}

// ---------------------------------------------------------------------------
// The document, resolved
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendSettings {
    ClaudeCodeCli {
        command: String,
        allowed_tools: String,
        permission_mode: String,
        max_turns: u32,
    },
    OpenAiCompatible {
        /// Already resolved from the environment. Never a `{env:...}` template:
        /// if it could not be resolved, loading failed instead.
        base_url: String,
        api_key: Option<String>,
        /// Kept for diagnostics — which variable supplied the URL above.
        base_url_env: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backend {
    pub name: String,
    pub timeout_seconds: u64,
    pub settings: BackendSettings,
}

impl Backend {
    pub fn type_id(&self) -> &'static str {
        match self.settings {
            BackendSettings::ClaudeCodeCli { .. } => CLAUDE_CODE_CLI,
            BackendSettings::OpenAiCompatible { .. } => OPENAI_COMPATIBLE_API,
        }
    }

    /// Does this backend type let a model call the *caller's* tools?
    ///
    /// `claude -p` emits Claude Code's own tool_use blocks, which Claude Code
    /// executes itself; it never returns an OpenAI `tool_calls` array against a
    /// schema we supplied. So the answer is no, and it is declared rather than
    /// discovered.
    ///
    /// A role on this backend still does real work, with Claude Code's own
    /// tools, and reports back in prose. What it cannot do is drive the tool
    /// loop in `agent`, which needs a schema *this* process supplies.
    pub fn supports_tool_calling(&self) -> bool {
        matches!(self.settings, BackendSettings::OpenAiCompatible { .. })
    }
}

/// One capability of ensemble's own agent loop. Names are the config spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgentTool {
    Read,
    Glob,
    Grep,
    Edit,
    Write,
    /// Run a shell command in the project, inside a Landlock sandbox.
    Bash,
}

impl AgentTool {
    pub const ALL: [AgentTool; 6] = [
        AgentTool::Read,
        AgentTool::Glob,
        AgentTool::Grep,
        AgentTool::Edit,
        AgentTool::Write,
        AgentTool::Bash,
    ];

    pub fn parse(name: &str) -> Option<AgentTool> {
        AgentTool::ALL.into_iter().find(|t| t.as_str() == name)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AgentTool::Read => "read",
            AgentTool::Glob => "glob",
            AgentTool::Grep => "grep",
            AgentTool::Edit => "edit",
            AgentTool::Write => "write",
            AgentTool::Bash => "bash",
        }
    }

    /// Can this tool change files?
    pub fn writes(self) -> bool {
        matches!(self, AgentTool::Edit | AgentTool::Write | AgentTool::Bash)
    }
}

pub const DEFAULT_MAX_STEPS: u32 = 20;
pub const DEFAULT_CONTEXT_TOKENS: u32 = 16_000;

#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub role: String,
    pub backend: String,
    pub model: String,
    pub prompt: Option<String>,
    pub temperature: Option<f32>,
    /// Empty: a think-only role. See [`RawAssignment::tools`].
    pub tools: Vec<AgentTool>,
    pub max_steps: u32,
    /// See [`RawAssignment::context_tokens`].
    pub context_tokens: u32,
    /// Outbound TCP for `bash`. Meaningless without that tool.
    pub network: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub version: u32,
    pub backends: BTreeMap<String, Backend>,
    /// The team. There is no manager: the agent that calls Ensemble is it.
    pub roles: BTreeMap<String, Assignment>,
    /// Where it was loaded from, for error messages.
    pub source: PathBuf,
}

impl Config {
    pub fn assignment(&self, role: &str) -> Option<&Assignment> {
        self.roles.get(role)
    }

    pub fn backend_for(&self, role: &str) -> Option<&Backend> {
        self.assignment(role)
            .and_then(|a| self.backends.get(&a.backend))
    }

    pub fn role_names(&self) -> Vec<&str> {
        self.roles.keys().map(String::as_str).collect()
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ConfigError {
    NotFound {
        path: PathBuf,
    },
    Unreadable {
        path: PathBuf,
        detail: String,
    },
    Syntax {
        path: PathBuf,
        detail: String,
    },
    /// One or more validation failures. Collected rather than returned one at a
    /// time: fixing a config file five round trips in a row is miserable, and
    /// every check here is independent of the others.
    Invalid {
        path: PathBuf,
        problems: Vec<String>,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::NotFound { path } => write!(
                f,
                "no {CONFIG_FILENAME} at {}. Create one, or pass --config <path>.",
                path.display()
            ),
            ConfigError::Unreadable { path, detail } => {
                write!(f, "cannot read {}: {detail}", path.display())
            }
            ConfigError::Syntax { path, detail } => {
                write!(f, "{} is not valid YAML: {detail}", path.display())
            }
            ConfigError::Invalid { path, problems } => {
                writeln!(f, "{} is invalid:", path.display())?;
                for p in problems {
                    writeln!(f, "  - {p}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ConfigError {}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// How the environment is read. Injected so validation is testable without
/// `set_var`, which is `unsafe` in edition 2024 and racy across threads.
pub trait Env {
    fn get(&self, key: &str) -> Option<String>;
}

pub struct SystemEnv;

impl Env for SystemEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

impl Env for BTreeMap<String, String> {
    fn get(&self, key: &str) -> Option<String> {
        BTreeMap::get(self, key).cloned()
    }
}

/// The project's own `ensemble.yaml`, and nothing else.
///
/// Parent directories are deliberately not searched: a team is a property of
/// one project, and a file in `~/projects/` silently configuring every
/// checkout below it is exactly the shared, out-of-sight config this rules
/// out. There is no user-level fallback either.
pub fn discover(project: &Path) -> Option<PathBuf> {
    let candidate = project.join(CONFIG_FILENAME);
    candidate.is_file().then_some(candidate)
}

pub fn load(path: &Path, env: &dyn Env) -> Result<Config, ConfigError> {
    if !path.exists() {
        return Err(ConfigError::NotFound {
            path: path.to_path_buf(),
        });
    }
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Unreadable {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;
    parse(&text, path, env)
}

pub fn parse(text: &str, path: &Path, env: &dyn Env) -> Result<Config, ConfigError> {
    let raw: RawConfig = serde_norway::from_str(text).map_err(|e| ConfigError::Syntax {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;
    validate(raw, path, env)
}

const DEFAULT_TIMEOUT_SECONDS: u64 = 600;

fn validate(raw: RawConfig, path: &Path, env: &dyn Env) -> Result<Config, ConfigError> {
    let mut problems: Vec<String> = Vec::new();

    if raw.version != SUPPORTED_VERSION {
        problems.push(format!(
            "version: {} is not supported; this build understands version {SUPPORTED_VERSION}",
            raw.version
        ));
    }

    if raw.backends.is_empty() {
        problems.push("backends: at least one backend must be defined".into());
    }

    // --- backends ---
    let mut backends: BTreeMap<String, Backend> = BTreeMap::new();
    for (name, rb) in &raw.backends {
        match resolve_backend(name, rb, env) {
            Ok(b) => {
                backends.insert(name.clone(), b);
            }
            Err(mut errs) => problems.append(&mut errs),
        }
    }

    // --- roles ---
    //
    if raw.manager.is_some() {
        problems.push(
            "manager: there is no manager block any more. The agent that calls Ensemble \
             (Claude Code, OpenCode, ...) is the manager; delete this block and give the \
             work to `roles:`."
                .into(),
        );
    }

    let mut roles: BTreeMap<String, Assignment> = BTreeMap::new();
    for (name, ra) in &raw.roles {
        if name.trim().is_empty() {
            problems.push("roles: a role name cannot be empty".into());
            continue;
        }
        let a = Assignment {
            role: name.clone(),
            backend: ra.backend.clone(),
            model: ra.model.clone(),
            prompt: ra.prompt.clone(),
            temperature: ra.temperature,
            tools: parse_tools(name, &ra.tools, &mut problems),
            max_steps: ra.max_steps.unwrap_or(DEFAULT_MAX_STEPS),
            context_tokens: ra.context_tokens.unwrap_or(DEFAULT_CONTEXT_TOKENS),
            network: ra.network.unwrap_or(false),
        };
        if ra.network.is_some() && !a.tools.contains(&AgentTool::Bash) {
            problems.push(format!(
                "roles.{name}: `network` only applies to the `bash` tool, which this role does not have"
            ));
        }
        if ra.max_steps == Some(0) {
            problems.push(format!("roles.{name}: `max_steps` must be greater than 0"));
        }
        if ra.context_tokens == Some(0) {
            problems.push(format!(
                "roles.{name}: `context_tokens` must be greater than 0"
            ));
        }
        if !a.tools.is_empty()
            && backends
                .get(&a.backend)
                .is_some_and(|b| !b.supports_tool_calling())
        {
            problems.push(format!(
                "roles.{name}: `tools` needs a backend that supports tool calling; '{}' does not",
                a.backend
            ));
        }
        check_assignment(&a, &backends, &raw.backends, &mut problems);
        roles.insert(name.clone(), a);
    }

    // A backend nothing uses is not an error — it is a spare, and deleting it
    // from a shared config is how you break someone else's profile. Say so once
    // and carry on.
    for name in backends.keys() {
        let used = roles.values().any(|a| a.backend == *name);
        if !used {
            eprintln!("ensemble: note: backend '{name}' is defined but no role uses it");
        }
    }

    if problems.is_empty() {
        Ok(Config {
            version: raw.version,
            backends,
            roles,
            source: path.to_path_buf(),
        })
    } else {
        Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            problems,
        })
    }
}

fn parse_tools(role: &str, names: &[String], problems: &mut Vec<String>) -> Vec<AgentTool> {
    let mut tools = Vec::new();
    for name in names {
        match AgentTool::parse(name) {
            Some(t) if !tools.contains(&t) => tools.push(t),
            Some(_) => {}
            None => {
                let known: Vec<&str> = AgentTool::ALL.iter().map(|t| t.as_str()).collect();
                problems.push(format!(
                    "roles.{role}: tool '{name}' is unknown. Known tools: {}.",
                    known.join(", ")
                ));
            }
        }
    }
    tools
}

/// Does this assignment name a backend that exists, with a non-empty model?
fn check_assignment(
    a: &Assignment,
    resolved: &BTreeMap<String, Backend>,
    declared: &BTreeMap<String, RawBackend>,
    problems: &mut Vec<String>,
) {
    let where_ = format!("roles.{}", a.role);

    if a.backend.trim().is_empty() {
        problems.push(format!("{where_}: `backend` is empty"));
    } else if !declared.contains_key(&a.backend) {
        let known: Vec<&str> = declared.keys().map(String::as_str).collect();
        problems.push(format!(
            "{where_}: backend '{}' is not defined. Defined backends: {}.",
            a.backend,
            if known.is_empty() {
                "(none)".to_string()
            } else {
                known.join(", ")
            }
        ));
    } else if !resolved.contains_key(&a.backend) {
        // Declared but failed to resolve. Its own problem is already in the
        // list; adding "not defined" here would be a second, misleading one.
    }

    if a.model.trim().is_empty() {
        problems.push(format!("{where_}: `model` is empty"));
    }

    if let Some(t) = a.temperature {
        if !(0.0..=2.0).contains(&t) {
            problems.push(format!(
                "{where_}: `temperature` is {t}; must be between 0.0 and 2.0"
            ));
        }
    }
}

fn resolve_backend(name: &str, rb: &RawBackend, env: &dyn Env) -> Result<Backend, Vec<String>> {
    let mut problems = Vec::new();
    let timeout_seconds = rb.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    if timeout_seconds == 0 {
        problems.push(format!(
            "backends.{name}: `timeout_seconds` must be greater than 0"
        ));
    }

    // Reject settings that belong to a different type rather than ignoring
    // them. A `base_url_env` on a claude-code-cli backend means someone
    // believes the URL is being used, and it is not.
    let mut reject_foreign = |keys: &[(&str, bool)], type_id: &str| {
        for (key, present) in keys {
            if *present {
                problems.push(format!(
                    "backends.{name}: `{key}` is not a setting of type '{type_id}'"
                ));
            }
        }
    };

    let settings = match rb.type_id.as_str() {
        CLAUDE_CODE_CLI => {
            reject_foreign(
                &[
                    ("base_url_env", rb.base_url_env.is_some()),
                    ("base_url_default", rb.base_url_default.is_some()),
                    ("api_key_env", rb.api_key_env.is_some()),
                ],
                CLAUDE_CODE_CLI,
            );
            Some(BackendSettings::ClaudeCodeCli {
                command: rb.command.clone().unwrap_or_else(|| "claude".into()),
                allowed_tools: rb
                    .allowed_tools
                    .clone()
                    .unwrap_or_else(|| "Read,Grep,Glob".into()),
                permission_mode: rb
                    .permission_mode
                    .clone()
                    .unwrap_or_else(|| "dontAsk".into()),
                max_turns: rb.max_turns.unwrap_or(12),
            })
        }
        OPENAI_COMPATIBLE_API => {
            reject_foreign(
                &[
                    ("command", rb.command.is_some()),
                    ("allowed_tools", rb.allowed_tools.is_some()),
                    ("permission_mode", rb.permission_mode.is_some()),
                    ("max_turns", rb.max_turns.is_some()),
                ],
                OPENAI_COMPATIBLE_API,
            );
            resolve_openai(name, rb, env, &mut problems).map(|(base_url, api_key, var)| {
                BackendSettings::OpenAiCompatible {
                    base_url,
                    api_key,
                    base_url_env: var,
                }
            })
        }
        other => {
            problems.push(format!(
                "backends.{name}: type '{other}' is unknown. Known types: \
                 {CLAUDE_CODE_CLI}, {OPENAI_COMPATIBLE_API}."
            ));
            None
        }
    };

    match (settings, problems.is_empty()) {
        (Some(settings), true) => Ok(Backend {
            name: name.to_string(),
            timeout_seconds,
            settings,
        }),
        _ => Err(problems),
    }
}

/// Resolve `base_url` and the optional key from the environment.
///
/// The precedence is: the variable named by `base_url_env`, then
/// `base_url_default`. That ordering is the requirement — "environment
/// variables override default values" — and it is why the default is allowed to
/// be absent: a backend with no default must be configured before use, and says
/// so at load time rather than at the first request.
fn resolve_openai(
    name: &str,
    rb: &RawBackend,
    env: &dyn Env,
    problems: &mut Vec<String>,
) -> Option<(String, Option<String>, String)> {
    let Some(var) = rb.base_url_env.clone() else {
        problems.push(format!(
            "backends.{name}: `base_url_env` is required for type '{OPENAI_COMPATIBLE_API}' \
             (the name of the variable holding the base URL, not the URL itself)"
        ));
        return None;
    };

    let from_env = env
        .get(&var)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let default = rb
        .base_url_default
        .clone()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());

    let base_url = match from_env.or(default) {
        Some(u) => u,
        None => {
            problems.push(format!(
                "backends.{name}: ${var} is unset and there is no `base_url_default`. \
                 Export ${var} (for example http://127.0.0.1:8000/v1) before using this backend."
            ));
            return None;
        }
    };

    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        problems.push(format!(
            "backends.{name}: base URL '{base_url}' must start with http:// or https://"
        ));
        return None;
    }

    // A key is optional — a local llama-server usually wants none — but a
    // *named* variable that is empty is a different thing from no variable at
    // all: somebody expected authentication. Say so rather than sending an
    // empty bearer token and reading a 401 later.
    let api_key = match &rb.api_key_env {
        None => None,
        Some(key_var) => match env.get(key_var).filter(|v| !v.trim().is_empty()) {
            Some(k) => Some(k),
            None => {
                eprintln!(
                    "ensemble: note: backends.{name} names ${key_var} for its API key, \
                     but it is unset; requests will be sent unauthenticated"
                );
                None
            }
        },
    };

    Some((base_url.trim_end_matches('/').to_string(), api_key, var))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn p() -> PathBuf {
        PathBuf::from("ensemble.yaml")
    }

    /// The document from the README, which is also the shipped example.
    const REFERENCE: &str = r#"
version: 1
backends:
  claude:
    type: claude-code-cli
  openai-local:
    type: openai-compatible-api
    base_url_env: OPENAI_LOCAL_BASE_URL
    base_url_default: "http://127.0.0.1:8000/v1"
    api_key_env: OPENAI_LOCAL_API_KEY
  openai-remote:
    type: openai-compatible-api
    base_url_env: OPENAI_REMOTE_BASE_URL
    base_url_default: "http://10.10.10.5:8000/v1"
    api_key_env: OPENAI_REMOTE_API_KEY
roles:
  planner:
    backend: openai-local
    model: qwen3-coder
  coder:
    backend: openai-remote
    model: qwen3-coder
  reviewer:
    backend: openai-local
    model: qwen3-coder
"#;

    fn problems_of(err: ConfigError) -> Vec<String> {
        match err {
            ConfigError::Invalid { problems, .. } => problems,
            other => panic!("expected Invalid, got: {other}"),
        }
    }

    #[test]
    fn the_reference_config_loads() {
        let c = parse(REFERENCE, &p(), &env(&[])).expect("reference config must load");
        assert_eq!(c.version, 1);
        assert_eq!(c.backends.len(), 3);
        assert_eq!(c.roles.len(), 3);
        assert_eq!(c.role_names(), vec!["coder", "planner", "reviewer"]);
    }

    const AGENT_DOC: &str = r#"
version: 1
backends:
  openai-local:
    type: openai-compatible-api
    base_url_env: OPENAI_LOCAL_BASE_URL
    base_url_default: "http://127.0.0.1:11434/v1"
  claude:
    type: claude-code-cli
roles:
  coder:
    backend: openai-local
    model: m
    tools: [read, grep, edit]
    max_steps: 5
  thinker:
    backend: openai-local
    model: m
"#;

    #[test]
    fn a_role_with_tools_is_an_agent_and_one_without_is_think_only() {
        let c = parse(AGENT_DOC, &p(), &env(&[])).unwrap();
        assert_eq!(
            c.roles["coder"].tools,
            vec![AgentTool::Read, AgentTool::Grep, AgentTool::Edit]
        );
        assert_eq!(c.roles["coder"].max_steps, 5);
        assert!(c.roles["thinker"].tools.is_empty());
        assert_eq!(c.roles["thinker"].max_steps, DEFAULT_MAX_STEPS);
        assert_eq!(c.roles["coder"].context_tokens, DEFAULT_CONTEXT_TOKENS);
    }

    #[test]
    fn an_unknown_tool_or_zero_steps_is_rejected() {
        let doc = AGENT_DOC
            .replace("[read, grep, edit]", "[read, sudo]")
            .replace("max_steps: 5", "max_steps: 0");
        let Err(ConfigError::Invalid { problems, .. }) = parse(&doc, &p(), &env(&[])) else {
            panic!("expected invalid");
        };
        assert!(
            problems
                .iter()
                .any(|p| p.contains("tool 'sudo' is unknown")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("max_steps")),
            "{problems:?}"
        );
    }

    #[test]
    fn context_tokens_is_read_and_zero_is_rejected() {
        let doc = AGENT_DOC.replace("max_steps: 5", "max_steps: 5\n    context_tokens: 4000");
        let c = parse(&doc, &p(), &env(&[])).unwrap();
        assert_eq!(c.roles["coder"].context_tokens, 4000);

        let zero = AGENT_DOC.replace("max_steps: 5", "max_steps: 5\n    context_tokens: 0");
        let Err(ConfigError::Invalid { problems, .. }) = parse(&zero, &p(), &env(&[])) else {
            panic!("expected invalid");
        };
        assert!(
            problems.iter().any(|p| p.contains("context_tokens")),
            "{problems:?}"
        );
    }

    #[test]
    fn bash_is_a_writing_tool_and_network_is_off_unless_asked_for() {
        let doc = AGENT_DOC.replace("[read, grep, edit]", "[read, bash]");
        let c = parse(&doc, &p(), &env(&[])).unwrap();
        assert!(c.roles["coder"].tools.contains(&AgentTool::Bash));
        assert!(AgentTool::Bash.writes());
        assert!(!c.roles["coder"].network);

        let on = doc.replace("max_steps: 5", "max_steps: 5\n    network: true");
        assert!(parse(&on, &p(), &env(&[])).unwrap().roles["coder"].network);
    }

    #[test]
    fn network_without_bash_is_rejected() {
        let doc = AGENT_DOC.replace("max_steps: 5", "max_steps: 5\n    network: true");
        let Err(ConfigError::Invalid { problems, .. }) = parse(&doc, &p(), &env(&[])) else {
            panic!("expected invalid");
        };
        assert!(
            problems.iter().any(|p| p.contains("`network`")),
            "{problems:?}"
        );
    }

    #[test]
    fn tools_on_a_backend_that_cannot_call_tools_are_rejected() {
        let doc = AGENT_DOC.replace(
            "  coder:\n    backend: openai-local",
            "  coder:\n    backend: claude",
        );
        let Err(ConfigError::Invalid { problems, .. }) = parse(&doc, &p(), &env(&[])) else {
            panic!("expected invalid");
        };
        assert!(
            problems.iter().any(|p| p.contains("supports tool calling")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_role_may_override_the_backends_default_temperature() {
        let doc = r#"
version: 1
backends:
  openai-local:
    type: openai-compatible-api
    base_url_env: OPENAI_LOCAL_BASE_URL
    base_url_default: "http://127.0.0.1:11434/v1"
roles:
  scout:
    backend: openai-local
    model: qwen3.5:9b
    temperature: 0.2
  plain:
    backend: openai-local
    model: qwen3.5:9b
"#;
        let c = parse(doc, &p(), &env(&[])).unwrap();
        assert_eq!(c.roles["scout"].temperature, Some(0.2));
        // Unset is unset, not defaulted to something: the backend's own
        // default still applies.
        assert_eq!(c.roles["plain"].temperature, None);
    }

    #[test]
    fn a_temperature_outside_zero_to_two_is_rejected() {
        let doc = r#"
version: 1
backends:
  openai-local:
    type: openai-compatible-api
    base_url_env: OPENAI_LOCAL_BASE_URL
    base_url_default: "http://127.0.0.1:11434/v1"
roles:
  scout:
    backend: openai-local
    model: m
    temperature: 3.5
"#;
        let err = parse(doc, &p(), &env(&[])).unwrap_err();
        let problems = problems_of(err);
        assert!(
            problems.iter().any(|p| p.contains("temperature")),
            "{problems:?}"
        );
    }

    #[test]
    fn env_overrides_the_default_base_url() {
        let c = parse(
            REFERENCE,
            &p(),
            &env(&[("OPENAI_LOCAL_BASE_URL", "http://192.168.1.9:9000/v1")]),
        )
        .unwrap();
        match &c.backends["openai-local"].settings {
            BackendSettings::OpenAiCompatible { base_url, .. } => {
                assert_eq!(base_url, "http://192.168.1.9:9000/v1");
            }
            other => panic!("wrong settings: {other:?}"),
        }
        // The one without an override keeps its default.
        match &c.backends["openai-remote"].settings {
            BackendSettings::OpenAiCompatible { base_url, .. } => {
                assert_eq!(base_url, "http://10.10.10.5:8000/v1");
            }
            other => panic!("wrong settings: {other:?}"),
        }
    }

    #[test]
    fn an_empty_env_var_falls_through_to_the_default() {
        // An exported-but-empty variable is the normal shape of "I meant to set
        // this and did not". Treating "" as an override produces a request to
        // the empty URL, which fails far from here.
        let c = parse(REFERENCE, &p(), &env(&[("OPENAI_LOCAL_BASE_URL", "   ")])).unwrap();
        match &c.backends["openai-local"].settings {
            BackendSettings::OpenAiCompatible { base_url, .. } => {
                assert_eq!(base_url, "http://127.0.0.1:8000/v1")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn api_keys_are_read_from_the_environment_never_the_file() {
        let c = parse(
            REFERENCE,
            &p(),
            &env(&[("OPENAI_LOCAL_API_KEY", "sk-secret")]),
        )
        .unwrap();
        match &c.backends["openai-local"].settings {
            BackendSettings::OpenAiCompatible { api_key, .. } => {
                assert_eq!(api_key.as_deref(), Some("sk-secret"))
            }
            other => panic!("{other:?}"),
        }
        // And an unset one is simply absent, not an empty bearer token.
        match &c.backends["openai-remote"].settings {
            BackendSettings::OpenAiCompatible { api_key, .. } => assert_eq!(*api_key, None),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_inline_api_key_is_refused_by_name() {
        let yaml = r#"
version: 1
backends:
  x:
    type: openai-compatible-api
    base_url_env: X_URL
    base_url_default: "http://127.0.0.1:1/v1"
    api_key: "sk-oops"
"#;
        let err = parse(yaml, &p(), &env(&[])).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("api_key"),
            "must name the offending key: {msg}"
        );
        // And it must not be a silently-ignored field.
        assert!(matches!(err, ConfigError::Syntax { .. }), "{err}");
    }

    #[test]
    fn an_old_manager_block_is_refused_with_a_pointer() {
        let yaml = r#"
version: 1
backends:
  x: { type: openai-compatible-api, base_url_env: X, base_url_default: "http://127.0.0.1:1/v1" }
manager: { backend: x, model: m }
roles:
  coder: { backend: x, model: m }
"#;
        let problems = problems_of(parse(yaml, &p(), &env(&[])).unwrap_err());
        assert!(
            problems.iter().any(|p| p.contains("no manager block")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_role_naming_an_undefined_backend_is_fatal_and_lists_the_real_ones() {
        let yaml = r#"
version: 1
backends:
  openai-local: { type: openai-compatible-api, base_url_env: U, base_url_default: "http://127.0.0.1:1/v1" }
roles:
  coder: { backend: opanai-local, model: m }
"#;
        let problems = problems_of(parse(yaml, &p(), &env(&[])).unwrap_err());
        let joined = problems.join("\n");
        assert!(joined.contains("opanai-local"), "{joined}");
        assert!(
            joined.contains("openai-local"),
            "lists what does exist: {joined}"
        );
    }

    #[test]
    fn a_role_with_an_empty_model_is_fatal() {
        let yaml = r#"
version: 1
backends:
  x: { type: openai-compatible-api, base_url_env: U, base_url_default: "http://127.0.0.1:1/v1" }
roles:
  coder: { backend: x, model: "  " }
"#;
        let problems = problems_of(parse(yaml, &p(), &env(&[])).unwrap_err());
        assert!(
            problems
                .iter()
                .any(|p| p.contains("roles.coder") && p.contains("model")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_role_on_claude_code_is_fine() {
        let yaml = r#"
version: 1
backends:
  claude: { type: claude-code-cli }
  local: { type: openai-compatible-api, base_url_env: U, base_url_default: "http://127.0.0.1:1/v1" }
roles:
  reviewer: { backend: claude, model: claude-sonnet }
"#;
        let c = parse(yaml, &p(), &env(&[])).unwrap();
        assert_eq!(c.roles["reviewer"].backend, "claude");
        assert!(!c.backends["claude"].supports_tool_calling());
    }

    #[test]
    fn an_unknown_backend_type_lists_the_known_ones() {
        let yaml = r#"
version: 1
backends:
  x: { type: ollama }
"#;
        let problems = problems_of(parse(yaml, &p(), &env(&[])).unwrap_err());
        let joined = problems.join("\n");
        assert!(joined.contains("ollama"), "{joined}");
        assert!(
            joined.contains(CLAUDE_CODE_CLI) && joined.contains(OPENAI_COMPATIBLE_API),
            "{joined}"
        );
    }

    #[test]
    fn an_unresolvable_base_url_names_the_variable_to_export() {
        let yaml = r#"
version: 1
backends:
  remote: { type: openai-compatible-api, base_url_env: OPENAI_REMOTE_BASE_URL }
"#;
        let problems = problems_of(parse(yaml, &p(), &env(&[])).unwrap_err());
        let joined = problems.join("\n");
        assert!(joined.contains("OPENAI_REMOTE_BASE_URL"), "{joined}");
        assert!(joined.contains("Export"), "tells you the fix: {joined}");
    }

    #[test]
    fn a_base_url_without_a_scheme_is_refused() {
        let yaml = r#"
version: 1
backends:
  x: { type: openai-compatible-api, base_url_env: U, base_url_default: "127.0.0.1:8000/v1" }
"#;
        let problems = problems_of(parse(yaml, &p(), &env(&[])).unwrap_err());
        assert!(
            problems.iter().any(|p| p.contains("http://")),
            "{problems:?}"
        );
    }

    #[test]
    fn trailing_slashes_are_normalised_away() {
        let yaml = r#"
version: 1
backends:
  x: { type: openai-compatible-api, base_url_env: U, base_url_default: "http://127.0.0.1:8000/v1/" }
"#;
        let c = parse(yaml, &p(), &env(&[])).unwrap();
        match &c.backends["x"].settings {
            BackendSettings::OpenAiCompatible { base_url, .. } => {
                assert_eq!(base_url, "http://127.0.0.1:8000/v1")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn settings_from_the_wrong_backend_type_are_refused_not_ignored() {
        let yaml = r#"
version: 1
backends:
  claude: { type: claude-code-cli, base_url_env: NOPE }
  x: { type: openai-compatible-api, base_url_env: U, base_url_default: "http://127.0.0.1:1/v1", max_turns: 3 }
"#;
        let problems = problems_of(parse(yaml, &p(), &env(&[])).unwrap_err());
        let joined = problems.join("\n");
        assert!(joined.contains("base_url_env"), "{joined}");
        assert!(joined.contains("max_turns"), "{joined}");
    }

    #[test]
    fn an_unsupported_version_is_refused() {
        let yaml = r#"
version: 2
backends:
  x: { type: openai-compatible-api, base_url_env: U, base_url_default: "http://127.0.0.1:1/v1" }
"#;
        let problems = problems_of(parse(yaml, &p(), &env(&[])).unwrap_err());
        assert!(
            problems.iter().any(|p| p.contains("version")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_typo_in_a_role_key_is_an_error_not_a_default() {
        let yaml = r#"
version: 1
backends:
  x: { type: openai-compatible-api, base_url_env: U, base_url_default: "http://127.0.0.1:1/v1" }
roles:
  coder: { backend: x, modle: m }
"#;
        let err = parse(yaml, &p(), &env(&[])).unwrap_err();
        assert!(matches!(err, ConfigError::Syntax { .. }), "{err}");
        assert!(err.to_string().contains("modle"), "{err}");
    }

    #[test]
    fn every_independent_problem_is_reported_in_one_pass() {
        let yaml = r#"
version: 9
backends:
  x: { type: nonsense }
roles:
  r: { backend: missing, model: "" }
"#;
        let problems = problems_of(parse(yaml, &p(), &env(&[])).unwrap_err());
        assert!(problems.len() >= 4, "expected several, got: {problems:#?}");
    }

    #[test]
    fn zero_timeout_is_refused() {
        let yaml = r#"
version: 1
backends:
  x: { type: openai-compatible-api, base_url_env: U, base_url_default: "http://127.0.0.1:1/v1", timeout_seconds: 0 }
"#;
        let problems = problems_of(parse(yaml, &p(), &env(&[])).unwrap_err());
        assert!(
            problems.iter().any(|p| p.contains("timeout_seconds")),
            "{problems:?}"
        );
    }

    #[test]
    fn claude_code_defaults_are_read_only() {
        let yaml = r#"
version: 1
backends:
  claude: { type: claude-code-cli }
  x: { type: openai-compatible-api, base_url_env: U, base_url_default: "http://127.0.0.1:1/v1" }
roles:
  planner: { backend: claude, model: claude-sonnet }
"#;
        let c = parse(yaml, &p(), &env(&[])).unwrap();
        match &c.backends["claude"].settings {
            BackendSettings::ClaudeCodeCli {
                allowed_tools,
                command,
                max_turns,
                ..
            } => {
                assert_eq!(allowed_tools, "Read,Grep,Glob");
                assert_eq!(command, "claude");
                assert_eq!(*max_turns, 12);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            c.backends["claude"].timeout_seconds,
            DEFAULT_TIMEOUT_SECONDS
        );
    }

    #[test]
    fn assignment_and_backend_lookup_find_roles() {
        let c = parse(REFERENCE, &p(), &env(&[])).unwrap();
        assert_eq!(c.assignment("coder").unwrap().role, "coder");
        assert_eq!(c.backend_for("coder").unwrap().name, "openai-remote");
        assert!(c.assignment("nope").is_none());
        assert!(c.assignment("manager").is_none());
    }
}
