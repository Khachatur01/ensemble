//! `.ensemble/config.yaml`: load, resolve the environment, validate.
//!
//! Validation is deliberately loud and early. Every check here corresponds to a
//! failure that would otherwise surface much later and much less clearly — as a
//! 404 on the first request to an agent, as an agent that quietly runs with
//! settings nobody chose, or as an empty base URL that turns into a connection
//! refused.
//!
//! There are no default values. Every setting is written in the file, and a
//! missing one is refused by name. What differs between cases is a tagged enum
//! (`type:`), and each variant carries exactly its own required settings, so a
//! setting that does not apply cannot be written and one that does cannot be
//! left out. A few settings are optional because their absence means "not
//! used" (no system prompt, no API key), which is not a value anyone chose.
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
/// Variables for the config's `api_key_env` settings, beside the config file.
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

/// Where an OpenCode agent's model comes from.
pub const PROVIDER_MODEL: &str = "provider";
pub const LLM_BACKEND_MODEL: &str = "llm_backend";

/// The type of Ensemble's former built-in harness, which no longer exists.
/// Named only to tell someone whose config still uses it where to go.
const REMOVED_ENSEMBLE_AGENT: &str = "ensemble";

// ---------------------------------------------------------------------------
// The document, as written
// ---------------------------------------------------------------------------

/// `deny_unknown_fields` throughout: a typo'd key is a key that does nothing,
/// and an agent silently running with settings nobody chose is the failure mode
/// this whole module exists to prevent. `reviewr:` should be an error, not a
/// no-op. A key that is missing is as much an error as one that is unknown.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    version: u32,
    /// Model endpoints. An agent names one with `llm_backend`, and Ensemble
    /// hands its harness the endpoint, so it is defined once, here.
    llm_backends: BTreeMap<String, RawLlmBackend>,
    /// The team.
    agents: BTreeMap<String, RawAgent>,
    /// Commands a task's work must pass. Defined here, by a person, so a task
    /// names a gate and cannot bring a command of its own.
    gates: BTreeMap<String, RawGate>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGate {
    /// A shell command run in the project; exit status 0 passes.
    command: String,
    /// Seconds it may run before it is stopped and counted as failed.
    timeout_seconds: u32,
}

/// A model endpoint. `type` decides which settings it has.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum RawLlmBackend {
    #[serde(rename = "openai-compatible-api")]
    OpenAiCompatibleApi(RawOpenAiBackend),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOpenAiBackend {
    /// Where the endpoint is, as a URL: `http://127.0.0.1:11434/v1`.
    base_url: String,
    /// The *name* of the variable holding the API key. Absent: the endpoint
    /// wants none.
    api_key_env: Option<String>,
    /// Seconds one request may take.
    timeout_seconds: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgent {
    /// What the agent is for. Its first line is what the calling agent sees
    /// when it lists the team, and the whole of it is the agent's system
    /// prompt. Absent: the agent has no system prompt, a model with a name.
    prompt: Option<String>,
    /// Wall-clock cap on one delegation, in seconds. A run that reaches it
    /// stops and reports what it did, so a hung model cannot hold the caller
    /// forever.
    max_seconds: u32,
    /// Most delegations to this agent that run at once in a session; more
    /// wait for a free slot. 1 for a model server that can serve one request
    /// at a time.
    max_concurrent: u32,
    /// What runs the agent, and how.
    adapter: RawAdapter,
}

/// The harness behind an agent. `type` decides which settings it has.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum RawAdapter {
    #[serde(rename = "claude-code")]
    ClaudeCode(RawClaudeCodeAdapter),
    #[serde(rename = "opencode")]
    OpenCode(RawOpenCodeAdapter),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClaudeCodeAdapter {
    /// As Claude Code names it: an alias or a full model id.
    model: String,
    /// What the agent may do, in Claude Code's own tool names (`Read`,
    /// `Bash(git:*)`).
    tools: Vec<String>,
    /// Claude Code's `--permission-mode`. `dontAsk` denies what `tools` does
    /// not allow instead of waiting for a person who is not there.
    permission_mode: String,
    /// Most agent turns Claude Code may take for one delegation.
    max_turns: u32,
    /// The CLI: `claude` if it is on PATH.
    command: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOpenCodeAdapter {
    /// Where the model comes from.
    model: RawOpenCodeModel,
    /// What the agent may do, in OpenCode's permissions (`read`,
    /// `bash(git *)`).
    tools: Vec<String>,
    /// Cap on steps within one delegation.
    max_steps: u32,
    /// The CLI: `opencode` if it is on PATH.
    command: String,
}

/// The model of an OpenCode agent: one OpenCode is already set up for, or one
/// served by an LLM backend of this config.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum RawOpenCodeModel {
    #[serde(rename = "provider")]
    Provider(RawProviderModel),
    #[serde(rename = "llm_backend")]
    LlmBackend(RawBackendModel),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProviderModel {
    /// `provider/model`, as `opencode models` lists it.
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBackendModel {
    /// Which of `llm_backends` serves the model. Ensemble gives OpenCode the
    /// endpoint and key for the run.
    backend: String,
    /// The id the endpoint serves the model under, as its /models lists it.
    name: String,
    /// Tokens of context the server really runs the model with. OpenCode
    /// needs it to know when to compact the conversation.
    context_window: u32,
}

// ---------------------------------------------------------------------------
// The document, resolved
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendSettings {
    OpenAiCompatible {
        base_url: String,
        api_key: Option<String>,
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

/// One member of the team, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    pub name: String,
    pub prompt: Option<String>,
    /// See [`RawAgent::max_seconds`].
    pub max_seconds: u32,
    /// See [`RawAgent::max_concurrent`].
    pub max_concurrent: u32,
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
    pub model: OpenCodeModel,
    /// OpenCode permission names, each allowed; everything else is denied.
    /// `name(pattern)` allows only matching uses, such as `bash(git *)`.
    pub tools: Vec<String>,
    pub max_steps: u32,
}

/// Where an OpenCode agent's model comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenCodeModel {
    /// `provider/model`, as OpenCode names it, on a provider it is already
    /// set up for.
    Provider { model: String },
    /// A model served by one of [`Config::llm_backends`].
    LlmBackend {
        backend: String,
        /// The id the endpoint serves the model under.
        model: String,
        context_window: u32,
    },
}

impl OpenCodeAdapter {
    /// The model: `provider/model`, or the id an LLM backend serves it under.
    pub fn model_id(&self) -> &str {
        match &self.model {
            OpenCodeModel::Provider { model } | OpenCodeModel::LlmBackend { model, .. } => model,
        }
    }

    /// The LLM backend its model runs on, when it runs on one.
    pub fn llm_backend(&self) -> Option<&str> {
        match &self.model {
            OpenCodeModel::Provider { .. } => None,
            OpenCodeModel::LlmBackend { backend, .. } => Some(backend),
        }
    }

    /// Tokens of context the model's server runs it with, when it runs on an
    /// LLM backend.
    pub fn context_window(&self) -> Option<u32> {
        match &self.model {
            OpenCodeModel::Provider { .. } => None,
            OpenCodeModel::LlmBackend { context_window, .. } => Some(*context_window),
        }
    }
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
            Adapter::OpenCode(o) => o.model_id(),
        }
    }

    /// The LLM backend its model runs on, when it runs on one.
    pub fn llm_backend(&self) -> Option<&str> {
        match &self.adapter {
            Adapter::ClaudeCode(_) => None,
            Adapter::OpenCode(o) => o.llm_backend(),
        }
    }

    /// Tokens of context its model's server runs it with, when it runs on an
    /// LLM backend.
    pub fn context_window(&self) -> Option<u32> {
        match &self.adapter {
            Adapter::ClaudeCode(_) => None,
            Adapter::OpenCode(o) => o.context_window(),
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
    pub gates: BTreeMap<String, Gate>,
    /// Where it was loaded from, for error messages.
    pub source: PathBuf,
}

/// A command a task's work must pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    pub command: String,
    pub timeout_seconds: u32,
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
        detail: explain(&e),
    })?;
    validate(raw, path, env)
}

/// What serde said, which already says where in the document, with a missing
/// key put as the instruction it is and the removed `ensemble` type pointed to
/// its replacement.
fn explain(e: &serde_norway::Error) -> String {
    let mut detail = e.to_string();
    if let Some((before, rest)) = detail.split_once("missing field `")
        && let Some((key, after)) = rest.split_once('`')
    {
        detail = format!("{before}`{key}` is required{after}");
    }
    if detail.contains(&format!("unknown variant `{REMOVED_ENSEMBLE_AGENT}`")) {
        detail.push_str(&format!(
            ". Type '{REMOVED_ENSEMBLE_AGENT}' was removed; Ensemble no longer runs agents itself. To run a model on an LLM backend, use type '{OPENCODE_AGENT}' with a `model` of type '{LLM_BACKEND_MODEL}'."
        ));
    }
    detail
}

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

    // --- gates ---
    let mut gates = BTreeMap::new();
    for (name, g) in &raw.gates {
        if g.command.trim().is_empty() {
            problems.push(format!("gates.{name}: `command` is empty"));
        }
        if g.timeout_seconds == 0 {
            problems.push(format!(
                "gates.{name}: `timeout_seconds` must be greater than 0"
            ));
        }
        gates.insert(
            name.clone(),
            Gate {
                command: g.command.clone(),
                timeout_seconds: g.timeout_seconds,
            },
        );
    }

    // --- agents ---
    let mut agents: BTreeMap<String, AgentConfig> = BTreeMap::new();
    for (name, ra) in &raw.agents {
        if name.trim().is_empty() {
            problems.push("agents: an agent name cannot be empty".into());
            continue;
        }
        if ra.max_seconds == 0 {
            problems.push(format!(
                "agents.{name}: `max_seconds` must be greater than 0"
            ));
        }
        if ra.max_concurrent == 0 {
            problems.push(format!(
                "agents.{name}: `max_concurrent` must be greater than 0"
            ));
        }
        let at = format!("agents.{name}.adapter");
        let adapter = match &ra.adapter {
            RawAdapter::ClaudeCode(c) => {
                Adapter::ClaudeCode(claude_code_adapter(&at, c, &mut problems))
            }
            RawAdapter::OpenCode(o) => {
                Adapter::OpenCode(opencode_adapter(&at, o, &raw.llm_backends, &mut problems))
            }
        };
        agents.insert(
            name.clone(),
            AgentConfig {
                name: name.clone(),
                prompt: ra.prompt.clone(),
                max_seconds: ra.max_seconds,
                max_concurrent: ra.max_concurrent,
                adapter,
            },
        );
    }

    // A backend nothing uses is not an error — it is a spare, and deleting it
    // from a shared config is how you break someone else's setup. Say so once
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
            gates,
            source: path.to_path_buf(),
        })
    } else {
        Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            problems,
        })
    }
}

fn opencode_adapter(
    at: &str,
    ad: &RawOpenCodeAdapter,
    declared: &BTreeMap<String, RawLlmBackend>,
    problems: &mut Vec<String>,
) -> OpenCodeAdapter {
    for tool in &ad.tools {
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
    if ad.max_steps == 0 {
        problems.push(format!("{at}: `max_steps` must be greater than 0"));
    }
    let model = match &ad.model {
        RawOpenCodeModel::Provider(p) => {
            if p.name.trim().is_empty() {
                problems.push(format!("{at}: `model.name` is empty"));
            } else if !p.name.contains('/') {
                problems.push(format!(
                    "{at}: model '{}' must be provider/model, as `opencode models` lists it, or run on an LLM backend with a `model` of type '{LLM_BACKEND_MODEL}'",
                    p.name
                ));
            }
            OpenCodeModel::Provider {
                model: p.name.clone(),
            }
        }
        RawOpenCodeModel::LlmBackend(b) => {
            if !declared.contains_key(&b.backend) {
                let known: Vec<&str> = declared.keys().map(String::as_str).collect();
                problems.push(format!(
                    "{at}: LLM backend '{}' is not defined. Defined LLM backends: {}.",
                    b.backend,
                    if known.is_empty() {
                        "(none)".to_string()
                    } else {
                        known.join(", ")
                    }
                ));
            }
            // On a backend the model is the id the endpoint serves; Ensemble
            // names the provider.
            if b.name.trim().is_empty() {
                problems.push(format!("{at}: `model.name` is empty"));
            }
            if b.context_window == 0 {
                problems.push(format!(
                    "{at}: `model.context_window` must be greater than 0"
                ));
            }
            OpenCodeModel::LlmBackend {
                backend: b.backend.clone(),
                model: b.name.clone(),
                context_window: b.context_window,
            }
        }
    };
    if ad.command.trim().is_empty() {
        problems.push(format!("{at}: `command` is empty"));
    }
    OpenCodeAdapter {
        command: ad.command.clone(),
        model,
        tools: ad.tools.clone(),
        max_steps: ad.max_steps,
    }
}

fn claude_code_adapter(
    at: &str,
    ad: &RawClaudeCodeAdapter,
    problems: &mut Vec<String>,
) -> ClaudeCodeAdapter {
    if ad.model.trim().is_empty() {
        problems.push(format!("{at}: `model` is empty"));
    }
    if let Some(bad) = ad
        .tools
        .iter()
        .find(|t| t.trim().is_empty() || t.contains(','))
    {
        problems.push(format!(
            "{at}: tool '{bad}' must be one Claude Code tool or rule per list entry, such as Read or Bash(git:*)"
        ));
    }
    if ad.permission_mode.trim().is_empty() {
        problems.push(format!("{at}: `permission_mode` is empty"));
    }
    if ad.max_turns == 0 {
        problems.push(format!("{at}: `max_turns` must be greater than 0"));
    }
    if ad.command.trim().is_empty() {
        problems.push(format!("{at}: `command` is empty"));
    }
    ClaudeCodeAdapter {
        command: ad.command.clone(),
        model: ad.model.clone(),
        tools: ad.tools.clone(),
        permission_mode: ad.permission_mode.clone(),
        max_turns: ad.max_turns,
    }
}

fn resolve_backend(
    name: &str,
    rb: &RawLlmBackend,
    env: &dyn Env,
) -> Result<LlmBackend, Vec<String>> {
    match rb {
        RawLlmBackend::OpenAiCompatibleApi(b) => resolve_openai(name, b, env),
    }
}

/// Check the URL and timeout, and read the optional key from the environment.
fn resolve_openai(
    name: &str,
    b: &RawOpenAiBackend,
    env: &dyn Env,
) -> Result<LlmBackend, Vec<String>> {
    let mut problems = Vec::new();
    if b.timeout_seconds == 0 {
        problems.push(format!(
            "llm_backends.{name}: `timeout_seconds` must be greater than 0"
        ));
    }
    let base_url = b.base_url.trim();
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        problems.push(format!(
            "llm_backends.{name}: `base_url` '{base_url}' must start with http:// or https://"
        ));
    }

    // A key is optional — a local llama-server usually wants none — but a
    // *named* variable that is empty is a different thing from no variable at
    // all: somebody expected authentication. Say so rather than sending an
    // empty bearer token and reading a 401 later.
    let api_key = match &b.api_key_env {
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

    if problems.is_empty() {
        Ok(LlmBackend {
            name: name.to_string(),
            timeout_seconds: b.timeout_seconds,
            settings: BackendSettings::OpenAiCompatible {
                base_url: base_url.trim_end_matches('/').to_string(),
                api_key,
            },
        })
    } else {
        Err(problems)
    }
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

    fn claude(c: &Config, agent: &str) -> ClaudeCodeAdapter {
        match &c.agents[agent].adapter {
            Adapter::ClaudeCode(cc) => cc.clone(),
            other => panic!("not a claude-code agent: {other:?}"),
        }
    }

    /// OpenCode agents on two LLM backends, and one Claude Code agent. Every
    /// setting is written, as it has to be.
    const REFERENCE: &str = r#"
version: 1
llm_backends:
  openai-local:
    type: openai-compatible-api
    base_url: "http://127.0.0.1:8000/v1"
    api_key_env: OPENAI_LOCAL_API_KEY
    timeout_seconds: 600
  openai-remote:
    type: openai-compatible-api
    base_url: "http://10.10.10.5:8000/v1"
    api_key_env: OPENAI_REMOTE_API_KEY
    timeout_seconds: 300
gates: {}
agents:
  planner:
    max_seconds: 1800
    max_concurrent: 1
    adapter:
      type: opencode
      model:
        type: llm_backend
        backend: openai-local
        name: qwen3-coder
        context_window: 32768
      tools: [read, glob, grep, list]
      max_steps: 30
      command: opencode
  coder:
    max_seconds: 900
    max_concurrent: 1
    adapter:
      type: opencode
      model:
        type: llm_backend
        backend: openai-remote
        name: qwen3-coder
        context_window: 32768
      tools: [read, edit, bash]
      max_steps: 30
      command: opencode
  reviewer:
    max_seconds: 1800
    max_concurrent: 1
    adapter:
      type: claude-code
      model: sonnet
      tools: [Read, Grep, Glob]
      permission_mode: dontAsk
      max_turns: 30
      command: claude
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

    /// The message of a document that serde refuses.
    fn refusal(doc: &str) -> String {
        match parse(doc, &p(), &env(&[])).unwrap_err() {
            ConfigError::Syntax { detail, .. } => detail,
            other => panic!("expected Syntax, got: {other}"),
        }
    }

    #[test]
    fn the_reference_config_loads() {
        let c = parse(REFERENCE, &p(), &env(&[])).expect("reference config must load");
        assert_eq!(c.version, 1);
        assert_eq!(c.llm_backends.len(), 2);
        assert_eq!(c.llm_backends["openai-remote"].timeout_seconds, 300);
        assert_eq!(c.agent_names(), vec!["coder", "planner", "reviewer"]);
        let coder = &c.agents["coder"];
        assert_eq!(coder.llm_backend(), Some("openai-remote"));
        assert_eq!(coder.runs_on(), "opencode on openai-remote");
        assert_eq!(coder.context_window(), Some(32768));
        assert_eq!(coder.max_seconds, 900);
        assert!(coder.can_change_files());
        assert_eq!(c.agents["reviewer"].llm_backend(), None);
        assert_eq!(c.agents["reviewer"].context_window(), None);
    }

    #[test]
    fn the_removed_ensemble_type_points_to_opencode() {
        let doc = REFERENCE.replace("type: claude-code", "type: ensemble");
        let detail = refusal(&doc);
        assert!(
            detail.contains("agents.reviewer.adapter")
                && detail.contains("'ensemble' was removed")
                && detail.contains("type 'opencode'"),
            "{detail}"
        );
    }

    #[test]
    fn settings_of_the_removed_harness_are_unknown_fields() {
        let doc = REFERENCE.replace(
            "      max_steps: 30\n      command: opencode\n  coder:",
            "      max_steps: 30\n      temperature: 0.2\n      command: opencode\n  coder:",
        );
        let detail = refusal(&doc);
        assert!(detail.contains("temperature"), "{detail}");
    }

    #[test]
    fn a_missing_setting_is_refused_by_name_and_place() {
        // (the line to take out, where it belongs, its key)
        for (line, at, key) in [
            ("    max_seconds: 900\n", "agents.coder", "max_seconds"),
            (
                "      max_steps: 30\n      command: opencode\n  reviewer:",
                "agents.coder",
                "max_steps",
            ),
            ("      max_turns: 30\n", "agents.reviewer", "max_turns"),
            ("      command: claude\n", "agents.reviewer", "command"),
            (
                "    timeout_seconds: 300\n",
                "llm_backends",
                "timeout_seconds",
            ),
            (
                "    base_url: \"http://10.10.10.5:8000/v1\"\n",
                "llm_backends",
                "base_url",
            ),
            (
                "        context_window: 32768\n      tools: [read, edit, bash]",
                "agents.coder",
                "context_window",
            ),
        ] {
            // Put back what follows the line, so only the one key is gone.
            let kept = match key {
                "max_steps" => "      command: opencode\n  reviewer:",
                "context_window" => "      tools: [read, edit, bash]",
                _ => "",
            };
            let doc = REFERENCE.replacen(line, kept, 1);
            assert_ne!(doc, REFERENCE, "the fixture lost its {key} line");
            let detail = refusal(&doc);
            assert!(
                detail.contains(at) && detail.contains(&format!("`{key}` is required")),
                "{key}: {detail}"
            );
        }
    }

    #[test]
    fn a_config_without_its_agents_or_backends_is_refused() {
        let detail = refusal("version: 1\ngates: {}\nagents: {}\n");
        assert!(detail.contains("`llm_backends` is required"), "{detail}");
        let detail = refusal("version: 1\nllm_backends: {}\n");
        assert!(detail.contains("`agents` is required"), "{detail}");
        let detail = refusal("version: 1\nllm_backends: {}\nagents: {}\n");
        assert!(detail.contains("`gates` is required"), "{detail}");
        assert!(
            parse(
                "version: 1\nllm_backends: {}\ngates: {}\nagents: {}\n",
                &p(),
                &env(&[])
            )
            .is_ok()
        );
    }

    #[test]
    fn a_setting_of_another_adapter_type_is_unknown_and_so_is_a_stray_context_window() {
        let doc = REFERENCE.replace(
            "      model: sonnet\n",
            "      model: sonnet\n      max_steps: 3\n",
        );
        let detail = refusal(&doc);
        assert!(detail.contains("max_steps"), "{detail}");

        // The window belongs to the model on an LLM backend, nowhere else.
        let doc = REFERENCE.replace(
            "    max_concurrent: 1\n    adapter:\n      type: claude-code",
            "    max_concurrent: 1\n    context_window: 8000\n    adapter:\n      type: claude-code",
        );
        assert!(refusal(&doc).contains("context_window"));

        let provider = "model:\n        type: provider\n        name: opencode/big-pickle\n        context_window: 1\n";
        let doc = format!(
            "version: 1\nllm_backends: {{}}\ngates: {{}}\nagents:\n  x:\n    max_seconds: 60\n    max_concurrent: 1\n    adapter:\n      type: opencode\n      {provider}      tools: []\n      max_steps: 3\n      command: opencode\n"
        );
        assert!(refusal(&doc).contains("context_window"));
    }

    #[test]
    fn an_opencode_model_is_a_provider_model_or_an_llm_backend_model() {
        let doc = |model: &str| {
            format!(
                "version: 1\nllm_backends: {{}}\ngates: {{}}\nagents:\n  x:\n    max_seconds: 60\n    max_concurrent: 1\n    adapter:\n      type: opencode\n      model: {model}\n      tools: [read]\n      max_steps: 3\n      command: opencode\n"
            )
        };
        let c = parse(
            &doc("{ type: provider, name: opencode/big-pickle }"),
            &p(),
            &env(&[]),
        )
        .unwrap();
        let oc = opencode(&c, "x");
        assert_eq!(oc.model_id(), "opencode/big-pickle");
        assert_eq!((oc.llm_backend(), oc.context_window()), (None, None));

        let detail = refusal(&doc("{ type: local }"));
        assert!(
            detail.contains("unknown variant `local`")
                && detail.contains("provider")
                && detail.contains("llm_backend"),
            "{detail}"
        );
        let detail = refusal(&doc("opencode/big-pickle"));
        assert!(detail.contains("agents.x"), "{detail}");
    }

    #[test]
    fn the_window_of_an_llm_backend_model_must_be_positive() {
        let zero = REFERENCE.replacen("context_window: 32768", "context_window: 0", 1);
        assert!(joined_problems(&zero).contains("greater than 0"));
    }

    #[test]
    fn an_agent_naming_an_undefined_backend_is_fatal_and_lists_the_real_ones() {
        let doc = REFERENCE.replace("backend: openai-remote", "backend: opanai-remote");
        let joined = joined_problems(&doc);
        assert!(
            joined.contains("LLM backend 'opanai-remote' is not defined")
                && joined.contains("openai-local, openai-remote"),
            "{joined}"
        );
    }

    #[test]
    fn a_backend_no_agent_uses_is_not_an_error() {
        let doc = REFERENCE.replace("backend: openai-remote", "backend: openai-local");
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

    /// The keys a schema object lists, less `type`, which the enum consumes.
    fn schema_keys(node: &serde_json::Value) -> Vec<String> {
        let mut keys: Vec<String> = node["properties"]
            .as_object()
            .unwrap()
            .keys()
            .filter(|k| *k != "type")
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    /// Every key of the schema object is required, but `optional` ones.
    fn schema_required(node: &serde_json::Value) -> Vec<String> {
        let mut keys: Vec<String> = node["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .filter(|k| k != "type")
            .collect();
        keys.sort();
        keys
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
        for (def, keys, label) in [
            (
                "openai_compatible_api_backend",
                accepted_keys::<RawOpenAiBackend>(),
                "openai-compatible-api backend",
            ),
            ("agent", accepted_keys::<RawAgent>(), "agents"),
            ("gate", accepted_keys::<RawGate>(), "gates"),
            (
                "claude_code_adapter",
                accepted_keys::<RawClaudeCodeAdapter>(),
                "claude-code adapter",
            ),
            (
                "opencode_adapter",
                accepted_keys::<RawOpenCodeAdapter>(),
                "opencode adapter",
            ),
            (
                "opencode_provider_model",
                accepted_keys::<RawProviderModel>(),
                "opencode provider model",
            ),
            (
                "opencode_llm_backend_model",
                accepted_keys::<RawBackendModel>(),
                "opencode llm_backend model",
            ),
        ] {
            assert_eq!(schema_keys(&defs[def]), keys, "{label}");
        }
    }

    #[test]
    fn the_schema_requires_every_key_but_the_optional_two() {
        let schema = schema();
        let defs = &schema["definitions"];
        assert_eq!(
            schema_required(&schema),
            accepted_keys::<RawConfig>(),
            "top level"
        );
        let optional = |def: &str, keys: &[&str]| {
            let mut all = schema_keys(&defs[def]);
            all.retain(|k| !keys.contains(&k.as_str()));
            all
        };
        for (def, keys) in [
            ("openai_compatible_api_backend", vec!["api_key_env"]),
            ("agent", vec!["prompt"]),
            ("gate", vec![]),
            ("claude_code_adapter", vec![]),
            ("opencode_adapter", vec![]),
            ("opencode_provider_model", vec![]),
            ("opencode_llm_backend_model", vec![]),
        ] {
            assert_eq!(schema_required(&defs[def]), optional(def, &keys), "{def}");
        }
        // And nothing in it says what a missing key would have been.
        assert!(!SCHEMA.contains("\"default\":"), "the schema has a default");
    }

    #[test]
    fn the_schemas_choices_are_the_codes() {
        let schema = schema();
        let defs = &schema["definitions"];
        let tag = |def: &str| defs[def]["properties"]["type"]["const"].clone();
        assert_eq!(tag("openai_compatible_api_backend"), OPENAI_COMPATIBLE_API);
        assert_eq!(tag("claude_code_adapter"), CLAUDE_CODE_AGENT);
        assert_eq!(tag("opencode_adapter"), OPENCODE_AGENT);
        assert_eq!(tag("opencode_provider_model"), PROVIDER_MODEL);
        assert_eq!(tag("opencode_llm_backend_model"), LLM_BACKEND_MODEL);
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
        std::fs::write(dir.join(CONFIG_FILENAME), REFERENCE).unwrap();
        std::fs::write(
            dir.join(ENV_FILENAME),
            "OPENAI_LOCAL_API_KEY=from-file\nOPENAI_REMOTE_API_KEY=remote-file\n",
        )
        .unwrap();
        let path = discover(&tmp.0).expect("found in .ensemble");
        let key = |c: &Config, name: &str| match &c.llm_backends[name].settings {
            BackendSettings::OpenAiCompatible { api_key, .. } => api_key.clone(),
        };

        let c = load(&path, &env(&[])).unwrap();
        assert_eq!(key(&c, "openai-local").as_deref(), Some("from-file"));
        assert_eq!(key(&c, "openai-remote").as_deref(), Some("remote-file"));

        // A variable that is really set wins over the file.
        let c = load(&path, &env(&[("OPENAI_LOCAL_API_KEY", "from-env")])).unwrap();
        assert_eq!(key(&c, "openai-local").as_deref(), Some("from-env"));
    }

    #[test]
    fn the_sample_loads_and_sets_every_setting_the_schema_has() {
        let c = parse(SAMPLE, &p(), &env(&[])).expect("the sample is a valid config");
        let kinds: Vec<&str> = c.agents.values().map(|a| a.kind()).collect();
        for kind in [CLAUDE_CODE_AGENT, OPENCODE_AGENT] {
            assert!(kinds.contains(&kind), "the sample has no {kind} agent");
        }
        assert!(
            c.agents.values().any(|a| a.llm_backend().is_some()),
            "the sample has no agent on an LLM backend"
        );
        assert!(
            c.agents
                .values()
                .any(|a| a.kind() == OPENCODE_AGENT && a.llm_backend().is_none()),
            "the sample has no agent on a provider"
        );

        let doc: serde_json::Value = serde_norway::from_str(SAMPLE).unwrap();
        let schema = schema();
        let defs = &schema["definitions"];
        let missing = |def: &str, objects: Vec<&serde_json::Value>| -> Vec<String> {
            assert!(!objects.is_empty(), "the sample has no {def}");
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
        let models = |kind: &str| -> Vec<&serde_json::Value> {
            adapters(OPENCODE_AGENT)
                .into_iter()
                .map(|a| &a["model"])
                .filter(|m| m["type"] == kind)
                .collect()
        };
        for (def, objects) in [
            ("openai_compatible_api_backend", backends),
            ("agent", agents.clone()),
            ("gate", doc["gates"].as_object().unwrap().values().collect()),
            ("claude_code_adapter", adapters(CLAUDE_CODE_AGENT)),
            ("opencode_adapter", adapters(OPENCODE_AGENT)),
            ("opencode_provider_model", models(PROVIDER_MODEL)),
            ("opencode_llm_backend_model", models(LLM_BACKEND_MODEL)),
        ] {
            assert_eq!(missing(def, objects), Vec::<String>::new(), "{def}");
        }
        let id = schema["$id"].as_str().unwrap().to_string();
        assert!(SAMPLE.starts_with(&format!("# yaml-language-server: $schema={id}")));
    }

    #[test]
    fn a_claude_code_agent_is_what_its_adapter_says() {
        let doc = REFERENCE.replace(
            "      tools: [Read, Grep, Glob]\n      permission_mode: dontAsk\n      max_turns: 30\n      command: claude",
            "      tools: [Read, Edit, \"Bash(git diff:*)\"]\n      permission_mode: acceptEdits\n      max_turns: 9\n      command: /opt/claude",
        );
        let c = parse(&doc, &p(), &env(&[])).unwrap();
        let f = claude(&c, "reviewer");
        assert_eq!(f.tools, ["Read", "Edit", "Bash(git diff:*)"]);
        assert_eq!(
            (f.permission_mode.as_str(), f.max_turns),
            ("acceptEdits", 9)
        );
        assert_eq!(f.command, "/opt/claude");
        assert!(c.agents["reviewer"].can_change_files());
        assert_eq!(c.agents["reviewer"].runs_on(), CLAUDE_CODE_AGENT);

        // Read-only is what the config says, not something that is assumed.
        let r = REFERENCE.replace("tools: [Read, Grep, Glob]", "tools: []");
        let c = parse(&r, &p(), &env(&[])).unwrap();
        assert!(!c.agents["reviewer"].has_tools());
    }

    #[test]
    fn a_claude_code_agent_takes_one_tool_per_entry_and_a_positive_turn_count() {
        let doc = REFERENCE
            .replace("[Read, Grep, Glob]", "[\"Read,Edit\", Grep]")
            .replace("max_turns: 30", "max_turns: 0")
            .replace("command: claude", "command: \" \"")
            .replace("permission_mode: dontAsk", "permission_mode: \"\"");
        let joined = joined_problems(&doc);
        for expected in [
            "one Claude Code tool or rule per list entry",
            "`max_turns` must be greater than 0",
            "`command` is empty",
            "`permission_mode` is empty",
        ] {
            assert!(joined.contains(expected), "{expected}: {joined}");
        }
    }

    #[test]
    fn an_opencode_agent_checks_its_fields() {
        let doc = r#"
version: 1
llm_backends: {}
gates: {}
agents:
  doer:
    max_seconds: 600
    max_concurrent: 1
    adapter:
      type: opencode
      model: { type: provider, name: "ollama/qwen3.5:9b" }
      tools: [read, edit, "bash(git *)"]
      max_steps: 12
      command: /opt/opencode
"#;
        let c = parse(doc, &p(), &env(&[])).unwrap();
        let doer = opencode(&c, "doer");
        assert_eq!(doer.tools, ["read", "edit", "bash(git *)"]);
        assert_eq!(doer.max_steps, 12);
        assert_eq!(doer.command, "/opt/opencode");
        assert!(c.agents["doer"].can_change_files());

        let bad = r#"
version: 1
llm_backends: {}
gates: {}
agents:
  x:
    max_seconds: 60
    max_concurrent: 1
    adapter:
      type: opencode
      model: { type: provider, name: big-pickle }
      tools: ["read,edit"]
      max_steps: 0
      command: opencode
"#;
        let joined = joined_problems(bad);
        assert!(
            joined.contains("must be provider/model")
                && joined.contains("or run on an LLM backend"),
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
        assert_eq!(opencode(&c, "planner").model_id(), "qwen3-coder");
    }

    #[test]
    fn an_unknown_adapter_type_lists_the_known_ones() {
        let doc = REFERENCE.replace("type: claude-code", "type: codex");
        let detail = refusal(&doc);
        assert!(
            detail.contains("agents.reviewer.adapter")
                && detail.contains("unknown variant `codex`")
                && detail.contains("claude-code")
                && detail.contains("opencode"),
            "{detail}"
        );
    }

    #[test]
    fn max_concurrent_must_be_positive() {
        let zero = REFERENCE.replacen("max_concurrent: 1", "max_concurrent: 0", 1);
        assert!(joined_problems(&zero).contains("`max_concurrent` must be greater than 0"));
    }

    #[test]
    fn max_seconds_must_be_positive() {
        let zero = REFERENCE.replace("max_seconds: 900", "max_seconds: 0");
        assert!(joined_problems(&zero).contains("`max_seconds` must be greater than 0"));
        let c = parse(REFERENCE, &p(), &env(&[])).unwrap();
        assert_eq!(c.agents["coder"].max_seconds, 900);
    }

    #[test]
    fn the_base_url_is_the_one_written_in_the_config() {
        // No variable overrides it: the file is the whole truth.
        let c = parse(
            REFERENCE,
            &p(),
            &env(&[("OPENAI_LOCAL_BASE_URL", "http://192.168.1.9:9000/v1")]),
        )
        .unwrap();
        match &c.llm_backends["openai-local"].settings {
            BackendSettings::OpenAiCompatible { base_url, .. } => {
                assert_eq!(base_url, "http://127.0.0.1:8000/v1");
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
    fn a_backend_may_name_no_key_at_all() {
        let yaml = r#"
version: 1
llm_backends:
  x: { type: openai-compatible-api, base_url: "http://127.0.0.1:1/v1", timeout_seconds: 5 }
gates: {}
agents: {}
"#;
        let c = parse(yaml, &p(), &env(&[])).unwrap();
        match &c.llm_backends["x"].settings {
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
    base_url: "http://127.0.0.1:1/v1"
    timeout_seconds: 5
    api_key: "sk-oops"
gates: {}
agents: {}
"#;
        let detail = refusal(yaml);
        assert!(
            detail.contains("api_key"),
            "must name the offending key: {detail}"
        );
    }

    #[test]
    fn the_removed_url_settings_are_unknown() {
        for key in [
            "base_url_env: U",
            "base_url_default: \"http://127.0.0.1:1/v1\"",
        ] {
            let yaml = format!(
                "version: 1\nllm_backends:\n  x:\n    type: openai-compatible-api\n    base_url: \"http://127.0.0.1:1/v1\"\n    timeout_seconds: 5\n    {key}\ngates: {{}}\nagents: {{}}\n"
            );
            let detail = refusal(&yaml);
            assert!(detail.contains("unknown field"), "{detail}");
        }
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
gates: {}
agents: {}
"#;
        let detail = refusal(yaml);
        assert!(detail.contains("llm_backends.x"), "{detail}");
        assert!(detail.contains("ollama"), "{detail}");
        assert!(detail.contains(OPENAI_COMPATIBLE_API), "{detail}");
    }

    #[test]
    fn a_base_url_without_a_scheme_is_refused() {
        let yaml = r#"
version: 1
llm_backends:
  x: { type: openai-compatible-api, base_url: "127.0.0.1:8000/v1", timeout_seconds: 5 }
gates: {}
agents: {}
"#;
        assert!(joined_problems(yaml).contains("http://"));
    }

    #[test]
    fn trailing_slashes_are_normalised_away() {
        let yaml = r#"
version: 1
llm_backends:
  x: { type: openai-compatible-api, base_url: "http://127.0.0.1:8000/v1/", timeout_seconds: 5 }
gates: {}
agents: {}
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
        let yaml = "version: 2\nllm_backends: {}\ngates: {}\nagents: {}\n";
        assert!(joined_problems(yaml).contains("version"));
    }

    #[test]
    fn a_typo_in_an_adapter_key_is_an_error_not_a_default() {
        let doc = REFERENCE.replace("      model: sonnet", "      modle: sonnet");
        let detail = refusal(&doc);
        assert!(detail.contains("modle"), "{detail}");
    }

    #[test]
    fn every_independent_problem_is_reported_in_one_pass() {
        let yaml = r#"
version: 9
llm_backends:
  x: { type: openai-compatible-api, base_url: "nope", timeout_seconds: 0 }
gates: {}
agents:
  r:
    max_seconds: 0
    max_concurrent: 1
    adapter:
      type: opencode
      model: { type: llm_backend, backend: missing, name: "", context_window: 0 }
      tools: []
      max_steps: 1
      command: opencode
"#;
        let problems = problems_of(parse(yaml, &p(), &env(&[])).unwrap_err());
        assert!(problems.len() >= 6, "expected several, got: {problems:#?}");
    }

    #[test]
    fn zero_timeout_is_refused() {
        let yaml = r#"
version: 1
llm_backends:
  x: { type: openai-compatible-api, base_url: "http://127.0.0.1:1/v1", timeout_seconds: 0 }
gates: {}
agents: {}
"#;
        assert!(joined_problems(yaml).contains("timeout_seconds"));
    }

    #[test]
    fn agents_are_found_by_name() {
        let c = parse(REFERENCE, &p(), &env(&[])).unwrap();
        assert_eq!(c.agent("coder").unwrap().name, "coder");
        assert!(c.agent("nope").is_none());
    }

    #[test]
    fn a_prompt_is_optional_and_absent_means_none() {
        let c = parse(REFERENCE, &p(), &env(&[])).unwrap();
        assert_eq!(c.agents["coder"].prompt, None);
        let with = REFERENCE.replacen(
            "  coder:\n    max_seconds: 900\n    max_concurrent: 1\n",
            "  coder:\n    prompt: Writes code.\n    max_seconds: 900\n    max_concurrent: 1\n",
            1,
        );
        let c = parse(&with, &p(), &env(&[])).unwrap();
        assert_eq!(c.agents["coder"].prompt.as_deref(), Some("Writes code."));
    }
}
