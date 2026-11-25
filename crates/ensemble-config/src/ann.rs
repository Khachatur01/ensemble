//! The ANN pool of a hub or a runner: the artificial neural networks it may
//! call, in two layers. A **provider** is where models come from: a server it
//! calls itself, or another runner, whose shared models it reaches through
//! the hub (`type: runner`). A **model** is what a provider serves, an LLM or
//! an embedding model, that a person added, as the provider said it is:
//! nothing a provider serves is in the pool until a person adds it, and a
//! runner may share what is through the hub.
//!
//! A provider's name is unique in the pool, so whatever names a model (the
//! hub's distiller and embedding model) names it as
//! `<provider>:<id>`. A process reaches the providers it uses through the hub at
//! a loopback address of its own ([`RelayAt`]), where each looks like any
//! OpenAI-compatible server; a model of one is named there as the runner
//! that shares it names it, `<its provider>:<id>`.

use super::*;
use serde::{Deserialize, Serialize};

/// How long a call to a provider reached through the hub may take on the
/// caller's side: the runner that serves it holds it to its own provider's
/// `timeout_seconds`, so this is only an upper bound.
pub const RELAYED_TIMEOUT_SECONDS: u64 = 3600;

/// The type of a provider that is another runner, reached through the hub.
pub const RUNNER_PROVIDER: &str = "runner";

/// Where a process serves the providers it reaches through the hub: an
/// address on loopback, `http://127.0.0.1:<port>`, and the key a call there
/// needs, made when the process starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayAt {
    pub base_url: String,
    pub key: String,
}

/// One provider as it is kept: a server it calls itself, or another runner,
/// reached through the hub. Its key is named by the variable that holds it,
/// never written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum ProviderSetting {
    #[serde(rename = "openai-compatible-api")]
    OpenAiCompatible {
        /// Where the endpoint is, as a URL: `http://127.0.0.1:11434/v1`.
        base_url: String,
        /// The variable holding the API key. None: the endpoint wants none.
        api_key_env: Option<String>,
        /// Seconds one request may take.
        timeout_seconds: u64,
        /// The most calls to it at once, its own and those shared through
        /// the hub together.
        max_concurrent: u32,
    },
    #[serde(rename = "anthropic-api")]
    Anthropic {
        /// Where the API is, without `/v1`: `https://api.anthropic.com`.
        base_url: String,
        api_key_env: Option<String>,
        timeout_seconds: u64,
        max_concurrent: u32,
    },
    /// Another runner, whose shared models are reached through the hub.
    #[serde(rename = "runner")]
    Runner {
        /// The runner, as the hub knows it: `<owner>/<name>`.
        runner: String,
    },
}

impl ProviderSetting {
    /// Its type, as it is written.
    pub fn type_id(&self) -> &'static str {
        match self {
            ProviderSetting::OpenAiCompatible { .. } => OPENAI_COMPATIBLE_API,
            ProviderSetting::Anthropic { .. } => ANTHROPIC_API,
            ProviderSetting::Runner { .. } => RUNNER_PROVIDER,
        }
    }

    /// The variable its key is in, if it names one.
    pub fn api_key_env(&self) -> Option<&str> {
        match self {
            ProviderSetting::OpenAiCompatible { api_key_env, .. }
            | ProviderSetting::Anthropic { api_key_env, .. } => api_key_env.as_deref(),
            ProviderSetting::Runner { .. } => None,
        }
    }

    /// Whether it serves the OpenAI API: a server of that type, or a runner,
    /// as a runner shares no other.
    pub fn is_openai(&self) -> bool {
        !matches!(self, ProviderSetting::Anthropic { .. })
    }
}

/// How strong an LLM is, for choosing one that is enough.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Small,
    Medium,
    Strong,
}

impl Tier {
    pub const NAMES: &'static [&'static str] = &["small", "medium", "strong"];

    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Small => "small",
            Tier::Medium => "medium",
            Tier::Strong => "strong",
        }
    }

    pub fn parse(text: &str) -> Option<Tier> {
        match text.trim() {
            "small" => Some(Tier::Small),
            "medium" => Some(Tier::Medium),
            "strong" => Some(Tier::Strong),
            _ => None,
        }
    }
}

/// What a model is, as far as its provider said.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelKind {
    /// Messages in, text out.
    Llm {
        tier: Tier,
        /// Tokens of context it runs with; None: its provider did not say.
        context_window: Option<u32>,
        /// Whether it makes tool calls; None: its provider did not say.
        tools: Option<bool>,
    },
    /// Text in, a vector out.
    Embedding { dimensions: u32 },
}

impl ModelKind {
    /// Its type, as it is written: `unknown`, `llm` or `embedding`.
    pub fn type_id(&self) -> &'static str {
        match self {
            ModelKind::Llm { .. } => "llm",
            ModelKind::Embedding { .. } => "embedding",
        }
    }

    /// It, in a few words: `llm, strong, 32768 tokens, tools`.
    pub fn describe(&self) -> String {
        match self {
            ModelKind::Llm {
                tier,
                context_window,
                tools,
            } => {
                let mut said = format!("llm, {}", tier.as_str());
                if let Some(n) = context_window {
                    said.push_str(&format!(", {n} tokens"));
                }
                match tools {
                    Some(true) => said.push_str(", tools"),
                    Some(false) => said.push_str(", no tools"),
                    None => {}
                }
                said
            }
            ModelKind::Embedding { dimensions } => format!("embedding, {dimensions} dimensions"),
        }
    }
}

/// One model of a provider, as it is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolModel {
    /// The provider that serves it.
    pub provider: String,
    /// Its id there, as the provider lists it.
    pub id: String,
    pub kind: ModelKind,
    /// The provider served it when it was added, and does not list it now.
    pub missing: bool,
    /// Offered through the hub, to the hub and its owner's other runners: a
    /// runner's alone.
    pub shared: bool,
}

impl PoolModel {
    /// How it is named: `<provider>:<id>`.
    pub fn name(&self) -> String {
        format!("{}:{}", self.provider, self.id)
    }

    /// Whether it may be used as an LLM.
    pub fn usable_llm(&self) -> bool {
        matches!(self.kind, ModelKind::Llm { .. })
    }

    /// Whether it may be used as an embedding model.
    pub fn usable_embedding(&self) -> bool {
        matches!(self.kind, ModelKind::Embedding { .. })
    }
}

/// The ANN pool as it is kept: its providers by name, and the models they
/// serve.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnPoolSetting {
    pub providers: BTreeMap<String, ProviderSetting>,
    pub models: Vec<PoolModel>,
}

impl AnnPoolSetting {
    /// The model `id` of `provider`.
    pub fn model(&self, provider: &str, id: &str) -> Option<&PoolModel> {
        self.models
            .iter()
            .find(|m| m.provider == provider && m.id == id)
    }

    pub fn model_mut(&mut self, provider: &str, id: &str) -> Option<&mut PoolModel> {
        self.models
            .iter_mut()
            .find(|m| m.provider == provider && m.id == id)
    }

    /// Add the model `id` of `provider` as `kind`, or keep it as `kind` when
    /// it is there, among the models in the order of their providers and ids.
    pub fn add(&mut self, provider: &str, id: &str, kind: ModelKind) {
        match self.model_mut(provider, id) {
            Some(m) => m.kind = kind,
            None => self.models.push(PoolModel {
                provider: provider.to_string(),
                id: id.to_string(),
                kind,
                missing: false,
                shared: false,
            }),
        }
        self.models
            .sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));
    }
}

/// A model `<provider>:<id>` split: the provider's name holds no `:`, the id may.
pub fn split_name(name: &str) -> Option<(&str, &str)> {
    name.trim()
        .split_once(':')
        .filter(|(provider, id)| !provider.is_empty() && !id.is_empty())
}

/// The ANN pool of a hub or a runner, resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnnPool {
    /// The providers it calls itself, by name, with their keys.
    pub direct: BTreeMap<String, LlmBackend>,
    /// The most calls at once to each of `direct`.
    pub max_concurrent: BTreeMap<String, u32>,
    /// The providers that are other runners, by name: each runner as
    /// `<owner>/<name>`.
    pub relayed: BTreeMap<String, String>,
    pub models: Vec<PoolModel>,
}

/// One provider a runner shares, as it offers it through the hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shared<'a> {
    pub provider: &'a LlmBackend,
    pub max_concurrent: u32,
    pub models: Vec<&'a PoolModel>,
}

impl AnnPool {
    /// Every provider it may call, by name: its own, and, when it serves them
    /// at `relay`, the runners it reaches through the hub.
    pub fn backends(&self, relay: Option<&RelayAt>) -> BTreeMap<String, LlmBackend> {
        let mut all = self.direct.clone();
        if let Some(at) = relay {
            for name in self.relayed.keys() {
                all.insert(
                    name.clone(),
                    LlmBackend {
                        name: name.clone(),
                        timeout_seconds: RELAYED_TIMEOUT_SECONDS,
                        settings: BackendSettings::OpenAiCompatible {
                            base_url: format!("{}/{name}", at.base_url),
                            api_key: Some(at.key.clone()),
                        },
                    },
                );
            }
        }
        all
    }

    /// The model `id` of `provider`.
    pub fn model(&self, provider: &str, id: &str) -> Option<&PoolModel> {
        self.models
            .iter()
            .find(|m| m.provider == provider && m.id == id)
    }

    /// The providers it shares through the hub, each with the models of it
    /// shared.
    pub fn shared(&self) -> Vec<Shared<'_>> {
        self.direct
            .iter()
            .filter_map(|(name, provider)| {
                let models: Vec<&PoolModel> = self
                    .models
                    .iter()
                    .filter(|m| m.shared && m.provider == *name)
                    .collect();
                (!models.is_empty()).then(|| Shared {
                    provider,
                    max_concurrent: self.max_concurrent.get(name).copied().unwrap_or(1),
                    models,
                })
            })
            .collect()
    }

    /// Each runner it reaches through the hub, by the provider's name: what a
    /// loopback endpoint serves.
    pub fn routes(&self) -> BTreeMap<String, String> {
        self.relayed.clone()
    }
}

/// Is `id` a runner as a hub knows it, `<owner>/<name>`?
pub fn valid_runner_id(id: &str) -> bool {
    id.split_once('/').is_some_and(|(owner, name)| {
        !owner.is_empty() && !owner.contains(char::is_whitespace) && crate::runner::valid_name(name)
    })
}

/// The pool of `kept`, checked, with the keys of its providers from `env`. Only
/// a runner's models are shared, when `may_share`.
pub(crate) fn pool(
    kept: &AnnPoolSetting,
    may_share: bool,
    env: &dyn Env,
    problems: &mut Vec<String>,
) -> AnnPool {
    let mut out = AnnPool::default();
    for (name, s) in &kept.providers {
        let at = format!("ann_pool.providers.{name}");
        if !crate::runner::valid_name(name) {
            problems.push(format!(
                "ann_pool.providers: the name '{name}' must be letters, digits, '-', '_' or '.'"
            ));
            continue;
        }
        match s {
            ProviderSetting::Runner { runner } => {
                let runner = runner.trim().to_string();
                if !valid_runner_id(&runner) {
                    problems.push(format!(
                        "{at}.runner: '{runner}' must be a runner as the hub knows it, <owner>/<name>, such as ana/gpu-box"
                    ));
                }
                out.relayed.insert(name.clone(), runner);
            }
            ProviderSetting::OpenAiCompatible { max_concurrent, .. }
            | ProviderSetting::Anthropic { max_concurrent, .. } => {
                if *max_concurrent == 0 {
                    problems.push(format!("{at}: `max_concurrent` must be greater than 0"));
                }
                match resolve_provider(&at, name, s, env) {
                    Ok(b) => {
                        out.direct.insert(name.clone(), b);
                        out.max_concurrent.insert(name.clone(), *max_concurrent);
                    }
                    Err(mut errs) => problems.append(&mut errs),
                }
            }
        }
    }
    let mut seen = Vec::new();
    for m in &kept.models {
        let at = format!("ann_pool.models.{}", m.name());
        let Some(provider) = kept.providers.get(&m.provider) else {
            problems.push(format!("{at}: there is no provider '{}'", m.provider));
            continue;
        };
        if m.id.trim().is_empty() {
            problems.push(format!(
                "ann_pool.models: a model of {} has no id",
                m.provider
            ));
        }
        if seen.contains(&(&m.provider, &m.id)) {
            problems.push(format!("{at}: is listed twice"));
        }
        seen.push((&m.provider, &m.id));
        if let ModelKind::Embedding { dimensions: 0 } = m.kind {
            problems.push(format!("{at}: `dimensions` must be greater than 0"));
        }
        if let ModelKind::Llm {
            context_window: Some(0),
            ..
        } = m.kind
        {
            problems.push(format!("{at}: `context_window` must be greater than 0"));
        }
        if m.shared {
            if !may_share {
                problems.push(format!("{at}: a hub shares no model"));
            } else if !matches!(provider, ProviderSetting::OpenAiCompatible { .. }) {
                problems.push(format!(
                    "{at}: is shared, and only a model of an {OPENAI_COMPATIBLE_API} provider of the runner's own may be"
                ));
            }
        }
    }
    out.models = kept.models.clone();
    out
}

/// What a model named somewhere is used as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    Llm,
    Embedding,
}

/// What a pool is checked against: each provider's name, and the models.
pub(crate) struct Known<'a> {
    pub(crate) providers: Vec<&'a str>,
    pub(crate) models: &'a [PoolModel],
}

impl AnnPoolSetting {
    pub(crate) fn known(&self) -> Known<'_> {
        Known {
            providers: self.providers.keys().map(String::as_str).collect(),
            models: &self.models,
        }
    }
}

/// Check that the model `id` of `provider`, named at `at`, is one of `pool`'s,
/// and of `use_`.
pub(crate) fn check_model(
    at: &str,
    provider: &str,
    id: &str,
    use_: Use,
    pool: &Known<'_>,
    problems: &mut Vec<String>,
) {
    if !pool.providers.contains(&provider) {
        let known = &pool.providers;
        problems.push(format!(
            "{at}: there is no provider '{provider}' in the ANN pool. Its providers: {}.",
            if known.is_empty() {
                "(none)".to_string()
            } else {
                known.join(", ")
            }
        ));
        return;
    }
    let name = format!("{provider}:{id}");
    let Some(model) = pool
        .models
        .iter()
        .find(|m| m.provider == provider && m.id == id)
    else {
        problems.push(format!(
            "{at}: {name} is not a model of the ANN pool; `model add {name}` adds it"
        ));
        return;
    };
    let fits = match use_ {
        Use::Llm => model.usable_llm(),
        Use::Embedding => model.usable_embedding(),
    };
    if !fits {
        let wanted = match use_ {
            Use::Llm => "an LLM",
            Use::Embedding => "an embedding model",
        };
        problems.push(format!(
            "{at}: {name} is not {wanted}: it is {}",
            model.kind.describe()
        ));
    }
}
/// A provider the process calls itself, checked, with its key from `env`: a
/// URL, a timeout and an optional key. Not another runner.
pub(crate) fn resolve_provider(
    at: &str,
    name: &str,
    kept: &crate::ann::ProviderSetting,
    env: &dyn Env,
) -> Result<LlmBackend, Vec<String>> {
    use crate::ann::ProviderSetting;
    let (anthropic, base_url, api_key_env, timeout_seconds) = match kept {
        ProviderSetting::OpenAiCompatible {
            base_url,
            api_key_env,
            timeout_seconds,
            ..
        } => (false, base_url, api_key_env, *timeout_seconds),
        ProviderSetting::Anthropic {
            base_url,
            api_key_env,
            timeout_seconds,
            ..
        } => (true, base_url, api_key_env, *timeout_seconds),
        ProviderSetting::Runner { .. } => {
            return Err(vec![format!(
                "{at}: is another runner, reached through the hub"
            )]);
        }
    };
    let mut problems = Vec::new();
    if timeout_seconds == 0 {
        problems.push(format!("{at}: `timeout_seconds` must be greater than 0"));
    }
    let base_url = base_url.trim();
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        problems.push(format!(
            "{at}: `base_url` '{base_url}' must start with http:// or https://"
        ));
    }

    // A key is optional — a local llama-server usually wants none — but a
    // *named* variable that is empty is a different thing from no variable at
    // all: somebody expected authentication. Say so rather than sending an
    // empty bearer token and reading a 401 later.
    let api_key = match api_key_env {
        None => None,
        Some(key_var) => match env.get(key_var).filter(|v| !v.trim().is_empty()) {
            Some(k) => Some(k),
            None => {
                eprintln!(
                    "ensemble: note: {at} names ${key_var} for its API key, \
                     but it is unset; requests will be sent unauthenticated"
                );
                None
            }
        },
    };

    if problems.is_empty() {
        let base_url = base_url.trim_end_matches('/').to_string();
        Ok(LlmBackend {
            name: name.to_string(),
            timeout_seconds,
            settings: if anthropic {
                BackendSettings::Anthropic { base_url, api_key }
            } else {
                BackendSettings::OpenAiCompatible { base_url, api_key }
            },
        })
    } else {
        Err(problems)
    }
}
