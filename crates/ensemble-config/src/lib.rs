//! The settings of a hub and a runner: kept in each one's database as typed
//! rows (`kept`), changed only through operations, and resolved here into
//! what the process runs with, checked.
//!
//! Validation is deliberately loud and early. Every check here corresponds to a
//! failure that would otherwise surface much later and much less clearly — as a
//! 404 on the first request to a model, as a run on settings nobody chose, or
//! as an empty base URL that turns into a connection refused.
//!
//! There are no default values. Every setting is written, and a missing one is
//! refused by name. What differs between cases is a tagged enum (`type:`), and
//! each variant carries exactly its own required settings, so a setting that
//! does not apply cannot be written and one that does cannot be left out. A
//! few settings are optional because their absence means "not used" (no API
//! key), which is not a value anyone chose.
//!
//! Secrets are never settings. An API key is named by the *variable* that
//! holds it (`api_key_env`), and the value is read from the environment, or
//! from the keys the hub or the runner keeps, sealed in its database.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// The provider types that ship. Adding one is adding an adapter and a
/// variant.
pub const OPENAI_COMPATIBLE_API: &str = "openai-compatible-api";
/// Anthropic's own Messages API.
pub const ANTHROPIC_API: &str = "anthropic-api";

/// The harnesses, by the names their runs are recorded under.
pub const CLAUDE_CODE_AGENT: &str = "claude-code";
pub const OPENCODE_AGENT: &str = "opencode";
pub const CODEX_AGENT: &str = "codex";
pub const BUILTIN_AGENT: &str = "builtin";

// ---------------------------------------------------------------------------
// Providers and model lists, resolved
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

/// A provider of the ANN pool the process calls itself, resolved: where it is
/// and its key. It is named for the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmBackend {
    pub name: String,
    pub timeout_seconds: u64,
    pub settings: BackendSettings,
}

impl LlmBackend {
    /// An OpenAI-compatible provider called `name` at `base_url`, without a
    /// key.
    pub fn openai(name: &str, base_url: &str, timeout_seconds: u64) -> LlmBackend {
        LlmBackend {
            name: name.to_string(),
            timeout_seconds,
            settings: BackendSettings::OpenAiCompatible {
                base_url: base_url.trim_end_matches('/').to_string(),
                api_key: None,
            },
        }
    }

    pub fn type_id(&self) -> &'static str {
        match self.settings {
            BackendSettings::OpenAiCompatible { .. } => OPENAI_COMPATIBLE_API,
            BackendSettings::Anthropic { .. } => ANTHROPIC_API,
        }
    }
}

/// Models tried in order, one chat request at a time, with no CLI and no
/// tools: what the hub's knowledge distills with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectAdapter {
    /// The models it may run on, in the order they are tried.
    pub fallback: Fallback,
    pub max_tokens: u32,
}

impl DirectAdapter {
    /// The provider of its first model.
    pub fn provider(&self) -> &str {
        self.fallback.first().map_or("", |c| c.provider.as_str())
    }

    /// The id its first model is served under.
    pub fn model(&self) -> &str {
        self.fallback.first().map_or("", |c| c.model.as_str())
    }
}

/// What happens when none of the models in a `try` list can be used: no
/// provider answers, or none serves the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WhenNone {
    /// What asked fails, saying what each provider answered.
    Fail,
    /// What asked waits and is tried again later.
    Wait,
}

/// One model of a `try` list: the provider that serves it, and its id there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub provider: String,
    /// The id the endpoint serves the model under.
    pub model: String,
}

/// The models of a `try` list, in the order they are tried. The first one
/// that is reachable and serves the model is the one a call uses.
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
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Settings that do not resolve: every problem, each said with where it is.
/// Collected rather than returned one at a time: fixing settings five round
/// trips in a row is miserable, and every check here is independent of the
/// others.
#[derive(Debug)]
pub enum ConfigError {
    Invalid {
        path: PathBuf,
        problems: Vec<String>,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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

pub mod ann;
mod env;
mod harness;
pub mod hub;
pub mod kept;
pub mod runner;

pub use ann::{
    AnnPool, AnnPoolSetting, ModelKind, PoolModel, ProviderSetting, RelayAt, Tier, split_name,
};
pub use env::*;
pub use harness::{
    BuiltinHarness, CliHarness, Harness, HarnessChange, HarnessKind, HarnessModel, HarnessesSetting,
};
pub use hub::{
    AccountRules, Accounts, Connectors, Embed, EmbedSetting, Hub, HubSettings, IssueTracker,
    Knowledge, ModelSetting, ModelsSetting, Permission, Recall, Registration, RegistrationSetting,
    SourceFilter, SourceFilterSetting, Sources, Tasks, Tracker, TrackerSetting,
};
pub use runner::{Runner, RunnerSettings};

#[cfg(test)]
mod tests;
