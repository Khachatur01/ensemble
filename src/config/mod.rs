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

pub const CONFIG_FILENAME: &str = "config.yaml";

/// The JSON Schema for `config.yaml`, for editors. `ensemble schema`
/// prints it; the tests keep it in step with the structs below.
pub const SCHEMA: &str = include_str!("../../ensemble.schema.json");

/// The config `ensemble init` writes: every setting at least once, with what
/// it does. The tests keep it loading and complete.
pub const SAMPLE: &str = include_str!("../sample.yaml");

/// The first line of a config, which points an editor at the JSON Schema.
pub const SCHEMA_URL_LINE: &str = "# yaml-language-server: $schema=https://raw.githubusercontent.com/Khachatur01/ensemble/master/ensemble.schema.json";
pub const SUPPORTED_VERSION: u32 = 2;

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
pub const LLM_AGENT: &str = "llm";

/// Where an OpenCode agent's model comes from.
pub const PROVIDER_MODEL: &str = "provider";
pub const LLM_BACKEND_MODEL: &str = "llm_backend";

/// The type of Ensemble's former built-in harness, which no longer exists.
/// Named only to tell someone whose config still uses it where to go.
const REMOVED_ENSEMBLE_AGENT: &str = "ensemble";

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

/// One member of the team, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    /// `team/agent`, the name the manager delegates by.
    pub name: String,
    /// The team it belongs to.
    pub team: String,
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

/// Where an agent's tasks run. Delegations, blocking or in the background,
/// always run in the project: their caller is waiting to see the change.
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
    Llm(LlmAdapter),
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

/// One chat request to an LLM backend: a light agent with no tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmAdapter {
    pub backend: String,
    pub model: String,
    pub max_tokens: u32,
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
    /// The name without its team.
    pub fn short_name(&self) -> &str {
        self.name.split_once('/').map_or(&self.name, |(_, n)| n)
    }

    pub fn kind(&self) -> &'static str {
        match self.adapter {
            Adapter::ClaudeCode(_) => CLAUDE_CODE_AGENT,
            Adapter::OpenCode(_) => OPENCODE_AGENT,
            Adapter::Codex(_) => CODEX_AGENT,
            Adapter::Llm(_) => LLM_AGENT,
        }
    }

    pub fn model(&self) -> &str {
        match &self.adapter {
            Adapter::ClaudeCode(c) => &c.model,
            Adapter::OpenCode(o) => o.model_id(),
            Adapter::Codex(c) => &c.model,
            Adapter::Llm(l) => &l.model,
        }
    }

    /// The LLM backend its model runs on, when it runs on one.
    pub fn llm_backend(&self) -> Option<&str> {
        match &self.adapter {
            Adapter::ClaudeCode(_) | Adapter::Codex(_) => None,
            Adapter::OpenCode(o) => o.llm_backend(),
            Adapter::Llm(l) => Some(&l.backend),
        }
    }

    /// Tokens of context its model's server runs it with, when it runs on an
    /// LLM backend.
    pub fn context_window(&self) -> Option<u32> {
        match &self.adapter {
            Adapter::ClaudeCode(_) | Adapter::Codex(_) | Adapter::Llm(_) => None,
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
            Adapter::Codex(c) => codex_sandbox_writes(&c.sandbox),
            Adapter::Llm(_) => false,
        }
    }

    /// Its tools, by the names its adapter uses.
    pub fn tool_names(&self) -> Vec<&str> {
        match &self.adapter {
            Adapter::ClaudeCode(c) => c.tools.iter().map(String::as_str).collect(),
            Adapter::OpenCode(o) => o.tools.iter().map(String::as_str).collect(),
            // Its sandbox is what stands in for a list of tools.
            Adapter::Codex(c) => vec![c.sandbox.as_str()],
            Adapter::Llm(_) => Vec::new(),
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
    /// The organization's teams, by name. There is no manager in the file: the
    /// agent that calls Ensemble is it.
    pub teams: BTreeMap<String, Team>,
    /// Every agent of every team, by its qualified name, `team/agent`.
    pub agents: BTreeMap<String, AgentConfig>,
    /// Every team's gates, by qualified name, `team/gate`.
    pub gates: BTreeMap<String, Gate>,
    /// Where it was loaded from, for error messages.
    pub source: PathBuf,
}

/// A team of the organization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Team {
    pub name: String,
    /// What the team is for; the first line is what the manager sees.
    pub prompt: String,
    pub tracker: Tracker,
    /// Its agents, by qualified name.
    pub agents: Vec<String>,
}

impl Team {
    /// The first line of its prompt.
    pub fn summary(&self) -> &str {
        self.prompt.lines().next().unwrap_or("").trim()
    }
}

/// Where a team's tasks come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tracker {
    /// Markdown files in Ensemble's state directory.
    LocalMarkdown,
}

/// The name an agent is looked up by was not enough to tell which one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameError {
    Unknown {
        name: String,
        known: Vec<String>,
    },
    Ambiguous {
        name: String,
        candidates: Vec<String>,
    },
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NameError::Unknown { name, known } => write!(
                f,
                "no agent '{name}'. Agents: {}.",
                if known.is_empty() {
                    "(none)".to_string()
                } else {
                    known.join(", ")
                }
            ),
            NameError::Ambiguous { name, candidates } => write!(
                f,
                "'{name}' is the name of more than one agent: {}. Use the full name, team/agent.",
                candidates.join(", ")
            ),
        }
    }
}

/// A command a task's work must pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    pub command: String,
    pub timeout_seconds: u32,
}

impl Config {
    /// The agent called `name`: `team/agent`, or `agent` when only one team has
    /// an agent of that name.
    pub fn agent(&self, name: &str) -> Option<&AgentConfig> {
        self.qualify(name).ok().and_then(|q| self.agents.get(&q))
    }

    /// `name` as `team/agent`, or why it names no single agent.
    pub fn qualify(&self, name: &str) -> Result<String, NameError> {
        if self.agents.contains_key(name) {
            return Ok(name.to_string());
        }
        let candidates: Vec<String> = self
            .agents
            .values()
            .filter(|a| a.short_name() == name)
            .map(|a| a.name.clone())
            .collect();
        match candidates.len() {
            1 => Ok(candidates.into_iter().next().expect("one candidate")),
            0 => Err(NameError::Unknown {
                name: name.to_string(),
                known: self.agent_names().into_iter().map(String::from).collect(),
            }),
            _ => Err(NameError::Ambiguous {
                name: name.to_string(),
                candidates,
            }),
        }
    }

    /// The agents' qualified names.
    pub fn agent_names(&self) -> Vec<&str> {
        self.agents.keys().map(String::as_str).collect()
    }

    /// Keep only the teams in `only`, with their agents and gates: a user's
    /// choice of which teams to see, not a change to the organization. A name
    /// that is no team is an error that lists the teams.
    pub fn retain_teams(&mut self, only: &[String]) -> Result<(), String> {
        if let Some(unknown) = only.iter().find(|t| !self.teams.contains_key(t.as_str())) {
            let known: Vec<&str> = self.teams.keys().map(String::as_str).collect();
            return Err(format!("no team '{unknown}'. Teams: {}.", known.join(", ")));
        }
        self.teams.retain(|name, _| only.contains(name));
        self.agents.retain(|_, a| only.contains(&a.team));
        self.gates.retain(|key, _| {
            key.split_once('/')
                .is_some_and(|(team, _)| only.contains(&team.to_string()))
        });
        Ok(())
    }

    /// The gate `name` of `team`.
    pub fn gate(&self, team: &str, name: &str) -> Option<&Gate> {
        self.gates.get(&format!("{team}/{name}"))
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
                "no {CONFIG_FILENAME} at {}. Check the --org directory, or make one with `ensemble init --org <dir>`.",
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
pub mod migrate;
mod raw;
mod validate;

pub use env::*;
pub use validate::{load, parse};

#[cfg(test)]
mod tests;
