//! `ensemble.yaml`: load, resolve the environment, validate.
//!
//! Validation is deliberately loud and early. Every check here corresponds to a
//! failure that would otherwise surface much later and much less clearly — as a
//! 404 on the first request to an agent, as an agent that quietly runs with
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

/// The JSON Schema for `ensemble.yaml`, for editors. `ensemble schema`
/// prints it; the tests keep it in step with the structs below.
pub const SCHEMA: &str = include_str!("../ensemble.schema.json");
pub const SUPPORTED_VERSION: u32 = 1;

/// The LLM backend types that ship. Adding one is adding an adapter and a
/// variant.
pub const OPENAI_COMPATIBLE_API: &str = "openai-compatible-api";

/// The adapter types that ship. `ensemble` is Ensemble's own harness, run on
/// an LLM backend; `claude-code` and `opencode` are the Claude Code and
/// OpenCode CLIs, whole agents of their own.
pub const ENSEMBLE_AGENT: &str = "ensemble";
pub const CLAUDE_CODE_AGENT: &str = "claude-code";
pub const OPENCODE_AGENT: &str = "opencode";
const AGENT_TYPES: [&str; 3] = [ENSEMBLE_AGENT, CLAUDE_CODE_AGENT, OPENCODE_AGENT];

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
    /// Model endpoints, for agents of type `ensemble` to run on.
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
    /// Tokens of context the server really runs the model with. Required when
    /// the adapter has `tools`, whose conversation grows with every step; the agent
    /// keeps it within this, because a server that overflows silently drops the
    /// start of the conversation.
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
    /// `ensemble`, `claude-code` or `opencode`.
    #[serde(rename = "type")]
    kind: String,
    /// All: the model, as the backend, Claude Code or OpenCode names it.
    model: String,
    /// All: what the agent may do, in its harness's own tool names: Ensemble's
    /// for `ensemble`, Claude Code's (`Read`, `Bash(git:*)`) for
    /// `claude-code`, OpenCode's permissions (`read`, `bash(git *)`) for
    /// `opencode`.
    #[serde(default)]
    tools: Option<Vec<String>>,

    // --- ensemble ---
    /// Which of `llm_backends` the `ensemble` harness calls.
    #[serde(default)]
    llm_backend: Option<String>,
    /// Overrides the backend's own default for this agent. Needed when a
    /// model's baked-in sampling (Ollama's `temperature`/`presence_penalty`
    /// Modelfile defaults, for instance) is too hot for reliable tool calling.
    /// Takes precedence over whatever a caller asked for: a chat client has no
    /// business tuning an agent's reliability-critical sampling.
    #[serde(default)]
    temperature: Option<f32>,
    /// Penalty on tokens the model has already used at all, -2.0 to 2.0.
    /// Overrides the model's default; a positive one (Ollama's qwen3.5
    /// Modelfile sets 1.5) pushes the model away from repeating the paths,
    /// names and JSON keys that tool calls and code are made of.
    #[serde(default)]
    presence_penalty: Option<f32>,
    /// Penalty on tokens by how often they have been used, -2.0 to 2.0.
    #[serde(default)]
    frequency_penalty: Option<f32>,
    /// How much a reasoning model thinks before it answers: `none`, `low`,
    /// `medium` or `high`. Thinking costs tokens, and in a small context
    /// window it can use the whole reply budget before any answer or tool call
    /// appears; `none` turns it off.
    #[serde(default)]
    reasoning_effort: Option<String>,
    /// `ensemble` and `opencode`: cap on steps within one delegation.
    #[serde(default)]
    max_steps: Option<u32>,
    /// Let the agent's `bash` commands open outbound TCP connections. Off by
    /// default: a command that can read the filesystem and reach the network
    /// can send what it reads anywhere.
    #[serde(default)]
    network: Option<bool>,
    /// More paths the agent's `bash` commands may read, absolute or `~/`.
    /// They can read only the project, the system and a few toolchain
    /// directories otherwise.
    #[serde(default)]
    readable: Vec<String>,

    // --- claude-code and opencode ---
    /// The CLI, if it is not `claude` or `opencode` on PATH.
    #[serde(default)]
    command: Option<String>,
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
    /// Read a public web page as text.
    Fetch,
}

impl AgentTool {
    pub const ALL: [AgentTool; 7] = [
        AgentTool::Read,
        AgentTool::Glob,
        AgentTool::Grep,
        AgentTool::Edit,
        AgentTool::Write,
        AgentTool::Bash,
        AgentTool::Fetch,
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
            AgentTool::Fetch => "fetch",
        }
    }

    /// Can this tool change files?
    pub fn writes(self) -> bool {
        matches!(self, AgentTool::Edit | AgentTool::Write | AgentTool::Bash)
    }
}

pub const DEFAULT_MAX_STEPS: u32 = 20;
pub const DEFAULT_MAX_SECONDS: u32 = 1800;
const REASONING_EFFORTS: [&str; 4] = ["none", "low", "medium", "high"];

/// One member of the team, resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentConfig {
    pub name: String,
    pub prompt: Option<String>,
    /// See [`RawAgent::context_window`]. Set exactly when the agent is an
    /// `ensemble` agent with tools.
    pub context_window: Option<u32>,
    /// See [`RawAgent::max_seconds`].
    pub max_seconds: u32,
    pub adapter: Adapter,
}

/// What runs an agent.
#[derive(Debug, Clone, PartialEq)]
pub enum Adapter {
    Ensemble(EnsembleAdapter),
    ClaudeCode(ClaudeCodeAdapter),
    OpenCode(OpenCodeAdapter),
}

/// Ensemble's own harness: a model on one of the LLM backends, in a tool loop.
#[derive(Debug, Clone, PartialEq)]
pub struct EnsembleAdapter {
    /// Which of [`Config::llm_backends`] runs it.
    pub llm_backend: String,
    pub model: String,
    pub temperature: Option<f32>,
    /// See [`RawAdapter::presence_penalty`].
    pub presence_penalty: Option<f32>,
    /// See [`RawAdapter::frequency_penalty`].
    pub frequency_penalty: Option<f32>,
    /// See [`RawAdapter::reasoning_effort`].
    pub reasoning_effort: Option<String>,
    /// Empty: one model call, text back.
    pub tools: Vec<AgentTool>,
    pub max_steps: u32,
    /// Outbound TCP for `bash`. Meaningless without that tool.
    pub network: bool,
    /// See [`RawAdapter::readable`].
    pub readable: Vec<String>,
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
    /// `provider/model`, as OpenCode names it.
    pub model: String,
    /// OpenCode permission names, each allowed; everything else is denied.
    /// `name(pattern)` allows only matching uses, such as `bash(git *)`.
    pub tools: Vec<String>,
    pub max_steps: u32,
}

impl AgentConfig {
    pub fn kind(&self) -> &'static str {
        match self.adapter {
            Adapter::Ensemble(_) => ENSEMBLE_AGENT,
            Adapter::ClaudeCode(_) => CLAUDE_CODE_AGENT,
            Adapter::OpenCode(_) => OPENCODE_AGENT,
        }
    }

    pub fn model(&self) -> &str {
        match &self.adapter {
            Adapter::Ensemble(e) => &e.model,
            Adapter::ClaudeCode(c) => &c.model,
            Adapter::OpenCode(o) => &o.model,
        }
    }

    /// Where its model runs: an LLM backend's name, or the adapter type for an
    /// agent that brings its own.
    pub fn runs_on(&self) -> &str {
        match &self.adapter {
            Adapter::Ensemble(e) => &e.llm_backend,
            Adapter::ClaudeCode(_) => CLAUDE_CODE_AGENT,
            Adapter::OpenCode(_) => OPENCODE_AGENT,
        }
    }

    /// Does it have any tool at all?
    pub fn has_tools(&self) -> bool {
        match &self.adapter {
            Adapter::Ensemble(e) => !e.tools.is_empty(),
            Adapter::ClaudeCode(c) => !c.tools.is_empty(),
            Adapter::OpenCode(o) => !o.tools.is_empty(),
        }
    }

    /// Can it change files?
    pub fn can_change_files(&self) -> bool {
        match &self.adapter {
            Adapter::Ensemble(e) => e.tools.iter().any(|t| t.writes()),
            Adapter::ClaudeCode(c) => c.tools.iter().any(|t| claude_tool_writes(t)),
            Adapter::OpenCode(o) => o.tools.iter().any(|t| opencode_tool_writes(t)),
        }
    }

    /// Its tools, by the names its adapter uses.
    pub fn tool_names(&self) -> Vec<&str> {
        match &self.adapter {
            Adapter::Ensemble(e) => e.tools.iter().map(|t| t.as_str()).collect(),
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
            ENSEMBLE_AGENT => {
                let e = ensemble_adapter(&at, ad, &backends, &raw.llm_backends, &mut problems);
                if e.tools.is_empty() && ra.max_seconds.is_some() {
                    problems.push(format!(
                        "agents.{name}: `max_seconds` only applies to agents with `tools`; one without is bounded by its backend's `timeout_seconds`"
                    ));
                }
                if e.tools.is_empty() && ra.context_window.is_some() {
                    problems.push(format!(
                        "agents.{name}: `context_window` only applies to agents with `tools`"
                    ));
                }
                if !e.tools.is_empty() && ra.context_window.is_none() {
                    problems.push(format!(
                        "agents.{name}: an agent whose adapter has `tools` needs `context_window`, the tokens of context your server runs the model with (Ollama: OLLAMA_CONTEXT_LENGTH, default 4096)"
                    ));
                }
                Adapter::Ensemble(e)
            }
            CLAUDE_CODE_AGENT => {
                if ra.context_window.is_some() {
                    problems.push(format!(
                        "agents.{name}: `context_window` does not apply to a {CLAUDE_CODE_AGENT} agent; Claude Code manages its own context"
                    ));
                }
                Adapter::ClaudeCode(claude_code_adapter(&at, ad, &mut problems))
            }
            OPENCODE_AGENT => {
                if ra.context_window.is_some() {
                    problems.push(format!(
                        "agents.{name}: `context_window` does not apply to an {OPENCODE_AGENT} agent; OpenCode manages its own context"
                    ));
                }
                Adapter::OpenCode(opencode_adapter(&at, ad, &mut problems))
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
        let used = agents.values().any(|a| a.runs_on() == name);
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

fn parse_tools(where_: &str, names: &[String], problems: &mut Vec<String>) -> Vec<AgentTool> {
    let mut tools = Vec::new();
    for name in names {
        match AgentTool::parse(name) {
            Some(t) if !tools.contains(&t) => tools.push(t),
            Some(_) => {}
            None => {
                let known: Vec<&str> = AgentTool::ALL.iter().map(|t| t.as_str()).collect();
                problems.push(format!(
                    "{where_}: tool '{name}' is unknown. Known tools: {}.",
                    known.join(", ")
                ));
            }
        }
    }
    tools
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

fn ensemble_adapter(
    at: &str,
    ad: &RawAdapter,
    resolved: &BTreeMap<String, LlmBackend>,
    declared: &BTreeMap<String, RawLlmBackend>,
    problems: &mut Vec<String>,
) -> EnsembleAdapter {
    reject_foreign(
        at,
        ENSEMBLE_AGENT,
        &[
            ("command", ad.command.is_some()),
            ("permission_mode", ad.permission_mode.is_some()),
            ("max_turns", ad.max_turns.is_some()),
        ],
        problems,
    );
    let llm_backend = ad.llm_backend.clone().unwrap_or_default();
    if llm_backend.trim().is_empty() {
        problems.push(format!(
            "{at}: an {ENSEMBLE_AGENT} adapter needs `llm_backend`, one of `llm_backends`"
        ));
    } else if !declared.contains_key(&llm_backend) {
        let known: Vec<&str> = declared.keys().map(String::as_str).collect();
        problems.push(format!(
            "{at}: LLM backend '{llm_backend}' is not defined. Defined LLM backends: {}.",
            if known.is_empty() {
                "(none)".to_string()
            } else {
                known.join(", ")
            }
        ));
    } else if !resolved.contains_key(&llm_backend) {
        // Declared but failed to resolve. Its own problem is already in the
        // list; adding "not defined" here would be a second, misleading one.
    }
    let tools = parse_tools(at, ad.tools.as_deref().unwrap_or_default(), problems);
    let has_bash = tools.contains(&AgentTool::Bash);
    if !ad.readable.is_empty() && !has_bash {
        problems.push(format!(
            "{at}: `readable` only applies to the `bash` tool, which this agent does not have"
        ));
    }
    for path in &ad.readable {
        if !path.starts_with('/') && !path.starts_with("~/") {
            problems.push(format!(
                "{at}: readable path '{path}' must be absolute or start with ~/"
            ));
        }
    }
    if ad.network.is_some() && !has_bash {
        problems.push(format!(
            "{at}: `network` only applies to the `bash` tool, which this agent does not have"
        ));
    }
    if ad.max_steps == Some(0) {
        problems.push(format!("{at}: `max_steps` must be greater than 0"));
    }
    if let Some(effort) = &ad.reasoning_effort
        && !REASONING_EFFORTS.contains(&effort.as_str())
    {
        problems.push(format!(
            "{at}: `reasoning_effort` is '{effort}'; must be one of {}",
            REASONING_EFFORTS.join(", ")
        ));
    }
    if let Some(t) = ad.temperature
        && !(0.0..=2.0).contains(&t)
    {
        problems.push(format!(
            "{at}: `temperature` is {t}; must be between 0.0 and 2.0"
        ));
    }
    for (key, value) in [
        ("presence_penalty", ad.presence_penalty),
        ("frequency_penalty", ad.frequency_penalty),
    ] {
        if let Some(v) = value
            && !(-2.0..=2.0).contains(&v)
        {
            problems.push(format!(
                "{at}: `{key}` is {v}; must be between -2.0 and 2.0"
            ));
        }
    }
    EnsembleAdapter {
        llm_backend,
        model: ad.model.clone(),
        temperature: ad.temperature,
        presence_penalty: ad.presence_penalty,
        frequency_penalty: ad.frequency_penalty,
        reasoning_effort: ad.reasoning_effort.clone(),
        tools,
        max_steps: ad.max_steps.unwrap_or(DEFAULT_MAX_STEPS),
        network: ad.network.unwrap_or(false),
        readable: ad.readable.clone(),
    }
}

fn opencode_adapter(at: &str, ad: &RawAdapter, problems: &mut Vec<String>) -> OpenCodeAdapter {
    reject_foreign(
        at,
        OPENCODE_AGENT,
        &[
            ("llm_backend", ad.llm_backend.is_some()),
            ("temperature", ad.temperature.is_some()),
            ("presence_penalty", ad.presence_penalty.is_some()),
            ("frequency_penalty", ad.frequency_penalty.is_some()),
            ("reasoning_effort", ad.reasoning_effort.is_some()),
            ("network", ad.network.is_some()),
            ("readable", !ad.readable.is_empty()),
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
    if !ad.model.contains('/') {
        problems.push(format!(
            "{at}: model '{}' must be provider/model, as `opencode models` lists it",
            ad.model
        ));
    }
    let command = ad.command.clone().unwrap_or_else(|| "opencode".into());
    if command.trim().is_empty() {
        problems.push(format!("{at}: `command` is empty"));
    }
    OpenCodeAdapter {
        command,
        model: ad.model.clone(),
        tools,
        max_steps: ad.max_steps.unwrap_or(OPENCODE_DEFAULT_MAX_STEPS),
    }
}

fn claude_code_adapter(at: &str, ad: &RawAdapter, problems: &mut Vec<String>) -> ClaudeCodeAdapter {
    reject_foreign(
        at,
        CLAUDE_CODE_AGENT,
        &[
            ("llm_backend", ad.llm_backend.is_some()),
            ("temperature", ad.temperature.is_some()),
            ("presence_penalty", ad.presence_penalty.is_some()),
            ("frequency_penalty", ad.frequency_penalty.is_some()),
            ("reasoning_effort", ad.reasoning_effort.is_some()),
            ("max_steps", ad.max_steps.is_some()),
            ("network", ad.network.is_some()),
            ("readable", !ad.readable.is_empty()),
        ],
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
            "llm_llm_backends.{name}: `timeout_seconds` must be greater than 0"
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
                "llm_llm_backends.{name}: type '{other}' is unknown. Known types: \
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
            "llm_llm_backends.{name}: `base_url_env` is required for type '{OPENAI_COMPATIBLE_API}' \
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
                "llm_llm_backends.{name}: ${var} is unset and there is no `base_url_default`. \
                 Export ${var} (for example http://127.0.0.1:8000/v1) before using this backend."
            ));
            return None;
        }
    };

    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        problems.push(format!(
            "llm_llm_backends.{name}: base URL '{base_url}' must start with http:// or https://"
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
        PathBuf::from("ensemble.yaml")
    }

    /// The ensemble adapter of an agent that has one.
    fn ens<'a>(c: &'a Config, agent: &str) -> &'a EnsembleAdapter {
        match &c.agents[agent].adapter {
            Adapter::Ensemble(e) => e,
            other => panic!("not an ensemble agent: {other:?}"),
        }
    }

    /// The document from the README, which is also the shipped example.
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
      llm_backend: openai-local
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
        assert_eq!(c.llm_backends.len(), 2);
        assert_eq!(c.agents.len(), 3);
        assert_eq!(c.agent_names(), vec!["coder", "planner", "reviewer"]);
    }

    const AGENT_DOC: &str = r#"
version: 1
llm_backends:
  openai-local:
    type: openai-compatible-api
    base_url_env: OPENAI_LOCAL_BASE_URL
    base_url_default: "http://127.0.0.1:11434/v1"
agents:
  coder:
    context_window: 8000
    adapter:
      type: ensemble
      llm_backend: openai-local
      model: m
      tools: [read, grep, edit]
      max_steps: 5
  thinker:
    adapter:
      type: ensemble
      llm_backend: openai-local
      model: m
"#;

    #[test]
    fn a_role_with_tools_is_an_agent_and_one_without_is_think_only() {
        let c = parse(AGENT_DOC, &p(), &env(&[])).unwrap();
        assert_eq!(
            ens(&c, "coder").tools,
            vec![AgentTool::Read, AgentTool::Grep, AgentTool::Edit]
        );
        assert_eq!(ens(&c, "coder").max_steps, 5);
        assert_eq!(c.agents["coder"].max_seconds, DEFAULT_MAX_SECONDS);
        assert!(ens(&c, "thinker").tools.is_empty());
        assert_eq!(ens(&c, "thinker").max_steps, DEFAULT_MAX_STEPS);
        assert_eq!(c.agents["coder"].context_window, Some(8000));
        assert_eq!(c.agents["thinker"].context_window, None);
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
    fn reasoning_effort_is_read_and_checked() {
        let doc = AGENT_DOC.replace("max_steps: 5", "max_steps: 5\n      reasoning_effort: none");
        let c = parse(&doc, &p(), &env(&[])).unwrap();
        assert_eq!(ens(&c, "coder").reasoning_effort.as_deref(), Some("none"));
        assert_eq!(ens(&c, "thinker").reasoning_effort, None);

        let bad = AGENT_DOC.replace("max_steps: 5", "max_steps: 5\n      reasoning_effort: lots");
        let Err(ConfigError::Invalid { problems, .. }) = parse(&bad, &p(), &env(&[])) else {
            panic!("expected invalid");
        };
        assert!(
            problems.iter().any(|p| p.contains("must be one of")),
            "{problems:?}"
        );
    }

    #[test]
    fn context_window_is_required_for_tools_and_refused_without_them() {
        let missing = AGENT_DOC.replace("    context_window: 8000\n", "");
        let Err(ConfigError::Invalid { problems, .. }) = parse(&missing, &p(), &env(&[])) else {
            panic!("expected invalid");
        };
        assert!(
            problems
                .iter()
                .any(|p| p.contains("needs `context_window`")),
            "{problems:?}"
        );

        let zero = AGENT_DOC.replace("context_window: 8000", "context_window: 0");
        let Err(ConfigError::Invalid { problems, .. }) = parse(&zero, &p(), &env(&[])) else {
            panic!("expected invalid");
        };
        assert!(
            problems.iter().any(|p| p.contains("greater than 0")),
            "{problems:?}"
        );

        let think_only = AGENT_DOC.replace(
            "  thinker:\n    adapter:",
            "  thinker:\n    context_window: 8000\n    adapter:",
        );
        let Err(ConfigError::Invalid { problems, .. }) = parse(&think_only, &p(), &env(&[])) else {
            panic!("expected invalid");
        };
        assert!(
            problems
                .iter()
                .any(|p| p.contains("only applies to agents with")),
            "{problems:?}"
        );
    }

    #[test]
    fn bash_is_a_writing_tool_and_network_is_off_unless_asked_for() {
        let doc = AGENT_DOC.replace("[read, grep, edit]", "[read, bash]");
        let c = parse(&doc, &p(), &env(&[])).unwrap();
        assert!(ens(&c, "coder").tools.contains(&AgentTool::Bash));
        assert!(AgentTool::Bash.writes());
        assert!(!ens(&c, "coder").network);

        let on = doc.replace("max_steps: 5", "max_steps: 5\n      network: true");
        assert!(ens(&parse(&on, &p(), &env(&[])).unwrap(), "coder").network);
    }

    #[test]
    fn readable_paths_need_bash_and_an_absolute_or_home_path() {
        let doc = AGENT_DOC.replace("tools: [read, grep, edit]", "tools: [read, bash]");
        let ok = doc.replace(
            "max_steps: 5",
            "max_steps: 5\n      readable: [\"~/.m2\", /srv]",
        );
        assert_eq!(
            ens(&parse(&ok, &p(), &env(&[])).unwrap(), "coder").readable,
            vec!["~/.m2", "/srv"]
        );
        let relative = doc.replace("max_steps: 5", "max_steps: 5\n      readable: [data]");
        let Err(ConfigError::Invalid { problems, .. }) = parse(&relative, &p(), &env(&[])) else {
            panic!("expected invalid");
        };
        assert!(
            problems.iter().any(|p| p.contains("'data'")),
            "{problems:?}"
        );
        let no_bash = AGENT_DOC.replace("max_steps: 5", "max_steps: 5\n      readable: [/srv]");
        let Err(ConfigError::Invalid { problems, .. }) = parse(&no_bash, &p(), &env(&[])) else {
            panic!("expected invalid");
        };
        assert!(
            problems.iter().any(|p| p.contains("`readable`")),
            "{problems:?}"
        );
    }

    #[test]
    fn network_without_bash_is_rejected() {
        let doc = AGENT_DOC.replace("max_steps: 5", "max_steps: 5\n      network: true");
        let Err(ConfigError::Invalid { problems, .. }) = parse(&doc, &p(), &env(&[])) else {
            panic!("expected invalid");
        };
        assert!(
            problems.iter().any(|p| p.contains("`network`")),
            "{problems:?}"
        );
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
        let mut adapters = schema_keys(&defs["ensemble_adapter"]);
        adapters.extend(schema_keys(&defs["claude_code_adapter"]));
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
        let role = &schema["definitions"]["ensemble_adapter"]["properties"];
        assert_eq!(role["type"]["const"], ENSEMBLE_AGENT);
        let claude = &schema["definitions"]["claude_code_adapter"]["properties"];
        assert_eq!(claude["type"]["const"], CLAUDE_CODE_AGENT);
        assert_eq!(
            AGENT_TYPES,
            [ENSEMBLE_AGENT, CLAUDE_CODE_AGENT, OPENCODE_AGENT]
        );
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

        let mut tools = schema_enum(&role["tools"]["items"]);
        tools.sort();
        let mut known: Vec<String> = AgentTool::ALL
            .iter()
            .map(|t| t.as_str().to_string())
            .collect();
        known.sort();
        assert_eq!(tools, known);

        assert_eq!(schema_enum(&role["reasoning_effort"]), REASONING_EFFORTS);
        assert_eq!(role["max_steps"]["default"], DEFAULT_MAX_STEPS);
        assert_eq!(agent["max_seconds"]["default"], DEFAULT_MAX_SECONDS);
        assert_eq!(
            backend["timeout_seconds"]["default"],
            DEFAULT_TIMEOUT_SECONDS
        );
        assert_eq!(schema["properties"]["version"]["const"], SUPPORTED_VERSION);
    }

    #[test]
    fn the_shipped_example_names_the_schema_for_editors() {
        let example = include_str!("../ensemble.yaml");
        let id = schema()["$id"].as_str().unwrap().to_string();
        assert!(
            example.contains(&format!("# yaml-language-server: $schema={id}")),
            "ensemble.yaml should start with the schema modeline"
        );
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
    fn a_claude_code_agent_needs_no_llm_backend_and_defaults_to_read_only() {
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
            "      model: sonnet
",
            "      model: sonnet
      llm_backend: x
      max_steps: 3
",
        );
        let joined = problems_of(parse(&doc, &p(), &env(&[])).unwrap_err()).join(
            "
",
        );
        assert!(
            joined.contains("agents.reviewer.adapter: `llm_backend` is not a field of adapter type 'claude-code'"),
            "{joined}"
        );
        assert!(joined.contains("`max_steps` is not a field"), "{joined}");

        let ens = AGENT_DOC.replace(
            "      max_steps: 5
",
            "      max_steps: 5
      max_turns: 4
",
        );
        let joined = problems_of(parse(&ens, &p(), &env(&[])).unwrap_err()).join(
            "
",
        );
        assert!(
            joined.contains("`max_turns` is not a field of adapter type 'ensemble'"),
            "{joined}"
        );
    }

    #[test]
    fn a_claude_code_agent_takes_no_context_window_and_one_tool_per_entry() {
        let doc = CLAUDE_DOC
            .replace(
                "    prompt: Reviews code.
",
                "    prompt: Reviews code.
    context_window: 8000
",
            )
            .replace("[Read, Edit, ", "[\"Read,Edit\", ");
        let joined = problems_of(parse(&doc, &p(), &env(&[])).unwrap_err()).join(
            "
",
        );
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
        let Adapter::OpenCode(looker) = &c.agents["looker"].adapter else {
            panic!("an opencode agent");
        };
        assert_eq!(looker.tools, OPENCODE_DEFAULT_TOOLS);
        assert_eq!(
            (looker.command.as_str(), looker.max_steps),
            ("opencode", OPENCODE_DEFAULT_MAX_STEPS)
        );
        assert!(!c.agents["looker"].can_change_files());
        let Adapter::OpenCode(doer) = &c.agents["doer"].adapter else {
            panic!("an opencode agent");
        };
        assert_eq!(doer.tools, ["read", "edit", "bash(git *)"]);
        assert_eq!(doer.max_steps, 12);
        assert!(c.agents["doer"].can_change_files());

        let bad = r#"
version: 1
agents:
  x:
    context_window: 8000
    adapter: { type: opencode, model: big-pickle, tools: ["read,edit"], max_turns: 3, temperature: 0.1 }
"#;
        let joined = problems_of(parse(bad, &p(), &env(&[])).unwrap_err()).join("\n");
        assert!(
            joined.contains("OpenCode manages its own context"),
            "{joined}"
        );
        assert!(joined.contains("must be provider/model"), "{joined}");
        assert!(
            joined.contains("tool 'read,edit' must be an OpenCode permission"),
            "{joined}"
        );
        assert!(
            joined.contains("`max_turns` is not a field of adapter type 'opencode'"),
            "{joined}"
        );
        assert!(joined.contains("`temperature` is not a field"), "{joined}");
    }

    #[test]
    fn an_unknown_adapter_type_lists_the_known_ones() {
        let doc = AGENT_DOC.replace(
            "  thinker:\n    adapter:\n      type: ensemble",
            "  thinker:\n    adapter:\n      type: codex",
        );
        let problems = problems_of(parse(&doc, &p(), &env(&[])).unwrap_err());
        assert!(
            problems.iter().any(
                |p| p.contains("agents.thinker.adapter: type 'codex' is unknown")
                    && p.contains("ensemble, claude-code, opencode")
            ),
            "{problems:?}"
        );
    }

    #[test]
    fn max_seconds_must_be_positive_and_needs_tools() {
        let set = AGENT_DOC.replace("max_steps: 5", "max_steps: 5\n    max_seconds: 60");
        assert_eq!(
            parse(&set, &p(), &env(&[])).unwrap().agents["coder"].max_seconds,
            60
        );
        let zero = AGENT_DOC.replace("max_steps: 5", "max_steps: 5\n    max_seconds: 0");
        let Err(ConfigError::Invalid { problems, .. }) = parse(&zero, &p(), &env(&[])) else {
            panic!("expected invalid");
        };
        assert!(
            problems.iter().any(|p| p.contains("`max_seconds` must be")),
            "{problems:?}"
        );
        let think_only = format!(
            "{AGENT_DOC}  idle:\n    max_seconds: 60\n    adapter: {{ type: ensemble, llm_backend: openai-local, model: m }}\n"
        );
        let Err(ConfigError::Invalid { problems, .. }) = parse(&think_only, &p(), &env(&[])) else {
            panic!("expected invalid");
        };
        assert!(
            problems
                .iter()
                .any(|p| p.contains("agents.idle: `max_seconds` only")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_role_may_override_the_backends_default_temperature() {
        let doc = r#"
version: 1
llm_backends:
  openai-local:
    type: openai-compatible-api
    base_url_env: OPENAI_LOCAL_BASE_URL
    base_url_default: "http://127.0.0.1:11434/v1"
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
        let c = parse(doc, &p(), &env(&[])).unwrap();
        assert_eq!(ens(&c, "scout").temperature, Some(0.2));
        // Unset is unset, not defaulted to something: the backend's own
        // default still applies.
        assert_eq!(ens(&c, "plain").temperature, None);
    }

    #[test]
    fn penalties_are_read_and_bounded() {
        let doc = AGENT_DOC.replace(
            "max_steps: 5",
            "max_steps: 5\n      presence_penalty: 0\n      frequency_penalty: -0.5",
        );
        let c = parse(&doc, &p(), &env(&[])).unwrap();
        assert_eq!(ens(&c, "coder").presence_penalty, Some(0.0));
        assert_eq!(ens(&c, "coder").frequency_penalty, Some(-0.5));
        assert_eq!(ens(&c, "thinker").presence_penalty, None);

        let bad = AGENT_DOC.replace(
            "max_steps: 5",
            "max_steps: 5\n      presence_penalty: 2.5\n      frequency_penalty: -3",
        );
        let joined = problems_of(parse(&bad, &p(), &env(&[])).unwrap_err()).join("\n");
        assert!(joined.contains("`presence_penalty` is 2.5"), "{joined}");
        assert!(joined.contains("`frequency_penalty` is -3"), "{joined}");
    }

    #[test]
    fn a_temperature_outside_zero_to_two_is_rejected() {
        let doc = r#"
version: 1
llm_backends:
  openai-local:
    type: openai-compatible-api
    base_url_env: OPENAI_LOCAL_BASE_URL
    base_url_default: "http://127.0.0.1:11434/v1"
agents:
  scout:
    adapter:
      type: ensemble
      llm_backend: openai-local
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
    fn a_role_naming_an_undefined_backend_is_fatal_and_lists_the_real_ones() {
        let yaml = r#"
version: 1
llm_backends:
  openai-local: { type: openai-compatible-api, base_url_env: U, base_url_default: "http://127.0.0.1:1/v1" }
agents:
  coder:
    adapter:
      type: ensemble
      llm_backend: opanai-local
      model: m
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
llm_backends:
  x: { type: openai-compatible-api, base_url_env: U, base_url_default: "http://127.0.0.1:1/v1" }
agents:
  coder:
    adapter:
      type: ensemble
      llm_backend: x
      model: "  "
"#;
        let problems = problems_of(parse(yaml, &p(), &env(&[])).unwrap_err());
        assert!(
            problems
                .iter()
                .any(|p| p.contains("agents.coder") && p.contains("model")),
            "{problems:?}"
        );
    }

    #[test]
    fn an_unknown_backend_type_lists_the_known_ones() {
        let yaml = r#"
version: 1
llm_backends:
  x: { type: ollama }
"#;
        let problems = problems_of(parse(yaml, &p(), &env(&[])).unwrap_err());
        let joined = problems.join("\n");
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
        let problems = problems_of(parse(yaml, &p(), &env(&[])).unwrap_err());
        let joined = problems.join("\n");
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
llm_backends:
  x: { type: openai-compatible-api, base_url_env: U, base_url_default: "http://127.0.0.1:1/v1" }
agents:
  coder:
    adapter:
      type: ensemble
      llm_backend: x
      modle: m
"#;
        let err = parse(yaml, &p(), &env(&[])).unwrap_err();
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
      type: ensemble
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
        let problems = problems_of(parse(yaml, &p(), &env(&[])).unwrap_err());
        assert!(
            problems.iter().any(|p| p.contains("timeout_seconds")),
            "{problems:?}"
        );
    }

    #[test]
    fn agents_are_found_by_name() {
        let c = parse(REFERENCE, &p(), &env(&[])).unwrap();
        assert_eq!(c.agent("coder").unwrap().name, "coder");
        assert!(c.agent("nope").is_none());
    }
}
