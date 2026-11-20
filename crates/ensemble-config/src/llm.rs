//! The `llm` block of the hub's settings and of the runner's: the LLM
//! backends a hub or a runner uses. It calls its `direct` backends itself; the
//! others a runner serves, and it reaches them through the hub: `runners` in
//! the hub's settings, `hub` in the runner's. A runner may offer some of its
//! direct backends to the hub and other runners (`shares`), and says how many
//! such calls it takes at once (`max_shared_calls`).
//!
//! A name is unique across a file's groups, so whatever names a backend (the
//! hub's distiller and embedding model, a team's agents) names it alone, and a
//! backend moves between groups without changing where it is used. A process
//! reaches the backends it uses through the hub at a loopback address of its
//! own ([`RelayAt`]), where each looks like any OpenAI-compatible backend.

use super::validate::resolve_backend;
use super::*;
use crate::raw::RawLlmBackend;
use serde::Deserialize;

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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawHubLlm {
    pub(crate) direct: RawDirect,
    pub(crate) runners: Option<RawRelayedGroup>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawDirect {
    pub(crate) backends: BTreeMap<String, RawLlmBackend>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRunnerLlm {
    pub(crate) direct: RawRunnerDirect,
    pub(crate) hub: Option<RawRelayedGroup>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRunnerDirect {
    pub(crate) backends: BTreeMap<String, RawLlmBackend>,
    pub(crate) shares: Vec<String>,
    pub(crate) max_shared_calls: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRelayedGroup {
    pub(crate) backends: BTreeMap<String, RawRelayed>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRelayed {
    pub(crate) runner: String,
    pub(crate) backend: String,
}

/// A hub's `llm`, checked, its keys read from `env`.
pub(crate) fn hub_llm(raw: &RawHubLlm, env: &dyn Env, problems: &mut Vec<String>) -> HubLlm {
    let direct = direct("llm.direct", &raw.direct.backends, env, problems);
    let runners = relayed("llm.runners", raw.runners.as_ref(), &direct, problems);
    HubLlm { direct, runners }
}

/// A runner's `llm`, checked, its keys read from `env`. `hub` says whether
/// the runner names one, which sharing and the backends of `llm.hub` need.
pub(crate) fn runner_llm(
    raw: &RawRunnerLlm,
    hub: bool,
    env: &dyn Env,
    problems: &mut Vec<String>,
) -> RunnerLlm {
    let d = &raw.direct;
    let direct = direct("llm.direct", &d.backends, env, problems);
    let relayed = relayed("llm.hub", raw.hub.as_ref(), &direct, problems);
    let mut shares: Vec<String> = Vec::new();
    for name in &d.shares {
        let name = name.trim().to_string();
        match d.backends.get(&name) {
            None => problems.push(format!(
                "llm.direct.shares: '{name}' is not one of llm.direct.backends"
            )),
            Some(RawLlmBackend::OpenAiCompatibleApi(_)) => {}
            Some(_) => problems.push(format!(
                "llm.direct.shares: '{name}' is not an {OPENAI_COMPATIBLE_API} backend, the one kind a runner offers"
            )),
        }
        if shares.contains(&name) {
            problems.push(format!("llm.direct.shares: '{name}' is listed twice"));
        }
        shares.push(name);
    }
    if d.max_shared_calls == 0 {
        problems.push("llm.direct: `max_shared_calls` must be greater than 0".into());
    }
    if !hub && !shares.is_empty() {
        problems.push(
            "llm.direct.shares: a runner offers its backends through a hub, and there is no `hub`; say `shares: []`, or name the hub".into(),
        );
    }
    if !hub && !relayed.is_empty() {
        problems.push(
            "llm.hub: these backends are reached through a hub, and there is no `hub`; leave the block out, or name the hub".into(),
        );
    }
    RunnerLlm {
        direct,
        shares,
        max_shared_calls: d.max_shared_calls,
        hub: relayed,
    }
}

/// The backends of a `direct` group, each resolved with its key from `env`.
fn direct(
    at: &str,
    raw: &BTreeMap<String, RawLlmBackend>,
    env: &dyn Env,
    problems: &mut Vec<String>,
) -> BTreeMap<String, LlmBackend> {
    let mut out = BTreeMap::new();
    for (name, rb) in raw {
        if !crate::runner::valid_name(name) {
            problems.push(format!(
                "{at}.backends: the name '{name}' must be letters, digits, '-', '_' or '.'"
            ));
            continue;
        }
        match resolve_backend(&format!("{at}.backends.{name}"), name, rb, env) {
            Ok(b) => {
                out.insert(name.clone(), b);
            }
            Err(mut errs) => problems.append(&mut errs),
        }
    }
    out
}

/// The backends of a group reached through the hub, at `at`; their names are
/// not those of `direct`.
fn relayed(
    at: &str,
    raw: Option<&RawRelayedGroup>,
    direct: &BTreeMap<String, LlmBackend>,
    problems: &mut Vec<String>,
) -> BTreeMap<String, Relayed> {
    let mut out = BTreeMap::new();
    for (name, r) in raw.map(|g| &g.backends).into_iter().flatten() {
        let here = format!("{at}.backends.{name}");
        if !crate::runner::valid_name(name) {
            problems.push(format!(
                "{at}.backends: the name '{name}' must be letters, digits, '-', '_' or '.'"
            ));
            continue;
        }
        if direct.contains_key(name) {
            problems.push(format!(
                "{here}: llm.direct has a backend '{name}' too; a name is unique across the groups of `llm`"
            ));
        }
        let runner = r.runner.trim().to_string();
        if !valid_runner_id(&runner) {
            problems.push(format!(
                "{here}.runner: '{runner}' must be a runner as the hub knows it, <owner>/<name>, such as ana/gpu-box"
            ));
        }
        let backend = r.backend.trim().to_string();
        if !crate::runner::valid_name(&backend) {
            problems.push(format!(
                "{here}.backend: '{backend}' must be the name of a backend in that runner's llm.direct"
            ));
        }
        out.insert(name.clone(), Relayed { runner, backend });
    }
    out
}

/// Every name a file's `llm` defines, with its type: a backend reached
/// through the hub is OpenAI-compatible, as a runner offers no other.
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
