//! `.ensemble/config.yaml`: load, resolve the environment, validate.
//!
//! Validation is deliberately loud and early. Every check here corresponds to a
//! failure that would otherwise surface much later and much less clearly — as a
//! 404 on the first request to an agent, as an agent that quietly runs with
//! default settings, or as an empty base URL that turns into a connection refused.
//!
//! Secrets never live in this file. An API key is named by the *variable* that
//! holds it (`api_key_env`), and the value is read from the environment at load
//! time. There is no `api_key` field; a config that tries one is rejected by
//! name so the mistake is caught before the file is committed. The values can
//! also live in `.ensemble/.env`, beside the config and out of git.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// Everything Ensemble keeps in a project lives in this directory.
pub const CONFIG_DIR: &str = ".ensemble";
pub const CONFIG_FILENAME: &str = "config.yaml";
/// Variables for the config's `*_env` settings, beside the config file.
pub const ENV_FILENAME: &str = ".env";

/// The JSON Schema for `config.yaml`, for editors. `ensemble schema`
/// prints it; the tests keep it in step with the structs below.
pub const SCHEMA: &str = include_str!("../ensemble.schema.json");

/// The config `ensemble init` writes: every setting at least once, with what
/// it does. The tests keep it loading and complete.
pub const SAMPLE: &str = include_str!("sample.yaml");
pub const SUPPORTED_VERSION: u32 = 1;

/// The LLM backend types that ship. Adding one is adding an adapter and a
/// variant.
pub const OPENAI_COMPATIBLE_API: &str = "openai-compatible-api";

/// The adapter types that ship: the Claude Code and OpenCode CLIs, whole
/// agents of their own that Ensemble starts in the project.
pub const CLAUDE_CODE_AGENT: &str = "claude-code";
pub const OPENCODE_AGENT: &str = "opencode";
const AGENT_TYPES: [&str; 2] = [CLAUDE_CODE_AGENT, OPENCODE_AGENT];

/// The type of Ensemble's former built-in harness, which no longer exists.
/// Named only to tell someone whose config still uses it where to go.
const REMOVED_ENSEMBLE_AGENT: &str = "ensemble";

/// OpenCode tools an `opencode` agent gets when it names none: it may look,
/// not change anything.
pub const OPENCODE_DEFAULT_TOOLS: [&str; 4] = ["read", "glob", "grep", "list"];
const OPENCODE_DEFAULT_MAX_STEPS: u32 = 30;

/// Claude Code tools a `claude-code` agent gets when it names none: it may
/// look, not change anything.
pub const CLAUDE_CODE_DEFAULT_TOOLS: [&str; 3] = ["Read", "Grep", "Glob"];
const CLAUDE_CODE_DEFAULT_PERMISSION_MODE: &str = "dontAsk";
const CLAUDE_CODE_DEFAULT_MAX_TURNS: u32 = 30;

// ---------------------------------------------------------------------------
// The document, as written
// ---------------------------------------------------------------------------

/// `deny_unknown_fields` throughout: a typo'd key is a key that does nothing,
/// and an agent silently running with default settings is the failure mode
/// this whole module exists to prevent. `reviewr:` should be an error, not a
/// no-op.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    version: u32,
    /// Model endpoints. An agent names one with `llm_backend`, and Ensemble
    /// hands its harness the endpoint, so it is defined once, here.
    #[serde(default)]
    llm_backends: BTreeMap<String, RawLlmBackend>,
    /// The team.
    #[serde(default)]
    agents: BTreeMap<String, RawAgent>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLlmBackend {
    #[serde(rename = "type")]
    type_id: String,

    base_url_env: Option<String>,
    base_url_default: Option<String>,
    api_key_env: Option<String>,
    timeout_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgent {
    /// What the agent is for. Its first line is what the calling agent sees
    /// when it lists the team, and the whole of it is the agent's system
    /// prompt. Optional: an agent with no prompt is a model with a name.
    #[serde(default)]
    prompt: Option<String>,
    /// Tokens of context the model's server really runs it with. OpenCode
    /// needs it to know when to compact the conversation, so it is required
    /// with an `llm_backend` and does not apply without one, where OpenCode
    /// knows its own models.
    #[serde(default)]
    context_window: Option<u32>,
    /// Wall-clock cap on one delegation, in seconds. Defaults to
    /// [`DEFAULT_MAX_SECONDS`]. A run that reaches it stops and reports what
    /// it did, so a hung model cannot hold the caller forever.
    #[serde(default)]
    max_seconds: Option<u32>,
    /// What runs the agent, and how.
    adapter: RawAdapter,
}

/// The harness behind an agent. `type` decides which of the other fields
/// apply; one that belongs to another type is refused, not ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAdapter {
    /// `claude-code` or `opencode`.
    #[serde(rename = "type")]
    kind: String,
    /// All: the model. As Claude Code names it, as OpenCode names it
    /// (`provider/model`), or, with an `llm_backend`, the id the endpoint
    /// serves it under.
    model: String,
    /// All: what the agent may do, in its harness's own tool names: Claude
    /// Code's (`Read`, `Bash(git:*)`) for `claude-code`, OpenCode's
    /// permissions (`read`, `bash(git *)`) for `opencode`.
    #[serde(default)]
    tools: Option<Vec<String>>,
    /// The CLI, if it is not `claude` or `opencode` on PATH.
    #[serde(default)]
    command: Option<String>,

    // --- opencode ---
    /// Which of `llm_backends` OpenCode runs the model on. Ensemble gives
    /// OpenCode the endpoint and key for the run. Without it the model is
    /// one OpenCode is already set up for.
    #[serde(default)]
    llm_backend: Option<String>,
    /// Cap on steps within one delegation.
    #[serde(default)]
    max_steps: Option<u32>,

    // --- claude-code ---
    /// Claude Code's `--permission-mode`. `dontAsk` denies what `tools` does
    /// not allow instead of waiting for a person who is not there.
    #[serde(default)]
    permission_mode: Option<String>,
    /// Most agent turns Claude Code may take for one delegation.
    #[serde(default)]
    max_turns: Option<u32>,
}

// ---------------------------------------------------------------------------
// The document, resolved
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendSettings {
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
pub struct LlmBackend {
    pub name: String,
    pub timeout_seconds: u64,
    pub settings: BackendSettings,
}

impl LlmBackend {
    pub fn type_id(&self) -> &'static str {
        match self.settings {
            BackendSettings::OpenAiCompatible { .. } => OPENAI_COMPATIBLE_API,
        }
    }
}

pub const DEFAULT_MAX_SECONDS: u32 = 1800;

/// One member of the team, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    pub name: String,
    pub prompt: Option<String>,
    /// See [`RawAgent::context_window`]. Set exactly when the agent runs on
    /// one of the LLM backends.
    pub context_window: Option<u32>,
    /// See [`RawAgent::max_seconds`].
    pub max_seconds: u32,
    pub adapter: Adapter,
}

/// What runs an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Adapter {
    ClaudeCode(ClaudeCodeAdapter),
    OpenCode(OpenCodeAdapter),
}

/// The Claude Code CLI, run headless in the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeAdapter {
    pub command: String,
    pub model: String,
    /// Claude Code's own tool names and rules, passed to `--allowedTools`.
    pub tools: Vec<String>,
    pub permission_mode: String,
    pub max_turns: u32,
}

/// The OpenCode CLI, run headless in the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCodeAdapter {
    pub command: String,
    /// `provider/model`, as OpenCode names it; with `llm_backend`, the id
    /// the endpoint serves the model under.
    pub model: String,
    /// Which of [`Config::llm_backends`] the model runs on, if any.
    pub llm_backend: Option<String>,
    /// OpenCode permission names, each allowed; everything else is denied.
    /// `name(pattern)` allows only matching uses, such as `bash(git *)`.
    pub tools: Vec<String>,
    pub max_steps: u32,
}

impl AgentConfig {
    pub fn kind(&self) -> &'static str {
        match self.adapter {
            Adapter::ClaudeCode(_) => CLAUDE_CODE_AGENT,
            Adapter::OpenCode(_) => OPENCODE_AGENT,
        }
    }

    pub fn model(&self) -> &str {
        match &self.adapter {
            Adapter::ClaudeCode(c) => &c.model,
            Adapter::OpenCode(o) => &o.model,
        }
    }

    /// The LLM backend its model runs on, when it runs on one.
    pub fn llm_backend(&self) -> Option<&str> {
        match &self.adapter {
            Adapter::ClaudeCode(_) => None,
            Adapter::OpenCode(o) => o.llm_backend.as_deref(),
        }
    }

    /// Where it runs: the adapter type, and the LLM backend when there is one.
    pub fn runs_on(&self) -> String {
        match self.llm_backend() {
            Some(backend) => format!("{} on {backend}", self.kind()),
            None => self.kind().to_string(),
        }
    }

    /// Does it have any tool at all?
    pub fn has_tools(&self) -> bool {
        !self.tool_names().is_empty()
    }

    /// Can it change files?
    pub fn can_change_files(&self) -> bool {
        match &self.adapter {
            Adapter::ClaudeCode(c) => c.tools.iter().any(|t| claude_tool_writes(t)),
            Adapter::OpenCode(o) => o.tools.iter().any(|t| opencode_tool_writes(t)),
        }
    }

    /// Its tools, by the names its adapter uses.
    pub fn tool_names(&self) -> Vec<&str> {
        match &self.adapter {
            Adapter::ClaudeCode(c) => c.tools.iter().map(String::as_str).collect(),
            Adapter::OpenCode(o) => o.tools.iter().map(String::as_str).collect(),
        }
    }
}

/// Can this OpenCode permission (or rule, such as `bash(git *)`) change
/// files? `task` starts subagents, which can.
pub fn opencode_tool_writes(tool: &str) -> bool {
    let name = tool.split('(').next().unwrap_or(tool).trim();
    matches!(name, "edit" | "write" | "patch" | "bash" | "task" | "*")
}

/// Can this Claude Code tool (or rule, such as `Bash(git:*)`) change files?
pub fn claude_tool_writes(tool: &str) -> bool {
    let name = tool.split('(').next().unwrap_or(tool).trim();
    matches!(
        name,
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" | "Bash"
    )
}

#[derive(Debug, Clone)]
pub struct Config {
    pub version: u32,
    pub llm_backends: BTreeMap<String, LlmBackend>,
    /// The team. There is no manager: the agent that calls Ensemble is it.
    pub agents: BTreeMap<String, AgentConfig>,
    /// Where it was loaded from, for error messages.
    pub source: PathBuf,
}

impl Config {
    pub fn agent(&self, name: &str) -> Option<&AgentConfig> {
        self.agents.get(name)
    }

    pub fn agent_names(&self) -> Vec<&str> {
        self.agents.keys().map(String::as_str).collect()
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

/// Where a project's config lives: `.ensemble/config.yaml`.
pub fn config_path(project: &Path) -> PathBuf {
    project.join(CONFIG_DIR).join(CONFIG_FILENAME)
}

/// The project's own `.ensemble/config.yaml`, and nothing else.
///
/// Parent directories are deliberately not searched: a team is a property of
/// one project, and a file in `~/projects/` silently configuring every
/// checkout below it is exactly the shared, out-of-sight config this rules
/// out. There is no user-level fallback either.
pub fn discover(project: &Path) -> Option<PathBuf> {
    let candidate = config_path(project);
    candidate.is_file().then_some(candidate)
}

/// The `.env` beside a config file.
pub fn env_path(config: &Path) -> PathBuf {
    config.with_file_name(ENV_FILENAME)
}

/// The real environment, then the `.env` file: a variable that is set wins,
/// as dotenv files are meant to behave.
struct Layered<'a> {
    first: &'a dyn Env,
    then: &'a BTreeMap<String, String>,
}

impl Env for Layered<'_> {
    fn get(&self, key: &str) -> Option<String> {
        self.first.get(key).or_else(|| self.then.get(key).cloned())
    }
}

/// Read a `.env` file: `NAME=value` per line, `#` comments, an optional
/// `export ` prefix, values optionally in single or double quotes. No
/// interpolation. Every malformed line is reported.
pub fn parse_dotenv(text: &str, path: &Path) -> Result<BTreeMap<String, String>, ConfigError> {
    let mut vars = BTreeMap::new();
    let mut problems = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((name, value)) = line.split_once('=') else {
            problems.push(format!("line {}: expected NAME=value", i + 1));
            continue;
        };
        let name = name.trim();
        let valid = name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            problems.push(format!("line {}: '{name}' is not a variable name", i + 1));
            continue;
        }
        let value = value.trim();
        let value = if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            inner
                .replace("\\n", "\n")
                .replace("\\\"", "\"")
                .replace("\\\\", "\\")
        } else if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
            inner.to_string()
        } else {
            // An unquoted value ends at a comment.
            value
                .split_once(" #")
                .map_or(value, |(v, _)| v)
                .trim_end()
                .to_string()
        };
        vars.insert(name.to_string(), value);
    }
    if problems.is_empty() {
        Ok(vars)
    } else {
        Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            problems,
        })
    }
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
    let dotenv = env_path(path);
    let file_vars = if dotenv.is_file() {
        let vars = std::fs::read_to_string(&dotenv).map_err(|e| ConfigError::Unreadable {
            path: dotenv.clone(),
            detail: e.to_string(),
        })?;
        parse_dotenv(&vars, &dotenv)?
    } else {
        BTreeMap::new()
    };
    parse(
        &text,
        path,
        &Layered {
            first: env,
            then: &file_vars,
        },
    )
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

    // --- backends ---
    let mut backends: BTreeMap<String, LlmBackend> = BTreeMap::new();
    for (name, rb) in &raw.llm_backends {
        match resolve_backend(name, rb, env) {
            Ok(b) => {
                backends.insert(name.clone(), b);
            }
            Err(mut errs) => problems.append(&mut errs),
        }
    }

    // --- agents ---
    let mut agents: BTreeMap<String, AgentConfig> = BTreeMap::new();
    for (name, ra) in &raw.agents {
        if name.trim().is_empty() {
            problems.push("agents: an agent name cannot be empty".into());
            continue;
        }
        let ad = &ra.adapter;
        let at = format!("agents.{name}.adapter");
        if ad.model.trim().is_empty() {
            problems.push(format!("{at}: `model` is empty"));
        }
        if ra.max_seconds == Some(0) {
            problems.push(format!(
                "agents.{name}: `max_seconds` must be greater than 0"
            ));
        }
        if ra.context_window == Some(0) {
            problems.push(format!(
                "agents.{name}: `context_window` must be greater than 0"
            ));
        }
        let adapter = match ad.kind.as_str() {
            CLAUDE_CODE_AGENT => {
                if ra.context_window.is_some() {
                    problems.push(format!(
                        "agents.{name}: `context_window` does not apply to a {CLAUDE_CODE_AGENT} agent; Claude Code manages its own context"
                    ));
                }
                Adapter::ClaudeCode(claude_code_adapter(&at, ad, &mut problems))
            }
            OPENCODE_AGENT => {
                let oc = opencode_adapter(&at, ad, &raw.llm_backends, &mut problems);
                match (&oc.llm_backend, ra.context_window) {
                    (Some(_), None) => problems.push(format!(
                        "agents.{name}: an agent on an `llm_backend` needs `context_window`, the tokens of context the server runs the model with, so OpenCode knows when to compact"
                    )),
                    (None, Some(_)) => problems.push(format!(
                        "agents.{name}: `context_window` only applies with `llm_backend`; OpenCode knows the context of the models it is set up for"
                    )),
                    _ => {}
                }
                Adapter::OpenCode(oc)
            }
            REMOVED_ENSEMBLE_AGENT => {
                problems.push(format!(
                    "{at}: type '{REMOVED_ENSEMBLE_AGENT}' was removed; Ensemble no longer runs agents itself. To run this model, use type '{OPENCODE_AGENT}' with `llm_backend`."
                ));
                continue;
            }
            other => {
                problems.push(format!(
                    "{at}: type '{other}' is unknown. Known types: {}.",
                    AGENT_TYPES.join(", ")
                ));
                continue;
            }
        };
        agents.insert(
            name.clone(),
            AgentConfig {
                name: name.clone(),
                prompt: ra.prompt.clone(),
                context_window: ra.context_window,
                max_seconds: ra.max_seconds.unwrap_or(DEFAULT_MAX_SECONDS),
                adapter,
            },
        );
    }

    // A backend nothing uses is not an error — it is a spare, and deleting it
    // from a shared config is how you break someone else's profile. Say so once
    // and carry on.
    for name in backends.keys() {
        let used = agents.values().any(|a| a.llm_backend() == Some(name));
        if !used {
            eprintln!("ensemble: note: LLM backend '{name}' is defined but no agent uses it");
        }
    }

    if problems.is_empty() {
        Ok(Config {
            version: raw.version,
            llm_backends: backends,
            agents,
            source: path.to_path_buf(),
        })
    } else {
        Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            problems,
        })
    }
}

/// Refuse fields that belong to another adapter type rather than ignoring
/// them: an `llm_backend` on a `claude-code` agent means someone believes a
/// model server is being used, and it is not.
fn reject_foreign(at: &str, kind: &str, keys: &[(&str, bool)], problems: &mut Vec<String>) {
    for (key, present) in keys {
        if *present {
            problems.push(format!(
                "{at}: `{key}` is not a field of adapter type '{kind}'"
            ));
        }
    }
}

fn opencode_adapter(
    at: &str,
    ad: &RawAdapter,
    declared: &BTreeMap<String, RawLlmBackend>,
    problems: &mut Vec<String>,
) -> OpenCodeAdapter {
    reject_foreign(
        at,
        OPENCODE_AGENT,
        &[
            ("permission_mode", ad.permission_mode.is_some()),
            ("max_turns", ad.max_turns.is_some()),
        ],
        problems,
    );
    let tools = match &ad.tools {
        Some(list) => list.clone(),
        None => OPENCODE_DEFAULT_TOOLS.map(String::from).to_vec(),
    };
    for tool in &tools {
        let (name, pattern) = match tool.split_once('(') {
            Some((n, rest)) => (n, rest.strip_suffix(')')),
            None => (tool.as_str(), Some("")),
        };
        let name_ok = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-*.".contains(c));
        if !name_ok || pattern.is_none_or(|p| p.contains(')')) {
            problems.push(format!(
                "{at}: tool '{tool}' must be an OpenCode permission, such as read or edit, or a rule such as bash(git *)"
            ));
        }
    }
    if ad.max_steps == Some(0) {
        problems.push(format!("{at}: `max_steps` must be greater than 0"));
    }
    match &ad.llm_backend {
        Some(backend) if !declared.contains_key(backend) => {
            let known: Vec<&str> = declared.keys().map(String::as_str).collect();
            problems.push(format!(
                "{at}: LLM backend '{backend}' is not defined. Defined LLM backends: {}.",
                if known.is_empty() {
                    "(none)".to_string()
                } else {
                    known.join(", ")
                }
            ));
        }
        // On a backend the model is the id the endpoint serves; Ensemble
        // names the provider.
        Some(_) => {}
        None if !ad.model.contains('/') => problems.push(format!(
            "{at}: model '{}' must be provider/model, as `opencode models` lists it, or name an `llm_backend` to run it on",
            ad.model
        )),
        None => {}
    }
    let command = ad.command.clone().unwrap_or_else(|| "opencode".into());
    if command.trim().is_empty() {
        problems.push(format!("{at}: `command` is empty"));
    }
    OpenCodeAdapter {
        command,
        model: ad.model.clone(),
        llm_backend: ad.llm_backend.clone(),
        tools,
        max_steps: ad.max_steps.unwrap_or(OPENCODE_DEFAULT_MAX_STEPS),
    }
}

fn claude_code_adapter(at: &str, ad: &RawAdapter, problems: &mut Vec<String>) -> ClaudeCodeAdapter {
    if ad.llm_backend.is_some() {
        problems.push(format!(
            "{at}: `llm_backend` does not apply to a {CLAUDE_CODE_AGENT} agent; Claude Code runs Anthropic models, not an OpenAI-compatible endpoint"
        ));
    }
    reject_foreign(
        at,
        CLAUDE_CODE_AGENT,
        &[("max_steps", ad.max_steps.is_some())],
        problems,
    );
    let tools = match &ad.tools {
        Some(list) => list.clone(),
        None => CLAUDE_CODE_DEFAULT_TOOLS.map(String::from).to_vec(),
    };
    if let Some(bad) = tools
        .iter()
        .find(|t| t.trim().is_empty() || t.contains(','))
    {
        problems.push(format!(
            "{at}: tool '{bad}' must be one Claude Code tool or rule per list entry, such as Read or Bash(git:*)"
        ));
    }
    if ad.max_turns == Some(0) {
        problems.push(format!("{at}: `max_turns` must be greater than 0"));
    }
    let command = ad.command.clone().unwrap_or_else(|| "claude".into());
    if command.trim().is_empty() {
        problems.push(format!("{at}: `command` is empty"));
    }
    ClaudeCodeAdapter {
        command,
        model: ad.model.clone(),
        tools,
        permission_mode: ad
            .permission_mode
            .clone()
            .unwrap_or_else(|| CLAUDE_CODE_DEFAULT_PERMISSION_MODE.into()),
        max_turns: ad.max_turns.unwrap_or(CLAUDE_CODE_DEFAULT_MAX_TURNS),
    }
}

fn resolve_backend(
    name: &str,
    rb: &RawLlmBackend,
    env: &dyn Env,
) -> Result<LlmBackend, Vec<String>> {
    let mut problems = Vec::new();
    let timeout_seconds = rb.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    if timeout_seconds == 0 {
        problems.push(format!(
            "llm_backends.{name}: `timeout_seconds` must be greater than 0"
        ));
    }

    let settings = match rb.type_id.as_str() {
        OPENAI_COMPATIBLE_API => {
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
                "llm_backends.{name}: type '{other}' is unknown. Known types: \
                 {OPENAI_COMPATIBLE_API}."
            ));
            None
        }
    };

    match (settings, problems.is_empty()) {
        (Some(settings), true) => Ok(LlmBackend {
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
    rb: &RawLlmBackend,
    env: &dyn Env,
    problems: &mut Vec<String>,
) -> Option<(String, Option<String>, String)> {
    let Some(var) = rb.base_url_env.clone() else {
        problems.push(format!(
            "llm_backends.{name}: `base_url_env` is required for type '{OPENAI_COMPATIBLE_API}' \
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
                "llm_backends.{name}: ${var} is unset and there is no `base_url_default`. \
                 Export ${var} (for example http://127.0.0.1:8000/v1) before using this backend."
            ));
            return None;
        }
    };

    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        problems.push(format!(
            "llm_backends.{name}: base URL '{base_url}' must start with http:// or https://"
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
                    "ensemble: note: llm_backends.{name} names ${key_var} for its API key, \
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
        PathBuf::from("config.yaml")
    }

    fn opencode(c: &Config, agent: &str) -> OpenCodeAdapter {
        match &c.agents[agent].adapter {
            Adapter::OpenCode(oc) => oc.clone(),
            other => panic!("not an opencode agent: {other:?}"),
        }
    }

    /// OpenCode agents on two LLM backends, and one Claude Code agent.
    const REFERENCE: &str = r#"
version: 1
llm_backends:
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
agents:
  planner:
    context_window: 32768
    adapter:
      type: opencode
      llm_backend: openai-local
      model: qwen3-coder
  coder:
    context_window: 32768
    adapter:
      type: opencode
      llm_backend: openai-remote
      model: qwen3-coder
      tools: [read, edit, bash]
  reviewer:
    adapter:
      type: claude-code
      model: sonnet
"#;

    fn problems_of(err: ConfigError) -> Vec<String> {
        match err {
            ConfigError::Invalid { problems, .. } => problems,
            other => panic!("expected Invalid, got: {other}"),
        }
    }

    fn joined_problems(doc: &str) -> String {
        problems_of(parse(doc, &p(), &env(&[])).unwrap_err()).join("\n")
    }

    #[test]
    fn the_reference_config_loads() {
        let c = parse(REFERENCE, &p(), &env(&[])).expect("reference config must load");
        assert_eq!(c.version, 1);
        assert_eq!(c.llm_backends.len(), 2);
        assert_eq!(c.agent_names(), vec!["coder", "planner", "reviewer"]);
        let coder = &c.agents["coder"];
        assert_eq!(coder.llm_backend(), Some("openai-remote"));
        assert_eq!(coder.runs_on(), "opencode on openai-remote");
        assert_eq!(coder.context_window, Some(32768));
        assert_eq!(coder.max_seconds, DEFAULT_MAX_SECONDS);
        assert!(coder.can_change_files());
        assert_eq!(c.agents["reviewer"].llm_backend(), None);
    }

    #[test]
    fn the_removed_ensemble_type_points_to_opencode() {
        let doc = REFERENCE.replace(
            "    adapter:\n      type: claude-code\n      model: sonnet",
            "    adapter:\n      type: ensemble\n      model: sonnet",
        );
        let joined = joined_problems(&doc);
        assert!(
            joined.contains("agents.reviewer.adapter: type 'ensemble' was removed")
                && joined.contains("type 'opencode' with `llm_backend`"),
            "{joined}"
        );
    }

    #[test]
    fn settings_of_the_removed_harness_are_unknown_fields() {
        let doc = REFERENCE.replace(
            "      model: qwen3-coder\n      tools:",
            "      model: qwen3-coder\n      temperature: 0.2\n      tools:",
        );
        let err = parse(&doc, &p(), &env(&[])).unwrap_err();
        assert!(matches!(err, ConfigError::Syntax { .. }), "{err}");
        assert!(err.to_string().contains("temperature"), "{err}");
    }

    #[test]
    fn context_window_goes_with_an_llm_backend_and_only_with_one() {
        let missing = REFERENCE.replacen("    context_window: 32768\n", "", 1);
        assert!(
            joined_problems(&missing)
                .contains("agents.planner: an agent on an `llm_backend` needs `context_window`"),
            "{}",
            joined_problems(&missing)
        );

        let zero = REFERENCE.replacen("context_window: 32768", "context_window: 0", 1);
        assert!(joined_problems(&zero).contains("greater than 0"));

        let without_backend = format!(
            "{REFERENCE}  looker:\n    context_window: 8000\n    adapter: {{ type: opencode, model: opencode/big-pickle }}\n"
        );
        assert!(
            joined_problems(&without_backend)
                .contains("agents.looker: `context_window` only applies with `llm_backend`"),
            "{}",
            joined_problems(&without_backend)
        );
    }

    #[test]
    fn an_agent_naming_an_undefined_backend_is_fatal_and_lists_the_real_ones() {
        let doc = REFERENCE.replace("llm_backend: openai-remote", "llm_backend: opanai-remote");
        let joined = joined_problems(&doc);
        assert!(
            joined.contains("LLM backend 'opanai-remote' is not defined")
                && joined.contains("openai-local, openai-remote"),
            "{joined}"
        );
    }

    #[test]
    fn a_backend_no_agent_uses_is_not_an_error() {
        let doc = REFERENCE.replace("llm_backend: openai-remote", "llm_backend: openai-local");
        assert!(parse(&doc, &p(), &env(&[])).is_ok());
    }

    /// The keys serde accepts for `T`, read from its unknown-field error.
    fn accepted_keys<T: serde::de::DeserializeOwned>() -> Vec<String> {
        let err = serde_norway::from_str::<T>("zzz_unknown: 1")
            .err()
            .expect("an unknown key is refused")
            .to_string();
        let list = err.split("expected").nth(1).expect("the error lists keys");
        let mut keys: Vec<String> = list
            .split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect();
        keys.sort();
        keys
    }

    fn schema() -> serde_json::Value {
        serde_json::from_str(SCHEMA).expect("the schema is JSON")
    }

    fn schema_keys(node: &serde_json::Value) -> Vec<String> {
        let mut keys: Vec<String> = node["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    fn schema_enum(node: &serde_json::Value) -> Vec<String> {
        node["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_schema_names_exactly_the_keys_the_config_accepts() {
        let schema = schema();
        assert_eq!(
            schema_keys(&schema),
            accepted_keys::<RawConfig>(),
            "top level"
        );
        let defs = &schema["definitions"];
        assert_eq!(
            schema_keys(&defs["llm_backend"]),
            accepted_keys::<RawLlmBackend>(),
            "llm_backends"
        );
        assert_eq!(
            schema_keys(&defs["agent"]),
            accepted_keys::<RawAgent>(),
            "agents"
        );
        // Every adapter key belongs to exactly one adapter type, or to both.
        let mut adapters = schema_keys(&defs["claude_code_adapter"]);
        adapters.extend(schema_keys(&defs["opencode_adapter"]));
        adapters.sort();
        adapters.dedup();
        assert_eq!(adapters, accepted_keys::<RawAdapter>(), "adapters");
    }

    #[test]
    fn the_schemas_choices_and_defaults_are_the_codes() {
        let schema = schema();
        let backend = &schema["definitions"]["llm_backend"]["properties"];
        let agent = &schema["definitions"]["agent"]["properties"];
        let claude = &schema["definitions"]["claude_code_adapter"]["properties"];
        assert_eq!(claude["type"]["const"], CLAUDE_CODE_AGENT);
        assert_eq!(AGENT_TYPES, [CLAUDE_CODE_AGENT, OPENCODE_AGENT]);
        let opencode = &schema["definitions"]["opencode_adapter"]["properties"];
        assert_eq!(opencode["type"]["const"], OPENCODE_AGENT);
        assert_eq!(
            opencode["tools"]["default"],
            serde_json::json!(OPENCODE_DEFAULT_TOOLS)
        );
        assert_eq!(opencode["max_steps"]["default"], OPENCODE_DEFAULT_MAX_STEPS);
        assert_eq!(
            claude["tools"]["default"],
            serde_json::json!(CLAUDE_CODE_DEFAULT_TOOLS)
        );
        assert_eq!(
            claude["permission_mode"]["default"],
            CLAUDE_CODE_DEFAULT_PERMISSION_MODE
        );
        assert_eq!(
            claude["max_turns"]["default"],
            CLAUDE_CODE_DEFAULT_MAX_TURNS
        );

        let mut types = schema_enum(&backend["type"]);
        types.sort();
        assert_eq!(types, [OPENAI_COMPATIBLE_API]);

        assert_eq!(agent["max_seconds"]["default"], DEFAULT_MAX_SECONDS);
        assert_eq!(
            backend["timeout_seconds"]["default"],
            DEFAULT_TIMEOUT_SECONDS
        );
        assert_eq!(schema["properties"]["version"]["const"], SUPPORTED_VERSION);
    }

    #[test]
    fn a_dotenv_file_is_read_with_comments_quotes_and_export() {
        let text =
            "# keys\nexport A_KEY=sk-1 # trailing\n\nB=\"two words\"\nC='it#is'\nD=\nE = spaced \n";
        let vars = parse_dotenv(text, Path::new(".env")).unwrap();
        assert_eq!(vars["A_KEY"], "sk-1");
        assert_eq!(vars["B"], "two words");
        assert_eq!(vars["C"], "it#is");
        assert_eq!(vars["D"], "");
        assert_eq!(vars["E"], "spaced");
        assert_eq!(vars.len(), 5);
    }

    #[test]
    fn a_malformed_dotenv_line_is_reported_with_its_number() {
        let Err(ConfigError::Invalid { problems, .. }) =
            parse_dotenv("OK=1\njust words\n9X=1\n", Path::new(".env"))
        else {
            panic!("expected invalid");
        };
        assert_eq!(
            problems,
            [
                "line 2: expected NAME=value",
                "line 3: '9X' is not a variable name"
            ]
        );
    }

    #[test]
    fn the_dotenv_beside_the_config_fills_in_what_the_environment_lacks() {
        let tmp = crate::workspace::testing::TempDir::new();
        let dir = tmp.0.join(CONFIG_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(CONFIG_FILENAME),
            REFERENCE.replace("    api_key_env: OPENAI_REMOTE_API_KEY\n", ""),
        )
        .unwrap();
        std::fs::write(
            dir.join(ENV_FILENAME),
            "OPENAI_LOCAL_API_KEY=from-file\nOPENAI_REMOTE_BASE_URL=http://10.0.0.1:1/v1\n",
        )
        .unwrap();
        let path = discover(&tmp.0).expect("found in .ensemble");
        let url = |c: &Config, name: &str| match &c.llm_backends[name].settings {
            BackendSettings::OpenAiCompatible {
                base_url, api_key, ..
            } => (base_url.clone(), api_key.clone()),
        };

        let c = load(&path, &env(&[])).unwrap();
        assert_eq!(url(&c, "openai-local").1.as_deref(), Some("from-file"));
        assert_eq!(url(&c, "openai-remote").0, "http://10.0.0.1:1/v1");

        // A variable that is really set wins over the file.
        let c = load(&path, &env(&[("OPENAI_LOCAL_API_KEY", "from-env")])).unwrap();
        assert_eq!(url(&c, "openai-local").1.as_deref(), Some("from-env"));
    }

    #[test]
    fn the_sample_loads_and_sets_every_setting_the_schema_has() {
        let c = parse(SAMPLE, &p(), &env(&[])).expect("the sample is a valid config");
        let kinds: Vec<&str> = c.agents.values().map(|a| a.kind()).collect();
        for kind in AGENT_TYPES {
            assert!(kinds.contains(&kind), "the sample has no {kind} agent");
        }
        assert!(
            c.agents.values().any(|a| a.llm_backend().is_some()),
            "the sample has no agent on an LLM backend"
        );

        let doc: serde_json::Value = serde_norway::from_str(SAMPLE).unwrap();
        let schema = schema();
        let defs = &schema["definitions"];
        let missing = |def: &str, objects: Vec<&serde_json::Value>| -> Vec<String> {
            schema_keys(&defs[def])
                .into_iter()
                .filter(|k| !objects.iter().any(|o| o.get(k).is_some()))
                .collect()
        };
        let backends: Vec<&serde_json::Value> =
            doc["llm_backends"].as_object().unwrap().values().collect();
        let agents: Vec<&serde_json::Value> = doc["agents"].as_object().unwrap().values().collect();
        let adapters = |kind: &str| -> Vec<&serde_json::Value> {
            agents
                .iter()
                .map(|a| &a["adapter"])
                .filter(|a| a["type"] == kind)
                .collect()
        };
        assert_eq!(
            missing("llm_backend", backends),
            Vec::<String>::new(),
            "llm_backends"
        );
        assert_eq!(
            missing("agent", agents.clone()),
            Vec::<String>::new(),
            "agents"
        );
        assert_eq!(
            missing("claude_code_adapter", adapters(CLAUDE_CODE_AGENT)),
            Vec::<String>::new(),
            "claude-code adapter"
        );
        assert_eq!(
            missing("opencode_adapter", adapters(OPENCODE_AGENT)),
            Vec::<String>::new(),
            "opencode adapter"
        );
        let id = schema["$id"].as_str().unwrap().to_string();
        assert!(SAMPLE.starts_with(&format!("# yaml-language-server: $schema={id}")));
    }

    const CLAUDE_DOC: &str = r#"
version: 1
agents:
  reviewer:
    prompt: Reviews code.
    adapter:
      type: claude-code
      model: sonnet
  fixer:
    max_seconds: 600
    adapter:
      type: claude-code
      model: opus
      tools: [Read, Edit, "Bash(git diff:*)"]
      permission_mode: acceptEdits
      max_turns: 9
      command: /opt/claude
"#;

    fn claude(c: &Config, agent: &str) -> ClaudeCodeAdapter {
        match &c.agents[agent].adapter {
            Adapter::ClaudeCode(cc) => cc.clone(),
            other => panic!("not a claude-code agent: {other:?}"),
        }
    }

    #[test]
    fn a_claude_code_agent_defaults_to_read_only() {
        let c = parse(CLAUDE_DOC, &p(), &env(&[])).unwrap();
        let r = claude(&c, "reviewer");
        assert_eq!(r.tools, CLAUDE_CODE_DEFAULT_TOOLS);
        assert_eq!(r.command, "claude");
        assert_eq!(r.permission_mode, "dontAsk");
        assert_eq!(r.max_turns, CLAUDE_CODE_DEFAULT_MAX_TURNS);
        assert!(!c.agents["reviewer"].can_change_files());
        assert_eq!(c.agents["reviewer"].runs_on(), CLAUDE_CODE_AGENT);

        let f = claude(&c, "fixer");
        assert_eq!(f.tools, ["Read", "Edit", "Bash(git diff:*)"]);
        assert_eq!(
            (f.permission_mode.as_str(), f.max_turns),
            ("acceptEdits", 9)
        );
        assert_eq!(f.command, "/opt/claude");
        assert_eq!(c.agents["fixer"].max_seconds, 600);
        assert!(c.agents["fixer"].can_change_files());
    }

    #[test]
    fn fields_of_the_other_adapter_type_are_refused() {
        let doc = CLAUDE_DOC.replace(
            "      model: sonnet\n",
            "      model: sonnet\n      llm_backend: x\n      max_steps: 3\n",
        );
        let joined = joined_problems(&doc);
        assert!(
            joined.contains(
                "agents.reviewer.adapter: `llm_backend` does not apply to a claude-code agent"
            ),
            "{joined}"
        );
        assert!(
            joined.contains("`max_steps` is not a field of adapter type 'claude-code'"),
            "{joined}"
        );

        let oc = REFERENCE.replace(
            "      tools: [read, edit, bash]\n",
            "      tools: [read, edit, bash]\n      max_turns: 4\n",
        );
        assert!(
            joined_problems(&oc).contains("`max_turns` is not a field of adapter type 'opencode'"),
            "{}",
            joined_problems(&oc)
        );
    }

    #[test]
    fn a_claude_code_agent_takes_no_context_window_and_one_tool_per_entry() {
        let doc = CLAUDE_DOC
            .replace(
                "    prompt: Reviews code.\n",
                "    prompt: Reviews code.\n    context_window: 8000\n",
            )
            .replace("[Read, Edit, ", "[\"Read,Edit\", ");
        let joined = joined_problems(&doc);
        assert!(
            joined.contains("Claude Code manages its own context"),
            "{joined}"
        );
        assert!(
            joined.contains("one Claude Code tool or rule per list entry"),
            "{joined}"
        );
    }

    #[test]
    fn an_opencode_agent_defaults_to_read_only_and_checks_its_fields() {
        let doc = r#"
version: 1
agents:
  looker:
    adapter: { type: opencode, model: opencode/big-pickle }
  doer:
    max_seconds: 600
    adapter:
      type: opencode
      model: ollama/qwen3.5:9b
      tools: [read, edit, "bash(git *)"]
      max_steps: 12
      command: /opt/opencode
"#;
        let c = parse(doc, &p(), &env(&[])).unwrap();
        let looker = opencode(&c, "looker");
        assert_eq!(looker.tools, OPENCODE_DEFAULT_TOOLS);
        assert_eq!(
            (looker.command.as_str(), looker.max_steps),
            ("opencode", OPENCODE_DEFAULT_MAX_STEPS)
        );
        assert_eq!(looker.llm_backend, None);
        assert!(!c.agents["looker"].can_change_files());
        let doer = opencode(&c, "doer");
        assert_eq!(doer.tools, ["read", "edit", "bash(git *)"]);
        assert_eq!(doer.max_steps, 12);
        assert!(c.agents["doer"].can_change_files());

        let bad = r#"
version: 1
agents:
  x:
    adapter: { type: opencode, model: big-pickle, tools: ["read,edit"], max_steps: 0 }
"#;
        let joined = joined_problems(bad);
        assert!(
            joined.contains("must be provider/model")
                && joined.contains("or name an `llm_backend`"),
            "{joined}"
        );
        assert!(
            joined.contains("tool 'read,edit' must be an OpenCode permission"),
            "{joined}"
        );
        assert!(
            joined.contains("`max_steps` must be greater than 0"),
            "{joined}"
        );
    }

    #[test]
    fn on_an_llm_backend_the_model_is_the_endpoints_own_id() {
        // No provider prefix: Ensemble names the provider.
        let c = parse(REFERENCE, &p(), &env(&[])).unwrap();
        assert_eq!(opencode(&c, "planner").model, "qwen3-coder");
    }

    #[test]
    fn an_unknown_adapter_type_lists_the_known_ones() {
        let doc = REFERENCE.replace("type: claude-code", "type: codex");
        let joined = joined_problems(&doc);
        assert!(
            joined.contains("agents.reviewer.adapter: type 'codex' is unknown")
                && joined.contains("claude-code, opencode"),
            "{joined}"
        );
    }

    #[test]
    fn max_seconds_must_be_positive() {
        let set = CLAUDE_DOC.replace("max_seconds: 600", "max_seconds: 60");
        assert_eq!(
            parse(&set, &p(), &env(&[])).unwrap().agents["fixer"].max_seconds,
            60
        );
        let zero = CLAUDE_DOC.replace("max_seconds: 600", "max_seconds: 0");
        assert!(joined_problems(&zero).contains("`max_seconds` must be greater than 0"));
    }

    #[test]
    fn env_overrides_the_default_base_url() {
        let c = parse(
            REFERENCE,
            &p(),
            &env(&[("OPENAI_LOCAL_BASE_URL", "http://192.168.1.9:9000/v1")]),
        )
        .unwrap();
        match &c.llm_backends["openai-local"].settings {
            BackendSettings::OpenAiCompatible { base_url, .. } => {
                assert_eq!(base_url, "http://192.168.1.9:9000/v1");
            }
        }
        // The one without an override keeps its default.
        match &c.llm_backends["openai-remote"].settings {
            BackendSettings::OpenAiCompatible { base_url, .. } => {
                assert_eq!(base_url, "http://10.10.10.5:8000/v1");
            }
        }
    }

    #[test]
    fn an_empty_env_var_falls_through_to_the_default() {
        // An exported-but-empty variable is the normal shape of "I meant to set
        // this and did not". Treating "" as an override produces a request to
        // the empty URL, which fails far from here.
        let c = parse(REFERENCE, &p(), &env(&[("OPENAI_LOCAL_BASE_URL", "   ")])).unwrap();
        match &c.llm_backends["openai-local"].settings {
            BackendSettings::OpenAiCompatible { base_url, .. } => {
                assert_eq!(base_url, "http://127.0.0.1:8000/v1")
            }
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
        match &c.llm_backends["openai-local"].settings {
            BackendSettings::OpenAiCompatible { api_key, .. } => {
                assert_eq!(api_key.as_deref(), Some("sk-secret"))
            }
        }
        // And an unset one is simply absent, not an empty bearer token.
        match &c.llm_backends["openai-remote"].settings {
            BackendSettings::OpenAiCompatible { api_key, .. } => assert_eq!(*api_key, None),
        }
    }

    #[test]
    fn an_inline_api_key_is_refused_by_name() {
        let yaml = r#"
version: 1
llm_backends:
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
    fn an_agent_with_an_empty_model_is_fatal() {
        let doc = REFERENCE.replace("model: sonnet", "model: \"  \"");
        let joined = joined_problems(&doc);
        assert!(
            joined.contains("agents.reviewer.adapter: `model` is empty"),
            "{joined}"
        );
    }

    #[test]
    fn an_unknown_backend_type_lists_the_known_ones() {
        let yaml = r#"
version: 1
llm_backends:
  x: { type: ollama }
"#;
        let joined = joined_problems(yaml);
        assert!(joined.contains("ollama"), "{joined}");
        assert!(joined.contains(OPENAI_COMPATIBLE_API), "{joined}");
    }

    #[test]
    fn an_unresolvable_base_url_names_the_variable_to_export() {
        let yaml = r#"
version: 1
llm_backends:
  remote: { type: openai-compatible-api, base_url_env: OPENAI_REMOTE_BASE_URL }
"#;
        let joined = joined_problems(yaml);
        assert!(joined.contains("OPENAI_REMOTE_BASE_URL"), "{joined}");
        assert!(joined.contains("Export"), "tells you the fix: {joined}");
    }

    #[test]
    fn a_base_url_without_a_scheme_is_refused() {
        let yaml = r#"
version: 1
llm_backends:
  x: { type: openai-compatible-api, base_url_env: U, base_url_default: "127.0.0.1:8000/v1" }
"#;
        assert!(joined_problems(yaml).contains("http://"));
    }

    #[test]
    fn trailing_slashes_are_normalised_away() {
        let yaml = r#"
version: 1
llm_backends:
  x: { type: openai-compatible-api, base_url_env: U, base_url_default: "http://127.0.0.1:8000/v1/" }
"#;
        let c = parse(yaml, &p(), &env(&[])).unwrap();
        match &c.llm_backends["x"].settings {
            BackendSettings::OpenAiCompatible { base_url, .. } => {
                assert_eq!(base_url, "http://127.0.0.1:8000/v1")
            }
        }
    }

    #[test]
    fn an_unsupported_version_is_refused() {
        let yaml = r#"
version: 2
llm_backends:
  x: { type: openai-compatible-api, base_url_env: U, base_url_default: "http://127.0.0.1:1/v1" }
"#;
        assert!(joined_problems(yaml).contains("version"));
    }

    #[test]
    fn a_typo_in_an_adapter_key_is_an_error_not_a_default() {
        let doc = REFERENCE.replace("      model: sonnet", "      modle: sonnet");
        let err = parse(&doc, &p(), &env(&[])).unwrap_err();
        assert!(matches!(err, ConfigError::Syntax { .. }), "{err}");
        assert!(err.to_string().contains("modle"), "{err}");
    }

    #[test]
    fn every_independent_problem_is_reported_in_one_pass() {
        let yaml = r#"
version: 9
llm_backends:
  x: { type: nonsense }
agents:
  r:
    adapter:
      type: opencode
      llm_backend: missing
      model: ""
"#;
        let problems = problems_of(parse(yaml, &p(), &env(&[])).unwrap_err());
        assert!(problems.len() >= 4, "expected several, got: {problems:#?}");
    }

    #[test]
    fn zero_timeout_is_refused() {
        let yaml = r#"
version: 1
llm_backends:
  x: { type: openai-compatible-api, base_url_env: U, base_url_default: "http://127.0.0.1:1/v1", timeout_seconds: 0 }
"#;
        assert!(joined_problems(yaml).contains("timeout_seconds"));
    }

    #[test]
    fn agents_are_found_by_name() {
        let c = parse(REFERENCE, &p(), &env(&[])).unwrap();
        assert_eq!(c.agent("coder").unwrap().name, "coder");
        assert!(c.agent("nope").is_none());
    }
}
