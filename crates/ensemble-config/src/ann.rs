//! The ANN pool of a hub or a runner: the artificial neural networks it may
//! call, in two layers. A **source** is where models come from: a server it
//! calls itself, or another runner, whose shared models it reaches through
//! the hub (`type: runner`). A **model** is what a source serves, an LLM or
//! an embedding model, found by asking the source; it is off until a person
//! turns it on, and a runner may share it through the hub.
//!
//! A source's name is unique in the pool, so whatever names a model (the
//! hub's distiller and embedding model, a team's agents) names it as
//! `<source>:<id>`. A process reaches the sources it uses through the hub at
//! a loopback address of its own ([`RelayAt`]), where each looks like any
//! OpenAI-compatible server; a model of one is named there as the runner
//! that shares it names it, `<its source>:<id>`.

use super::validate::resolve_source;
use super::*;
use serde::{Deserialize, Serialize};

/// How long a call to a source reached through the hub may take on the
/// caller's side: the runner that serves it holds it to its own source's
/// `timeout_seconds`, so this is only an upper bound.
pub const RELAYED_TIMEOUT_SECONDS: u64 = 3600;

/// The type of a source that is another runner, reached through the hub.
pub const RUNNER_SOURCE: &str = "runner";

/// Where a process serves the sources it reaches through the hub: an
/// address on loopback, `http://127.0.0.1:<port>`, and the key a call there
/// needs, made when the process starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayAt {
    pub base_url: String,
    pub key: String,
}

/// One source as it is kept: a server it calls itself, or another runner,
/// reached through the hub. Its key is named by the variable that holds it,
/// never written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum SourceSetting {
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

impl SourceSetting {
    /// Its type, as it is written.
    pub fn type_id(&self) -> &'static str {
        match self {
            SourceSetting::OpenAiCompatible { .. } => OPENAI_COMPATIBLE_API,
            SourceSetting::Anthropic { .. } => ANTHROPIC_API,
            SourceSetting::Runner { .. } => RUNNER_SOURCE,
        }
    }

    /// The variable its key is in, if it names one.
    pub fn api_key_env(&self) -> Option<&str> {
        match self {
            SourceSetting::OpenAiCompatible { api_key_env, .. }
            | SourceSetting::Anthropic { api_key_env, .. } => api_key_env.as_deref(),
            SourceSetting::Runner { .. } => None,
        }
    }

    /// Whether it serves the OpenAI API: a server of that type, or a runner,
    /// as a runner shares no other.
    pub fn is_openai(&self) -> bool {
        !matches!(self, SourceSetting::Anthropic { .. })
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

/// What a model is, as far as its source said.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelKind {
    /// Its source did not say; turning it on asks it.
    Unknown,
    /// Messages in, text out.
    Llm {
        tier: Tier,
        /// Tokens of context it runs with; None: its source did not say.
        context_window: Option<u32>,
        /// Whether it makes tool calls; None: its source did not say.
        tools: Option<bool>,
    },
    /// Text in, a vector out.
    Embedding { dimensions: u32 },
}

impl ModelKind {
    /// Its type, as it is written: `unknown`, `llm` or `embedding`.
    pub fn type_id(&self) -> &'static str {
        match self {
            ModelKind::Unknown => "unknown",
            ModelKind::Llm { .. } => "llm",
            ModelKind::Embedding { .. } => "embedding",
        }
    }

    /// It, in a few words: `llm, strong, 32768 tokens, tools`.
    pub fn describe(&self) -> String {
        match self {
            ModelKind::Unknown => "not known yet".into(),
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

/// One model of a source, as it is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolModel {
    /// The source that serves it.
    pub source: String,
    /// Its id there, as the source lists it.
    pub id: String,
    pub kind: ModelKind,
    /// Whether it may be used: a model a source newly lists is off until a
    /// person turns it on.
    pub on: bool,
    /// The source listed it before, and does not now.
    pub missing: bool,
    /// Offered through the hub, to the hub and its owner's other runners: a
    /// runner's alone.
    pub shared: bool,
}

impl PoolModel {
    /// How it is named: `<source>:<id>`.
    pub fn name(&self) -> String {
        format!("{}:{}", self.source, self.id)
    }

    /// Whether it may be used as an LLM.
    pub fn usable_llm(&self) -> bool {
        self.on && matches!(self.kind, ModelKind::Llm { .. })
    }

    /// Whether it may be used as an embedding model.
    pub fn usable_embedding(&self) -> bool {
        self.on && matches!(self.kind, ModelKind::Embedding { .. })
    }
}

/// The ANN pool as it is kept: its sources by name, and the models they
/// serve.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnPoolSetting {
    pub sources: BTreeMap<String, SourceSetting>,
    pub models: Vec<PoolModel>,
}

impl AnnPoolSetting {
    /// The model `id` of `source`.
    pub fn model(&self, source: &str, id: &str) -> Option<&PoolModel> {
        self.models
            .iter()
            .find(|m| m.source == source && m.id == id)
    }

    pub fn model_mut(&mut self, source: &str, id: &str) -> Option<&mut PoolModel> {
        self.models
            .iter_mut()
            .find(|m| m.source == source && m.id == id)
    }

    /// Keep the model `id` of `source` as `kind`, turned on, among the
    /// models in the order of their sources and ids.
    pub fn turn_on(&mut self, source: &str, id: &str, kind: ModelKind) {
        match self.model_mut(source, id) {
            Some(m) => {
                m.kind = kind;
                m.on = true;
            }
            None => self.models.push(PoolModel {
                source: source.to_string(),
                id: id.to_string(),
                kind,
                on: true,
                missing: false,
                shared: false,
            }),
        }
        self.models
            .sort_by(|a, b| (&a.source, &a.id).cmp(&(&b.source, &b.id)));
    }
}

/// A model `<source>:<id>` split: the source's name holds no `:`, the id may.
pub fn split_name(name: &str) -> Option<(&str, &str)> {
    name.trim()
        .split_once(':')
        .filter(|(source, id)| !source.is_empty() && !id.is_empty())
}

/// The ANN pool of a hub or a runner, resolved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnnPool {
    /// The sources it calls itself, by name, with their keys.
    pub direct: BTreeMap<String, LlmBackend>,
    /// The most calls at once to each of `direct`.
    pub max_concurrent: BTreeMap<String, u32>,
    /// The sources that are other runners, by name: each runner as
    /// `<owner>/<name>`.
    pub relayed: BTreeMap<String, String>,
    pub models: Vec<PoolModel>,
}

/// One source a runner shares, as it offers it through the hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shared<'a> {
    pub source: &'a LlmBackend,
    pub max_concurrent: u32,
    pub models: Vec<&'a PoolModel>,
}

impl AnnPool {
    /// Every source it may call, by name: its own, and, when it serves them
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

    /// The model `id` of `source`.
    pub fn model(&self, source: &str, id: &str) -> Option<&PoolModel> {
        self.models
            .iter()
            .find(|m| m.source == source && m.id == id)
    }

    /// The sources it shares through the hub, each with the models of it
    /// shared.
    pub fn shared(&self) -> Vec<Shared<'_>> {
        self.direct
            .iter()
            .filter_map(|(name, source)| {
                let models: Vec<&PoolModel> = self
                    .models
                    .iter()
                    .filter(|m| m.shared && m.source == *name)
                    .collect();
                (!models.is_empty()).then(|| Shared {
                    source,
                    max_concurrent: self.max_concurrent.get(name).copied().unwrap_or(1),
                    models,
                })
            })
            .collect()
    }

    /// Each runner it reaches through the hub, by the source's name: what a
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

/// The pool of `kept`, checked, with the keys of its sources from `env`. Only
/// a runner's models are shared, when `may_share`.
pub(crate) fn pool(
    kept: &AnnPoolSetting,
    may_share: bool,
    env: &dyn Env,
    problems: &mut Vec<String>,
) -> AnnPool {
    let mut out = AnnPool::default();
    for (name, s) in &kept.sources {
        let at = format!("ann_pool.sources.{name}");
        if !crate::runner::valid_name(name) {
            problems.push(format!(
                "ann_pool.sources: the name '{name}' must be letters, digits, '-', '_' or '.'"
            ));
            continue;
        }
        match s {
            SourceSetting::Runner { runner } => {
                let runner = runner.trim().to_string();
                if !valid_runner_id(&runner) {
                    problems.push(format!(
                        "{at}.runner: '{runner}' must be a runner as the hub knows it, <owner>/<name>, such as ana/gpu-box"
                    ));
                }
                out.relayed.insert(name.clone(), runner);
            }
            SourceSetting::OpenAiCompatible { max_concurrent, .. }
            | SourceSetting::Anthropic { max_concurrent, .. } => {
                if *max_concurrent == 0 {
                    problems.push(format!("{at}: `max_concurrent` must be greater than 0"));
                }
                match resolve_source(&at, name, s, env) {
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
        let Some(source) = kept.sources.get(&m.source) else {
            problems.push(format!("{at}: there is no source '{}'", m.source));
            continue;
        };
        if m.id.trim().is_empty() {
            problems.push(format!(
                "ann_pool.models: a model of {} has no id",
                m.source
            ));
        }
        if seen.contains(&(&m.source, &m.id)) {
            problems.push(format!("{at}: is listed twice"));
        }
        seen.push((&m.source, &m.id));
        if m.on && m.kind == ModelKind::Unknown {
            problems.push(format!(
                "{at}: is on, and what it is is not known: an LLM or an embedding model"
            ));
        }
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
            } else if !m.on {
                problems.push(format!("{at}: is shared, and off"));
            } else if !matches!(source, SourceSetting::OpenAiCompatible { .. }) {
                problems.push(format!(
                    "{at}: is shared, and only a model of an {OPENAI_COMPATIBLE_API} source of the runner's own may be"
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

/// What a pool is checked against: each source's name with whether it
/// serves the OpenAI API, and the models.
pub(crate) struct Known<'a> {
    pub(crate) openai: BTreeMap<&'a str, bool>,
    pub(crate) models: &'a [PoolModel],
}

impl AnnPoolSetting {
    pub(crate) fn known(&self) -> Known<'_> {
        Known {
            openai: self
                .sources
                .iter()
                .map(|(name, s)| (name.as_str(), s.is_openai()))
                .collect(),
            models: &self.models,
        }
    }
}

impl AnnPool {
    pub(crate) fn known(&self) -> Known<'_> {
        Known {
            openai: self
                .direct
                .iter()
                .map(|(name, b)| (name.as_str(), b.type_id() == OPENAI_COMPATIBLE_API))
                .chain(self.relayed.keys().map(|name| (name.as_str(), true)))
                .collect(),
            models: &self.models,
        }
    }
}

/// Check that the model `id` of `source`, named at `at`, is one of `pool`'s,
/// on, and of `use_`; for OpenCode, of an OpenAI-compatible source.
pub(crate) fn check_model(
    at: &str,
    source: &str,
    id: &str,
    use_: Use,
    opencode: bool,
    pool: &Known<'_>,
    problems: &mut Vec<String>,
) {
    let Some(openai) = pool.openai.get(source) else {
        let known: Vec<&str> = pool.openai.keys().copied().collect();
        problems.push(format!(
            "{at}: there is no source '{source}' in the ANN pool. Its sources: {}.",
            if known.is_empty() {
                "(none)".to_string()
            } else {
                known.join(", ")
            }
        ));
        return;
    };
    if opencode && !openai {
        problems.push(format!(
            "{at}: the source '{source}' is of type '{ANTHROPIC_API}', which OpenCode cannot use: it needs an '{OPENAI_COMPATIBLE_API}' one. Use a model of type '{PROVIDER_MODEL}', a `claude-code` agent, or a `direct` agent"
        ));
        return;
    }
    let name = format!("{source}:{id}");
    let Some(model) = pool
        .models
        .iter()
        .find(|m| m.source == source && m.id == id)
    else {
        problems.push(format!(
            "{at}: {name} is not a model of the ANN pool; `source refresh {source}` asks the source again"
        ));
        return;
    };
    let fits = match use_ {
        Use::Llm => model.usable_llm(),
        Use::Embedding => model.usable_embedding(),
    };
    if !model.on {
        problems.push(format!(
            "{at}: {name} is off; `model on {name}` turns it on"
        ));
    } else if !fits {
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
