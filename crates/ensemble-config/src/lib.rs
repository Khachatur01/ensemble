//! `config.yaml`, the helper team: load, resolve the environment, validate.
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
//! also come from a file passed with `--env-file`, out of git.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

pub const CONFIG_FILENAME: &str = "config.yaml";

/// The JSON Schema for `config.yaml`, for editors. `ensemble-runner team schema`
/// prints it; the tests keep it in step with the structs below.
pub const SCHEMA: &str = include_str!("../../../ensemble.schema.json");

/// The config `ensemble-runner team new` writes: every setting at least once, with what
/// it does. The tests keep it loading and complete.
pub const SAMPLE: &str = include_str!("sample.yaml");

/// The first line of a config, which points an editor at the JSON Schema.
pub const SCHEMA_URL_LINE: &str = "# yaml-language-server: $schema=https://raw.githubusercontent.com/Khachatur01/ensemble/master/ensemble.schema.json";
pub const SUPPORTED_VERSION: u32 = 1;

/// The LLM backend types that ship. Adding one is adding an adapter and a
/// variant.
pub const OPENAI_COMPATIBLE_API: &str = "openai-compatible-api";
/// Anthropic's own Messages API.
pub const ANTHROPIC_API: &str = "anthropic-api";

/// The adapter types that ship: the Claude Code and OpenCode CLIs, whole
/// agents of their own that Ensemble starts in the project.
pub const CLAUDE_CODE_AGENT: &str = "claude-code";
pub const OPENCODE_AGENT: &str = "opencode";
/// The Codex CLI, run headless with `codex exec`.
pub const CODEX_AGENT: &str = "codex";
/// What Codex's sandbox lets a run do.
pub const CODEX_SANDBOXES: [&str; 3] = ["read-only", "workspace-write", "danger-full-access"];
/// The light adapter: one request to an LLM backend, no CLI and no tools.
pub const DIRECT_AGENT: &str = "direct";
/// An agent loop in Ensemble itself, on an LLM backend: it reads, edits and
/// runs commands with tools of its own, so no agent CLI has to be installed.
pub const BUILTIN_AGENT: &str = "builtin";
/// The tools a `builtin` agent may be given.
pub const BUILTIN_TOOLS: [&str; 7] = ["read", "list", "grep", "glob", "write", "edit", "shell"];
/// An agent a node on a hub server offers: the task and a copy of the files go
/// there, and what the agent changed comes back.
pub const NODE_AGENT: &str = "node";

/// Where an OpenCode agent's model comes from.
pub const PROVIDER_MODEL: &str = "provider";
pub const LLM_BACKEND_MODEL: &str = "llm_backend";

// ---------------------------------------------------------------------------
// The document, resolved
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendSettings {
    OpenAiCompatible {
        base_url: String,
        api_key: Option<String>,
    },
    /// `POST {base_url}/v1/messages`, the key in `x-api-key`.
    Anthropic {
        base_url: String,
        api_key: Option<String>,
    },
}

impl BackendSettings {
    pub fn base_url(&self) -> &str {
        match self {
            BackendSettings::OpenAiCompatible { base_url, .. }
            | BackendSettings::Anthropic { base_url, .. } => base_url,
        }
    }

    pub fn api_key(&self) -> Option<&str> {
        match self {
            BackendSettings::OpenAiCompatible { api_key, .. }
            | BackendSettings::Anthropic { api_key, .. } => api_key.as_deref(),
        }
    }
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
            BackendSettings::Anthropic { .. } => ANTHROPIC_API,
        }
    }
}

/// One member of the helper team, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    /// The name the calling agent delegates by.
    pub name: String,
    pub prompt: Option<String>,
    /// See [`RawAgent::max_seconds`].
    pub max_seconds: u32,
    /// See [`RawAgent::max_concurrent`].
    pub max_concurrent: u32,
    pub isolation: Isolation,
    pub budget: Budget,
    /// The shared limit it takes `max_concurrent` and `budget` from, when it
    /// names one: its slots and its budget are then shared with the others.
    pub limit: Option<String>,
    pub adapter: Adapter,
}

/// Where an agent's tasks run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Isolation {
    /// In the project directory, alongside everything else.
    Shared,
    /// Each task in its own git worktree, on a branch named after the task,
    /// where the agent's work is committed. Ensemble never merges it.
    Worktree,
}

/// How much an agent may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    Unlimited,
    /// At most this many runs started in any hour.
    PerHour {
        delegations: u32,
    },
    /// At most this many tokens used by runs in any hour, as the runs report.
    PerHourTokens {
        tokens: u64,
    },
}

/// A limit that several agents share: their runs together stay within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub max_concurrent: u32,
    pub budget: Budget,
}

/// What runs an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Adapter {
    ClaudeCode(ClaudeCodeAdapter),
    OpenCode(OpenCodeAdapter),
    Codex(CodexAdapter),
    Direct(DirectAdapter),
    Builtin(BuiltinAdapter),
    Node(NodeAdapter),
}

/// An agent that runs on a node of a hub server: one the node offers, never a
/// definition sent from here, so the node's owner decides what runs there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAdapter {
    /// The hub server, `ws://` or `wss://`, signed in to with `ensemble-runner login`.
    pub hub: String,
    /// The agent the nodes offer.
    pub agent: String,
    /// The nodes that may run it, as `<owner>/<name>`, tried in order.
    pub nodes: Vec<String>,
    /// `<agent> on <node> or <node>`, for listings.
    pub label: String,
}

/// The Codex CLI, run headless in the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexAdapter {
    pub command: String,
    pub model: String,
    /// `read-only`, `workspace-write` or `danger-full-access`: what Codex's own
    /// sandbox lets it do. It is what limits the agent.
    pub sandbox: String,
}

/// Can a Codex sandbox change files?
pub fn codex_sandbox_writes(sandbox: &str) -> bool {
    sandbox != "read-only"
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
    pub system_prompt: SystemPrompt,
    pub max_steps: u32,
}

/// What an OpenCode agent's `prompt` does to OpenCode's own system prompt,
/// the one written for the model's family that teaches it OpenCode's tools.
/// Either way OpenCode still adds what it knows of the machine and the
/// project's `AGENTS.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SystemPrompt {
    /// After OpenCode's own: the agent's prompt goes to OpenCode as an
    /// instruction file.
    Append,
    /// In place of OpenCode's own, for a small model whose context cannot
    /// spare it. It needs a `prompt`.
    Replace,
}

/// One chat request to an LLM backend, with no CLI in between: a light agent
/// with no tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectAdapter {
    /// The models it may run on, in the order they are tried.
    pub fallback: Fallback,
    pub max_tokens: u32,
}

impl DirectAdapter {
    /// The LLM backend of its first model.
    pub fn backend(&self) -> &str {
        self.fallback.first().map_or("", |c| c.backend.as_str())
    }

    /// The id its first model is served under.
    pub fn model(&self) -> &str {
        self.fallback.first().map_or("", |c| c.model.as_str())
    }
}

/// An agent loop in Ensemble itself: the model on an LLM backend asks for
/// tools, Ensemble runs them in the working directory and answers, until the
/// model is done or `max_steps` is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinAdapter {
    /// The models it may run on, in the order they are tried. Each has its
    /// `context_window`.
    pub fallback: Fallback,
    /// Of [`BUILTIN_TOOLS`], the ones it may use.
    pub tools: Vec<String>,
    /// Most model calls for one run.
    pub max_steps: u32,
    /// Most tokens of answer per model call.
    pub max_tokens: u32,
}

impl BuiltinAdapter {
    /// The LLM backend of its first model.
    pub fn backend(&self) -> &str {
        self.fallback.first().map_or("", |c| c.backend.as_str())
    }

    /// The id its first model is served under.
    pub fn model(&self) -> &str {
        self.fallback.first().map_or("", |c| c.model.as_str())
    }
}

/// Can this `builtin` tool change files?
pub fn builtin_tool_writes(tool: &str) -> bool {
    matches!(tool, "write" | "edit" | "shell")
}
/// What an agent does when none of the models in its `try` list can be used:
/// no backend answers, or none serves the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WhenNone {
    /// The delegation or task fails, saying what each backend answered.
    Fail,
    /// A task waits in the queue and is tried again later. A delegation, which
    /// someone is waiting for, fails.
    Wait,
}

/// One model an agent may run on: the backend that serves it, and its id there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub backend: String,
    /// The id the endpoint serves the model under.
    pub model: String,
    /// Tokens of context the server runs it with, for an OpenCode agent.
    pub context_window: Option<u32>,
}

/// The models an agent may run on, in the order they are tried. The first one
/// that is reachable and serves the model is the one a run uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fallback {
    pub candidates: Vec<Candidate>,
    pub when_none: WhenNone,
}

impl Fallback {
    /// The first model: the one it runs on whenever that one is there.
    pub fn first(&self) -> Option<&Candidate> {
        self.candidates.first()
    }

    /// This list with only model `index` in it: what one run is bound to.
    pub fn bound(&self, index: usize) -> Fallback {
        Fallback {
            candidates: self.candidates.get(index).cloned().into_iter().collect(),
            when_none: self.when_none,
        }
    }
}

/// Where an OpenCode agent's model comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenCodeModel {
    /// `provider/model`, as OpenCode names it, on a provider it is already
    /// set up for.
    Provider { model: String },
    /// Models served by [`Config::llm_backends`], in the order they are tried.
    LlmBackend(Fallback),
}

impl OpenCodeAdapter {
    /// The model: `provider/model`, or the id an LLM backend serves its first
    /// model under.
    pub fn model_id(&self) -> &str {
        match &self.model {
            OpenCodeModel::Provider { model } => model,
            OpenCodeModel::LlmBackend(f) => f.first().map_or("", |c| c.model.as_str()),
        }
    }

    /// The LLM backend its first model runs on, when it runs on one.
    pub fn llm_backend(&self) -> Option<&str> {
        match &self.model {
            OpenCodeModel::Provider { .. } => None,
            OpenCodeModel::LlmBackend(f) => f.first().map(|c| c.backend.as_str()),
        }
    }

    /// Tokens of context the first model's server runs it with, when it runs
    /// on an LLM backend.
    pub fn context_window(&self) -> Option<u32> {
        match &self.model {
            OpenCodeModel::Provider { .. } => None,
            OpenCodeModel::LlmBackend(f) => f.first().and_then(|c| c.context_window),
        }
    }
}

impl AgentConfig {
    pub fn kind(&self) -> &'static str {
        match self.adapter {
            Adapter::ClaudeCode(_) => CLAUDE_CODE_AGENT,
            Adapter::OpenCode(_) => OPENCODE_AGENT,
            Adapter::Codex(_) => CODEX_AGENT,
            Adapter::Direct(_) => DIRECT_AGENT,
            Adapter::Builtin(_) => BUILTIN_AGENT,
            Adapter::Node(_) => NODE_AGENT,
        }
    }

    pub fn model(&self) -> &str {
        match &self.adapter {
            Adapter::ClaudeCode(c) => &c.model,
            Adapter::OpenCode(o) => o.model_id(),
            Adapter::Codex(c) => &c.model,
            Adapter::Direct(d) => d.model(),
            Adapter::Builtin(b) => b.model(),
            Adapter::Node(n) => &n.label,
        }
    }

    /// The LLM backend its first model runs on, when it runs on one.
    pub fn llm_backend(&self) -> Option<&str> {
        match &self.adapter {
            Adapter::ClaudeCode(_) | Adapter::Codex(_) | Adapter::Node(_) => None,
            Adapter::OpenCode(o) => o.llm_backend(),
            Adapter::Direct(d) => Some(d.backend()),
            Adapter::Builtin(b) => Some(b.backend()),
        }
    }

    /// The models it may run on, in order, when it runs on LLM backends.
    pub fn fallback(&self) -> Option<&Fallback> {
        match &self.adapter {
            Adapter::ClaudeCode(_) | Adapter::Codex(_) | Adapter::Node(_) => None,
            Adapter::OpenCode(o) => match &o.model {
                OpenCodeModel::Provider { .. } => None,
                OpenCodeModel::LlmBackend(f) => Some(f),
            },
            Adapter::Direct(d) => Some(&d.fallback),
            Adapter::Builtin(b) => Some(&b.fallback),
        }
    }

    /// Every LLM backend it may run on, with the model it would ask there.
    pub fn llm_backends(&self) -> Vec<(&str, &str)> {
        self.fallback().map_or_else(Vec::new, |f| {
            f.candidates
                .iter()
                .map(|c| (c.backend.as_str(), c.model.as_str()))
                .collect()
        })
    }

    /// This agent bound to model `index` of its `try` list, for one run. An
    /// agent with no such list is returned as it is.
    pub fn bound_to(&self, index: usize) -> AgentConfig {
        let mut bound = self.clone();
        match &mut bound.adapter {
            Adapter::OpenCode(OpenCodeAdapter {
                model: OpenCodeModel::LlmBackend(f),
                ..
            }) => *f = f.bound(index),
            Adapter::Direct(d) => d.fallback = d.fallback.bound(index),
            Adapter::Builtin(b) => b.fallback = b.fallback.bound(index),
            _ => {}
        }
        bound
    }

    /// Tokens of context its model's server runs it with, when it runs on an
    /// LLM backend.
    pub fn context_window(&self) -> Option<u32> {
        match &self.adapter {
            Adapter::OpenCode(o) => o.context_window(),
            Adapter::Builtin(b) => b.fallback.first().and_then(|c| c.context_window),
            _ => None,
        }
    }
    /// Where it runs: the adapter type, and the LLM backend when there is one.
    pub fn runs_on(&self) -> String {
        if let Adapter::Node(n) = &self.adapter {
            return format!("{NODE_AGENT} {}", n.nodes.join(" or "));
        }
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
            Adapter::Codex(c) => codex_sandbox_writes(&c.sandbox),
            Adapter::Direct(_) => false,
            Adapter::Builtin(b) => b.tools.iter().any(|t| builtin_tool_writes(t)),
            // What it changed there comes back and is written here.
            Adapter::Node(_) => true,
        }
    }

    /// Its tools, by the names its adapter uses.
    pub fn tool_names(&self) -> Vec<&str> {
        match &self.adapter {
            Adapter::ClaudeCode(c) => c.tools.iter().map(String::as_str).collect(),
            Adapter::OpenCode(o) => o.tools.iter().map(String::as_str).collect(),
            // Its sandbox is what stands in for a list of tools.
            Adapter::Codex(c) => vec![c.sandbox.as_str()],
            Adapter::Direct(_) => Vec::new(),
            Adapter::Builtin(b) => b.tools.iter().map(String::as_str).collect(),
            // The node's own agent has tools; they are its owner's to choose.
            Adapter::Node(_) => vec!["(on the node)"],
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
    /// Shared limits, by name.
    pub limits: BTreeMap<String, Limit>,
    /// The team's gates, by name.
    pub gates: BTreeMap<String, Gate>,
    /// The helper team: every agent, by name. There is no leader in the file:
    /// the agent that calls Ensemble is it.
    pub agents: BTreeMap<String, AgentConfig>,
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
    /// The agent called `name`.
    pub fn agent(&self, name: &str) -> Option<&AgentConfig> {
        self.agents.get(name)
    }

    /// The agents' names.
    pub fn agent_names(&self) -> Vec<&str> {
        self.agents.keys().map(String::as_str).collect()
    }

    /// The gate called `name`.
    pub fn gate(&self, name: &str) -> Option<&Gate> {
        self.gates.get(name)
    }

    /// The gate a task of `agent` names `name`: of the agent's own team when
    /// the helper team is several combined (`web/coder` names `web/test`), or
    /// the one `name` names as `<team>/<gate>`.
    pub fn gate_for(&self, agent: &str, name: &str) -> Option<&Gate> {
        if name.contains('/') {
            return self.gates.get(name);
        }
        match teams::team_of(agent) {
            Some(team) => self.gates.get(&format!("{team}/{name}")),
            None => self.gates.get(name),
        }
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
    /// No `hub.yaml` in the hub's folder.
    HubNotFound {
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
                "no {CONFIG_FILENAME} at {}. `ensemble-runner team list` lists the teams; `ensemble-runner team new <name>` makes one.",
                path.display()
            ),
            ConfigError::HubNotFound { path } => write!(
                f,
                "no {HUB_FILENAME} at {}. Make one with `ensemble-hub init`, or run the hub on the computer that has it.",
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

pub mod edit;
mod env;
pub mod hub;
mod interfaces;
mod raw;
pub mod runner;
pub mod settings;
pub mod settings_file;
pub mod teams;
mod validate;

pub use env::*;
pub use hub::{
    Embed, HUB_FILENAME, Hub, IssueTracker, Knowledge, Permission, Recall, Registration, Server,
    Tasks, hub_path, load_hub, parse_hub,
};
pub use interfaces::{Interfaces, Mcp};
pub(crate) use interfaces::{RawInterfaces, RawRunnerInterfaces, interfaces, runner_interfaces};
pub use runner::{
    Access, OfferedModels, OfferedRuns, Offers, RUNNER_FILENAME, Runner, load_runner, parse_runner,
    runner_path,
};
pub use settings_file::{SettingsFile, WithSettings};
pub use validate::{load, parse};

#[cfg(test)]
mod tests;
