//! The config document as written: serde structs that refuse unknown keys.

use super::{Isolation, SystemPrompt, WhenNone};
use serde::Deserialize;
use std::collections::BTreeMap;

/// `deny_unknown_fields` throughout: a typo'd key is a key that does nothing,
/// and an agent silently running with settings nobody chose is the failure mode
/// this whole module exists to prevent. `reviewr:` should be an error, not a
/// no-op. A key that is missing is as much an error as one that is unknown.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawConfig {
    pub(crate) version: u32,
    /// Named limits that agents share, such as one model server's concurrency
    /// or one subscription's budget. `{}` when there are none.
    pub(crate) limits: BTreeMap<String, RawLimit>,
    /// Commands a task's work must pass. Defined here, by a person, so a task
    /// names a gate and cannot bring a command of its own.
    pub(crate) gates: BTreeMap<String, RawGate>,
    /// The helper team: its agents, by name.
    pub(crate) agents: BTreeMap<String, RawAgent>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawGate {
    /// A shell command run in the project; exit status 0 passes.
    pub(crate) command: String,
    /// Seconds it may run before it is stopped and counted as failed.
    pub(crate) timeout_seconds: u32,
}

/// A model endpoint. `type` decides which settings it has.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum RawLlmBackend {
    #[serde(rename = "openai-compatible-api")]
    OpenAiCompatibleApi(RawOpenAiBackend),
    #[serde(rename = "anthropic-api")]
    AnthropicApi(RawAnthropicBackend),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAnthropicBackend {
    /// Where the API is, without `/v1`: `https://api.anthropic.com`.
    pub(crate) base_url: String,
    /// The name of the variable holding the API key.
    pub(crate) api_key_env: Option<String>,
    /// Seconds one request may take.
    pub(crate) timeout_seconds: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawOpenAiBackend {
    /// Where the endpoint is, as a URL: `http://127.0.0.1:11434/v1`.
    pub(crate) base_url: String,
    /// The *name* of the variable holding the API key. Absent: the endpoint
    /// wants none.
    pub(crate) api_key_env: Option<String>,
    /// Seconds one request may take.
    pub(crate) timeout_seconds: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAgent {
    /// What the agent is for. Its first line is what the calling agent sees
    /// when it lists the team, and the whole of it goes into the agent's
    /// system prompt. Absent: a CLI agent runs on its CLI's own system prompt
    /// alone, and a `direct` agent has none.
    pub(crate) prompt: Option<String>,
    /// Wall-clock cap on one delegation, in seconds. A run that reaches it
    /// stops and reports what it did, so a hung model cannot hold the caller
    /// forever.
    pub(crate) max_seconds: u32,
    /// Most delegations to this agent that run at once in a session; more
    /// wait for a free slot. 1 for a model server that can serve one request
    /// at a time.
    /// Written with `budget`, or not at all when `limit` names a shared one.
    pub(crate) max_concurrent: Option<u32>,
    /// Where its tasks run: in the project itself, or each in a git worktree
    /// of its own.
    pub(crate) isolation: Isolation,
    /// How much it may run in an hour. Written with `max_concurrent`, or not
    /// at all when `limit` names a shared one.
    pub(crate) budget: Option<RawBudget>,
    /// The name of one of the team's `limits`, which it then shares with the
    /// other agents that name it, in place of `max_concurrent` and `budget`.
    pub(crate) limit: Option<String>,
    /// What runs the agent, and how.
    pub(crate) adapter: RawAdapter,
}

/// How much an agent may run. `type` decides which settings it has.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum RawBudget {
    #[serde(rename = "unlimited")]
    Unlimited(RawUnlimited),
    #[serde(rename = "per_hour")]
    PerHour(RawPerHour),
    #[serde(rename = "per_hour_tokens")]
    PerHourTokens(RawPerHourTokens),
}

/// A limit agents share.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawLimit {
    /// Most runs at once across every agent that shares it.
    pub(crate) max_concurrent: u32,
    /// How much all of them together may run in an hour.
    pub(crate) budget: RawBudget,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPerHourTokens {
    /// Tokens, prompt and completion together, that runs may use in any hour.
    /// Counted from what each run reports; one that reports none counts none.
    pub(crate) tokens: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawUnlimited {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPerHour {
    /// Runs it may start in any hour: tasks, each gate retry, and
    /// delegations, blocking or in the background.
    pub(crate) delegations: u32,
}

/// The harness behind an agent. `type` decides which settings it has.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum RawAdapter {
    #[serde(rename = "claude-code")]
    ClaudeCode(RawClaudeCodeAdapter),
    #[serde(rename = "opencode")]
    OpenCode(RawOpenCodeAdapter),
    #[serde(rename = "codex")]
    Codex(RawCodexAdapter),
    #[serde(rename = "direct")]
    Direct(RawDirectAdapter),
    #[serde(rename = "builtin")]
    Builtin(RawBuiltinAdapter),
}

/// A light agent: the task, and the agent's prompt as the system prompt,
/// sent to an LLM backend as one chat request, with no CLI in between. It has
/// no tools, so it reads and changes nothing; it answers from what it is given.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawDirectAdapter {
    /// The models it may run on, in the order they are tried.
    #[serde(rename = "try")]
    pub(crate) candidates: Vec<RawDirectCandidate>,
    /// What happens when none of them can be used.
    pub(crate) when_none: WhenNone,
    /// Most tokens of answer.
    pub(crate) max_tokens: u32,
}

/// An agent loop in Ensemble itself, on an LLM backend, with tools of its own.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawBuiltinAdapter {
    /// The models it may run on, in the order they are tried.
    #[serde(rename = "try")]
    pub(crate) candidates: Vec<RawBuiltinCandidate>,
    /// What happens when none of them can be used.
    pub(crate) when_none: WhenNone,
    /// What it may do: `read`, `list`, `grep`, `glob`, `write`, `edit`, `shell`.
    pub(crate) tools: Vec<String>,
    /// Most model calls for one delegation.
    pub(crate) max_steps: u32,
    /// Most tokens of answer per model call.
    pub(crate) max_tokens: u32,
}

/// One model a `builtin` agent may run on.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawBuiltinCandidate {
    /// Which of the runner's LLM backends serves the model.
    pub(crate) backend: String,
    /// The id the endpoint serves the model under.
    pub(crate) model: String,
    /// Tokens of context the server runs the model with: the loop compacts
    /// the conversation before it outgrows them.
    pub(crate) context_window: u32,
}
/// One model a `direct` agent may run on.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawDirectCandidate {
    /// Which LLM backend serves the model: a name of the runner's, or of the
    /// hub's for its distiller.
    pub(crate) backend: String,
    /// The id the endpoint serves the model under.
    pub(crate) model: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawClaudeCodeAdapter {
    /// As Claude Code names it: an alias or a full model id.
    pub(crate) model: String,
    /// What the agent may do, in Claude Code's own tool names (`Read`,
    /// `Bash(git:*)`).
    pub(crate) tools: Vec<String>,
    /// Claude Code's `--permission-mode`. `dontAsk` denies what `tools` does
    /// not allow instead of waiting for a person who is not there.
    pub(crate) permission_mode: String,
    /// Most agent turns Claude Code may take for one delegation.
    pub(crate) max_turns: u32,
    /// The CLI: `claude` if it is on PATH.
    pub(crate) command: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawOpenCodeAdapter {
    /// Where the model comes from.
    pub(crate) model: RawOpenCodeModel,
    /// What the agent may do, in OpenCode's permissions (`read`,
    /// `bash(git *)`).
    pub(crate) tools: Vec<String>,
    /// What the agent's `prompt` does to OpenCode's own system prompt: adds
    /// to it, or takes its place.
    pub(crate) system_prompt: SystemPrompt,
    /// Cap on steps within one delegation.
    pub(crate) max_steps: u32,
    /// The CLI: `opencode` if it is on PATH.
    pub(crate) command: String,
}

/// The model of an OpenCode agent: one OpenCode is already set up for, or one
/// served by an LLM backend of this config.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum RawOpenCodeModel {
    #[serde(rename = "provider")]
    Provider(RawProviderModel),
    #[serde(rename = "llm_backend")]
    LlmBackend(RawBackendModel),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawProviderModel {
    /// `provider/model`, as `opencode models` lists it.
    pub(crate) name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawBackendModel {
    /// The models it may run on, in the order they are tried.
    #[serde(rename = "try")]
    pub(crate) candidates: Vec<RawBackendCandidate>,
    /// What happens when none of them can be used.
    pub(crate) when_none: WhenNone,
}

/// One model an OpenCode agent may run on.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawBackendCandidate {
    /// Which of the runner's LLM backends serves the model. Ensemble gives
    /// OpenCode the endpoint and key for the run.
    pub(crate) backend: String,
    /// The id the endpoint serves the model under, as its /models lists it.
    pub(crate) name: String,
    /// Tokens of context the server really runs the model with. OpenCode
    /// needs it to know when to compact the conversation.
    pub(crate) context_window: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawCodexAdapter {
    /// The program to run: `codex`, or a path to it.
    pub(crate) command: String,
    /// The model Codex runs on, as `codex exec --model` takes it.
    pub(crate) model: String,
    /// `read-only`, `workspace-write` or `danger-full-access`.
    pub(crate) sandbox: String,
}
