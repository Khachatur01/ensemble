//! One run, as the picker set it up (`picker`): the harness that drives it,
//! the model it runs on, and how long it may take. Nothing of it is a
//! setting of its own: the harness entry and the ANN pool say it all, and a
//! run works in its task's own worktree, where the harness may edit files and
//! run commands.

use ensemble_config::{HarnessKind, Tier};

/// The tools a `builtin` run is given: every one it has.
pub const BUILTIN_TOOLS: [&str; 7] = ["read", "list", "grep", "glob", "write", "edit", "shell"];
/// Claude Code's tools for a run: it reads, edits and runs commands, and
/// `dontAsk` denies anything else instead of waiting for a person.
pub const CLAUDE_CODE_TOOLS: [&str; 6] = ["Read", "Edit", "Write", "Glob", "Grep", "Bash"];
pub const CLAUDE_CODE_PERMISSION_MODE: &str = "dontAsk";
/// OpenCode's permissions for a run; everything else is denied.
pub const OPENCODE_TOOLS: [&str; 6] = ["read", "glob", "grep", "list", "edit", "bash"];
/// Codex's own sandbox for a run: it may change files in its folder.
pub const CODEX_SANDBOX: &str = "workspace-write";

/// One run: what drives it and on which model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSpec {
    /// The harness and the model, as runs are recorded and reported:
    /// `opencode on local:qwen3.5:9b`.
    pub name: String,
    pub harness: HarnessKind,
    /// The model's tier, so a retry after a failed gate can ask for more.
    pub tier: Tier,
    /// Wall-clock limit for the run.
    pub max_seconds: u32,
    pub adapter: Adapter,
}

/// What starts a run, by its harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Adapter {
    ClaudeCode(ClaudeCodeAdapter),
    OpenCode(OpenCodeAdapter),
    Codex(CodexAdapter),
    Builtin(BuiltinAdapter),
}

/// The Claude Code CLI, run headless in the task's worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeAdapter {
    pub command: String,
    pub model: String,
    /// Claude Code's own tool names, passed to `--allowedTools`.
    pub tools: Vec<String>,
    pub permission_mode: String,
    pub max_turns: u32,
}

/// The Codex CLI, run headless in the task's worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexAdapter {
    pub command: String,
    pub model: String,
    /// `read-only`, `workspace-write` or `danger-full-access`: what Codex's own
    /// sandbox lets it do.
    pub sandbox: String,
}

/// The OpenCode CLI, run headless in the task's worktree, on an LLM of the
/// ANN pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCodeAdapter {
    pub command: String,
    /// The provider of the pool that serves its model.
    pub provider: String,
    /// The id the provider serves the model under.
    pub model: String,
    /// Tokens of context the server runs the model with, so OpenCode knows
    /// when to compact.
    pub context_window: u32,
    /// OpenCode permission names, each allowed; everything else is denied.
    /// `name(pattern)` allows only matching uses, such as `bash(git *)`.
    pub tools: Vec<String>,
    pub max_steps: u32,
}

/// Ensemble's own loop on an LLM of the ANN pool: the model asks for tools,
/// Ensemble runs them in the working directory and answers, until the model
/// is done or `max_steps` is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinAdapter {
    pub provider: String,
    pub model: String,
    /// Tokens of context the server runs the model with; the loop compacts
    /// the conversation before it outgrows them.
    pub context_window: u32,
    /// Of [`BUILTIN_TOOLS`], the ones it may use.
    pub tools: Vec<String>,
    /// Most model calls for one run.
    pub max_steps: u32,
    /// Most tokens of answer per model call.
    pub max_tokens: u32,
}

impl RunSpec {
    pub fn kind(&self) -> &'static str {
        self.harness.as_str()
    }

    /// The model, as its harness names it.
    pub fn model(&self) -> &str {
        match &self.adapter {
            Adapter::ClaudeCode(c) => &c.model,
            Adapter::OpenCode(o) => &o.model,
            Adapter::Codex(c) => &c.model,
            Adapter::Builtin(b) => &b.model,
        }
    }

    /// The provider of the pool its model is on, when it runs on one.
    pub fn provider(&self) -> Option<&str> {
        match &self.adapter {
            Adapter::ClaudeCode(_) | Adapter::Codex(_) => None,
            Adapter::OpenCode(o) => Some(&o.provider),
            Adapter::Builtin(b) => Some(&b.provider),
        }
    }

    /// Where it runs: the harness, and the provider when there is one.
    pub fn runs_on(&self) -> String {
        match self.provider() {
            Some(provider) => format!("{} on {provider}", self.kind()),
            None => self.kind().to_string(),
        }
    }

    /// Can it change files?
    pub fn can_change_files(&self) -> bool {
        match &self.adapter {
            Adapter::ClaudeCode(c) => c.tools.iter().any(|t| claude_tool_writes(t)),
            Adapter::OpenCode(o) => o.tools.iter().any(|t| opencode_tool_writes(t)),
            Adapter::Codex(c) => codex_sandbox_writes(&c.sandbox),
            Adapter::Builtin(b) => b.tools.iter().any(|t| builtin_tool_writes(t)),
        }
    }
}

/// Can a Codex sandbox change files?
pub fn codex_sandbox_writes(sandbox: &str) -> bool {
    sandbox != "read-only"
}

/// Can this `builtin` tool change files?
pub fn builtin_tool_writes(tool: &str) -> bool {
    matches!(tool, "write" | "edit" | "shell")
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
