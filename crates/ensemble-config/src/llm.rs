//! The LLM backends a hub or a runner uses: those it calls itself, and those a
//! runner serves, which it reaches through the hub (`type: runner`). A runner
//! may offer some of its own to the hub and other runners (`shares`), and says
//! how many such calls it takes at once (`max_shared_calls`).
//!
//! A name is unique among them, so whatever names a backend (the hub's
//! distiller and embedding model, a team's agents) names it alone. A process
//! reaches the backends it uses through the hub at a loopback address of its
//! own ([`RelayAt`]), where each looks like any OpenAI-compatible backend.

use super::validate::resolve_backend;
use super::*;
use serde::{Deserialize, Serialize};

/// How long a call to a backend reached through the hub may take on the
/// caller's side: the runner that serves it holds it to its own backend's
/// `timeout_seconds`, so this is only an upper bound.
pub const RELAYED_TIMEOUT_SECONDS: u64 = 3600;

/// A backend a runner serves, reached through the hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relayed {
    /// The runner that serves it, as `<owner>/<name>`.
    pub runner: String,
    /// Its name in that runner's `llm.direct`.
    pub backend: String,
}

/// Where a process serves the backends it reaches through the hub: an
/// address on loopback, `http://127.0.0.1:<port>`, and the key a call there
/// needs, made when the process starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayAt {
    pub base_url: String,
    pub key: String,
}

impl Relayed {
    /// The backend called `name`, as a process that serves it at `at` calls
    /// it: OpenAI-compatible, at `<base_url>/<name>`.
    pub fn at(&self, name: &str, at: &RelayAt) -> LlmBackend {
        LlmBackend {
            name: name.to_string(),
            timeout_seconds: RELAYED_TIMEOUT_SECONDS,
            settings: BackendSettings::OpenAiCompatible {
                base_url: format!("{}/{name}", at.base_url),
                api_key: Some(at.key.clone()),
            },
        }
    }
}

/// A hub's `llm`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubLlm {
    /// Backends the hub calls itself.
    pub direct: BTreeMap<String, LlmBackend>,
    /// Backends runners serve, which the hub reaches over their links.
    pub runners: BTreeMap<String, Relayed>,
}

/// A runner's `llm`; none of it for a computer without a runner file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunnerLlm {
    /// Backends the runner calls itself.
    pub direct: BTreeMap<String, LlmBackend>,
    /// Those of `direct` it offers through the hub, to the hub and to other
    /// runners: always OpenAI-compatible.
    pub shares: Vec<String>,
    /// The most calls of others to its shared backends at once, all of them
    /// together; the hub queues the rest for a while.
    pub max_shared_calls: u32,
    /// Backends other runners serve, which it reaches through the hub.
    pub hub: BTreeMap<String, Relayed>,
}

impl HubLlm {
    /// Every backend the hub may use, by name: its own, and, when it serves
    /// them at `relay`, those of runners.
    pub fn backends(&self, relay: Option<&RelayAt>) -> BTreeMap<String, LlmBackend> {
        with_relayed(&self.direct, &self.runners, relay)
    }
}

impl RunnerLlm {
    /// Every backend the runner's agents may use, by name: its own, and,
    /// when it serves them at `relay`, those it reaches through the hub.
    pub fn backends(&self, relay: Option<&RelayAt>) -> BTreeMap<String, LlmBackend> {
        with_relayed(&self.direct, &self.hub, relay)
    }

    /// The backends it offers through the hub, by name.
    pub fn shared(&self) -> Vec<&LlmBackend> {
        self.shares
            .iter()
            .filter_map(|name| self.direct.get(name))
            .collect()
    }
}

fn with_relayed(
    direct: &BTreeMap<String, LlmBackend>,
    relayed: &BTreeMap<String, Relayed>,
    relay: Option<&RelayAt>,
) -> BTreeMap<String, LlmBackend> {
    let mut all = direct.clone();
    if let Some(at) = relay {
        for (name, r) in relayed {
            all.insert(name.clone(), r.at(name, at));
        }
    }
    all
}

/// Each backend of `relayed` by its name, with the runner that serves it and
/// its name there: what a loopback endpoint serves.
pub fn routes(relayed: &BTreeMap<String, Relayed>) -> BTreeMap<String, (String, String)> {
    relayed
        .iter()
        .map(|(name, r)| (name.clone(), (r.runner.clone(), r.backend.clone())))
        .collect()
}

/// Is `id` a runner as a hub knows it, `<owner>/<name>`?
pub fn valid_runner_id(id: &str) -> bool {
    id.split_once('/').is_some_and(|(owner, name)| {
        !owner.is_empty() && !owner.contains(char::is_whitespace) && crate::runner::valid_name(name)
    })
}

/// One LLM backend as it is kept: a server it calls itself, or one a runner
/// serves, reached through the hub. Its key is named by the variable that
/// holds it, never written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum BackendSetting {
    #[serde(rename = "openai-compatible-api")]
    OpenAiCompatible {
        /// Where the endpoint is, as a URL: `http://127.0.0.1:11434/v1`.
        base_url: String,
        /// The variable holding the API key. None: the endpoint wants none.
        api_key_env: Option<String>,
        /// Seconds one request may take.
        timeout_seconds: u64,
    },
    #[serde(rename = "anthropic-api")]
    Anthropic {
        /// Where the API is, without `/v1`: `https://api.anthropic.com`.
        base_url: String,
        api_key_env: Option<String>,
        timeout_seconds: u64,
    },
    /// A backend a runner serves, reached through the hub.
    #[serde(rename = "runner")]
    Runner {
        /// The runner, as the hub knows it: `<owner>/<name>`.
        runner: String,
        /// Its name among that runner's backends.
        backend: String,
    },
}

impl BackendSetting {
    /// Its type, as it is written.
    pub fn type_id(&self) -> &'static str {
        match self {
            BackendSetting::OpenAiCompatible { .. } => OPENAI_COMPATIBLE_API,
            BackendSetting::Anthropic { .. } => ANTHROPIC_API,
            BackendSetting::Runner { .. } => RUNNER_BACKEND,
        }
    }

    /// The variable its key is in, if it names one.
    pub fn api_key_env(&self) -> Option<&str> {
        match self {
            BackendSetting::OpenAiCompatible { api_key_env, .. }
            | BackendSetting::Anthropic { api_key_env, .. } => api_key_env.as_deref(),
            BackendSetting::Runner { .. } => None,
        }
    }

    /// Whether it serves the OpenAI API: one of its own of that type, or one
    /// a runner serves, as a runner offers no other.
    pub fn is_openai(&self) -> bool {
        !matches!(self, BackendSetting::Anthropic { .. })
    }
}

/// The type of a backend a runner serves, reached through the hub.
pub const RUNNER_BACKEND: &str = "runner";

/// The backends of `kept`, checked and split: those called directly, with
/// their keys from `env`, and those reached through the hub.
pub(crate) fn backends(
    kept: &BTreeMap<String, BackendSetting>,
    env: &dyn Env,
    problems: &mut Vec<String>,
) -> (BTreeMap<String, LlmBackend>, BTreeMap<String, Relayed>) {
    let mut direct = BTreeMap::new();
    let mut relayed = BTreeMap::new();
    for (name, b) in kept {
        let at = format!("backends.{name}");
        if !crate::runner::valid_name(name) {
            problems.push(format!(
                "backends: the name '{name}' must be letters, digits, '-', '_' or '.'"
            ));
            continue;
        }
        match b {
            BackendSetting::Runner { runner, backend } => {
                let runner = runner.trim().to_string();
                if !valid_runner_id(&runner) {
                    problems.push(format!(
                        "{at}.runner: '{runner}' must be a runner as the hub knows it, <owner>/<name>, such as ana/gpu-box"
                    ));
                }
                let backend = backend.trim().to_string();
                if !crate::runner::valid_name(&backend) {
                    problems.push(format!(
                        "{at}.backend: '{backend}' must be the name of one of that runner's backends"
                    ));
                }
                relayed.insert(name.clone(), Relayed { runner, backend });
            }
            _ => match resolve_backend(&at, name, b, env) {
                Ok(b) => {
                    direct.insert(name.clone(), b);
                }
                Err(mut errs) => problems.append(&mut errs),
            },
        }
    }
    (direct, relayed)
}

/// Every backend name kept with its type: one reached through the hub is
/// OpenAI-compatible, as a runner offers no other.
pub(crate) fn kept_kinds(
    kept: &BTreeMap<String, BackendSetting>,
) -> BTreeMap<String, &'static str> {
    kept.iter()
        .map(|(name, b)| {
            let kind = match b {
                BackendSetting::Anthropic { .. } => ANTHROPIC_API,
                _ => OPENAI_COMPATIBLE_API,
            };
            (name.clone(), kind)
        })
        .collect()
}

/// Every name of resolved backends, with its type: one reached through the
/// hub is OpenAI-compatible, as a runner offers no other.
pub(crate) fn kinds(
    direct: &BTreeMap<String, LlmBackend>,
    relayed: &BTreeMap<String, Relayed>,
) -> BTreeMap<String, &'static str> {
    direct
        .iter()
        .map(|(name, b)| (name.clone(), b.type_id()))
        .chain(
            relayed
                .keys()
                .map(|name| (name.clone(), OPENAI_COMPATIBLE_API)),
        )
        .collect()
}
