//! The operations on the LLM backends a hub or a runner keeps in its
//! settings, for every interface to reach: `backend.list`,
//! `backend.add`, `backend.remove`, `backend.test` and `backend.key`, and, for a
//! runner, `backend.share` and `backend.unshare`.
//!
//! A person gives what a backend is and its key, never the name of a
//! variable: the key is kept under a variable Ensemble names after the
//! backend ([`backend_key_variable`](ensemble_config::backend_key_variable)),
//! and the backend names that variable. Where it is kept is the context's
//! ([`WithBackends::keep_key`]): sealed in its database. One who keeps the
//! key elsewhere already, such
//! as a secrets manager that sets a variable, names that variable instead
//! (`key_env`). A backend is asked which models it serves before it is added,
//! so a wrong address or key is said at once, and an answer that wants a key
//! has an interface ask for it.

use ensemble_config::kept::Change;
use ensemble_config::llm::RUNNER_BACKEND;
use ensemble_config::{
    ANTHROPIC_API, BackendSetting, BackendSettings, LlmBackend, OPENAI_COMPATIBLE_API, Relayed,
    backend_key_variable,
};
use ensemble_ops::{Access, Caller, Code, Error, Field, Kind, Operation, Spec, operation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// A context whose settings keep LLM backends: a hub's or a runner's.
pub trait WithBackends {
    /// Why it may keep no key, if it may not.
    fn can_keep_keys(&self) -> Result<(), Error>;

    /// Keep `key` as the value of the variable `name`, or with none, forget
    /// it.
    fn keep_key(&self, name: &str, key: Option<&str>) -> Result<(), Error>;

    /// The backends kept, as they are written.
    fn kept_backends(&self) -> Result<KeptBackends, Error>;

    /// Make `edit` to the backends kept, and keep them with `change` once the
    /// settings still load, and what uses the backends, such as a runner's
    /// teams, still does; say what the running process did with them.
    fn change_backends(
        &self,
        change: Change,
        edit: &mut dyn FnMut(&mut KeptBackends) -> Result<(), Error>,
    ) -> Result<Option<String>, Error>;

    /// Tell the running process its backends changed though the settings did
    /// not, as when a key one of them names did; say what it did.
    fn backends_changed(&self) -> Option<String>;

    /// The backends as they are now, with their keys.
    fn backends(&self) -> Result<Backends, Error>;

    /// The value of the variable `name`, wherever it looks for one.
    fn variable(&self, name: &str) -> Option<String>;
}

/// The backends a hub or a runner keeps, as they are written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptBackends {
    pub backends: BTreeMap<String, BackendSetting>,
    /// Those of its own it offers through the hub: a runner's; a hub offers
    /// none.
    pub shares: Option<Vec<String>>,
}

/// The backends of a hub or a runner, resolved.
pub struct Backends {
    /// Those it calls itself.
    pub direct: BTreeMap<String, LlmBackend>,
    /// Those a runner shares, which it reaches through the hub.
    pub relayed: BTreeMap<String, Relayed>,
    /// Those of `direct` it offers through the hub: a runner's; a hub offers
    /// none.
    pub shares: Option<Vec<String>>,
}

/// Whose backends they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Hub,
    Runner,
}

/// The type of a backend a runner shares, reached through the hub.
pub const RUNNER_TYPE: &str = RUNNER_BACKEND;
const TYPES: &[&str] = &[OPENAI_COMPATIBLE_API, ANTHROPIC_API, RUNNER_TYPE];
/// Seconds one request may take, when an addition does not say.
const TIMEOUT_SECONDS: u64 = 300;

impl Side {
    fn program(self) -> &'static str {
        match self {
            Side::Hub => "ensemble-hub",
            Side::Runner => "ensemble-runner",
        }
    }
}

const NAME: Field = Field::text(
    "name",
    "The backend's name, as what uses it names it: letters, digits, '-' or '_'.",
)
.required()
.positional();
const KEY: Field = Field::secret("key", "The backend's API key (empty for none)");
const KEY_ENV: Field = Field::text(
    "key_env",
    "A variable that holds the key already, such as one a secrets manager sets, in place of a key Ensemble keeps.",
);
const NO_CHECK: Field = Field::new(
    "no_check",
    Kind::Flag,
    "Do not ask the backend which models it serves first.",
);
const ADD_FIELDS: [Field; 9] = [
    NAME,
    Field::new("type", Kind::Choice(TYPES), "What it is: an OpenAI-compatible server, Anthropic's API, or a backend a runner shares through the hub.").required(),
    Field::text("url", "Where the server is, such as http://127.0.0.1:8888/v1 or https://api.anthropic.com."),
    Field::text("runner", "The runner that shares it, as the hub knows it: <owner>/<name>."),
    Field::text("runner_backend", "Its name on that runner; the same name when left out."),
    KEY,
    KEY_ENV,
    Field::new("timeout", Kind::Number, "Seconds one request may take; 300 when left out."),
    NO_CHECK,
];
const ADD_HUB: &[Field] = &ADD_FIELDS;
const ADD_RUNNER: &[Field] = &[
    ADD_FIELDS[0],
    ADD_FIELDS[1],
    ADD_FIELDS[2],
    ADD_FIELDS[3],
    ADD_FIELDS[4],
    ADD_FIELDS[5],
    ADD_FIELDS[6],
    ADD_FIELDS[7],
    ADD_FIELDS[8],
    Field::new(
        "share",
        Kind::Flag,
        "Offer it through the hub, to the hub and other runners.",
    ),
];
const NAMED: &[Field] = &[NAME];
const TEST: &[Field] = &[Field::text("name", "One backend; every one when left out.").positional()];
const KEY_FIELDS: &[Field] = &[
    NAME,
    Field::secret("key", "The backend's new API key (empty takes its key out)"),
    KEY_ENV,
    NO_CHECK,
];

#[derive(Deserialize, Serialize)]
struct Add {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    url: Option<String>,
    runner: Option<String>,
    runner_backend: Option<String>,
    key: Option<String>,
    key_env: Option<String>,
    timeout: Option<u64>,
    #[serde(default)]
    no_check: bool,
    #[serde(default)]
    share: bool,
}

#[derive(Deserialize, Serialize)]
struct Named {
    name: String,
}

#[derive(Deserialize)]
struct Test {
    name: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct SetKey {
    name: String,
    key: Option<String>,
    key_env: Option<String>,
    #[serde(default)]
    no_check: bool,
}

#[derive(Deserialize)]
struct Nothing {}

/// The backend operations of a hub or a runner, for those `access` lets.
pub fn operations<C: WithBackends + 'static>(side: Side, access: Access) -> Vec<Operation<C>> {
    let add_spec = Spec {
        name: "backend.add",
        summary: "Add an LLM backend, once it answers: its key is kept for you, never written in the settings.",
        access,
        input: match side {
            Side::Hub => ADD_HUB,
            Side::Runner => ADD_RUNNER,
        },
    };
    let remove_spec = Spec {
        name: "backend.remove",
        summary: "Take an LLM backend out, and the key Ensemble kept for it; refused while something uses it.",
        access,
        input: NAMED,
    };
    let key_spec = Spec {
        name: "backend.key",
        summary: "Give a backend a new API key, or take its key out.",
        access,
        input: KEY_FIELDS,
    };
    let mut all = vec![
        operation(
            Spec {
                name: "backend.list",
                summary: "List the LLM backends: what each is, where, and whether it has a key.",
                access,
                input: &[],
            },
            move |c: &C, _: &Caller, _: Nothing| list(c, side),
        ),
        operation(add_spec, move |c: &C, caller: &Caller, input: Add| {
            let change = Change::new(caller, &add_spec, &input);
            add(c, side, input, change)
        }),
        operation(remove_spec, move |c: &C, caller: &Caller, input: Named| {
            let change = Change::new(caller, &remove_spec, &input);
            remove(c, &input.name, change)
        }),
        operation(
            Spec {
                name: "backend.test",
                summary: "Ask the backends which models they serve.",
                access,
                input: TEST,
            },
            |c: &C, _: &Caller, input: Test| test(c, input.name.as_deref()),
        ),
        operation(key_spec, move |c: &C, caller: &Caller, input: SetKey| {
            let change = Change::new(caller, &key_spec, &input);
            set_key(c, input, change)
        }),
    ];
    if side == Side::Runner {
        let share_spec = Spec {
            name: "backend.share",
            summary: "Offer one of the runner's own backends through the hub, to the hub and other runners.",
            access,
            input: NAMED,
        };
        let unshare_spec = Spec {
            name: "backend.unshare",
            summary: "Stop offering a backend through the hub.",
            access,
            input: NAMED,
        };
        all.push(operation(
            share_spec,
            move |c: &C, caller: &Caller, input: Named| {
                let change = Change::new(caller, &share_spec, &input);
                share(c, &input.name, true, change)
            },
        ));
        all.push(operation(
            unshare_spec,
            move |c: &C, caller: &Caller, input: Named| {
                let change = Change::new(caller, &unshare_spec, &input);
                share(c, &input.name, false, change)
            },
        ));
    }
    all
}

/// Each backend: what it is, where, and whether it has a key.
fn list<C: WithBackends>(c: &C, side: Side) -> Result<Value, Error> {
    let backends = c.backends()?;
    let kept = c.kept_backends()?;
    let shares = backends.shares.clone().unwrap_or_default();
    let mut rows = Vec::new();
    let mut said = Vec::new();
    for (name, b) in &backends.direct {
        let env = kept
            .backends
            .get(name)
            .and_then(BackendSetting::api_key_env)
            .map(String::from);
        let key = match (&env, b.settings.api_key()) {
            (None, _) => "no key".to_string(),
            (Some(var), Some(_)) if *var == backend_key_variable(name) => "key kept".into(),
            (Some(var), Some(_)) => format!("key from ${var}"),
            (Some(var), None) => format!("no key: ${var} is not set"),
        };
        let shared = shares.contains(name);
        said.push(format!(
            "{name}  {} at {}  ({key}{})",
            b.type_id(),
            b.settings.base_url(),
            if shared {
                ", shared through the hub"
            } else {
                ""
            }
        ));
        rows.push(json!({
            "name": name,
            "type": b.type_id(),
            "url": b.settings.base_url(),
            "key": b.settings.api_key().is_some(),
            "key_env": env,
            "shared": shared,
        }));
    }
    for (name, r) in &backends.relayed {
        said.push(format!(
            "{name}  {} through the hub: {} on {}",
            RUNNER_TYPE, r.backend, r.runner
        ));
        rows.push(json!({
            "name": name,
            "type": RUNNER_TYPE,
            "runner": r.runner,
            "runner_backend": r.backend,
        }));
    }
    if said.is_empty() {
        said.push(format!(
            "no LLM backends yet; `{} backend add` adds one",
            side.program()
        ));
    }
    Ok(json!({ "said": said.join("\n"), "backends": rows }))
}

fn add<C: WithBackends>(c: &C, side: Side, input: Add, change: Change) -> Result<Value, Error> {
    let name = input.name.trim().to_string();
    check_name(&name)?;
    if c.kept_backends()?.backends.contains_key(&name) {
        return Err(Error::new(
            Code::Conflict,
            format!("there is a backend '{name}' already"),
        )
        .on("name")
        .hint("`backend remove` it first, or choose another name"));
    }
    if input.share && side == Side::Runner && input.kind != OPENAI_COMPATIBLE_API {
        return Err(
            Error::invalid("only an openai-compatible-api backend may be shared").on("share"),
        );
    }
    let mut models = None;
    let mut keep = None;
    let setting = if input.kind == RUNNER_TYPE {
        for (given, field) in [
            (&input.url, "url"),
            (&input.key, "key"),
            (&input.key_env, "key_env"),
        ] {
            if given.is_some() {
                return Err(Error::invalid(format!(
                    "a backend a runner shares takes no `{field}`: the runner keeps its own"
                ))
                .on(field));
            }
        }
        let runner =
            input.runner.as_deref().map(str::trim).ok_or_else(|| {
                Error::invalid("`runner` is required: <owner>/<name>").on("runner")
            })?;
        if !ensemble_config::llm::valid_runner_id(runner) {
            return Err(Error::invalid(format!(
                "'{runner}' is not a runner as the hub knows it: <owner>/<name>, such as ana/gpu-box"
            ))
            .on("runner"));
        }
        let there = input
            .runner_backend
            .as_deref()
            .map(str::trim)
            .unwrap_or(&name);
        BackendSetting::Runner {
            runner: runner.to_string(),
            backend: there.to_string(),
        }
    } else {
        if input.runner.is_some() || input.runner_backend.is_some() {
            return Err(Error::invalid("`runner` is for a backend a runner shares").on("runner"));
        }
        let url = input
            .url
            .as_deref()
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                Error::invalid(format!("`url` is required for an {} backend", input.kind)).on("url")
            })?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(
                Error::invalid(format!("'{url}' must start with http:// or https://")).on("url"),
            );
        }
        if input.key.is_some() && input.key_env.is_some() {
            return Err(Error::invalid("give `key` or `key_env`, not both").on("key_env"));
        }
        let timeout = input.timeout.unwrap_or(TIMEOUT_SECONDS);
        if timeout == 0 {
            return Err(Error::invalid("`timeout` must be greater than 0").on("timeout"));
        }
        let key = given_key(c, input.key.as_deref(), input.key_env.as_deref())?;
        // Where a key given would go, before the backend is asked anything.
        if input.key.as_deref().is_some_and(|k| !k.is_empty()) {
            c.can_keep_keys()?;
        }
        let backend = backend_of(&name, &input.kind, &url, timeout, key.clone());
        if input.kind == ANTHROPIC_API && key.is_none() && input.key.is_none() {
            return Err(Error::invalid("Anthropic's API wants an API key").on("key"));
        }
        if !input.no_check {
            models = Some(ask(&backend, key.is_none())?);
        }
        let env = match (&input.key_env, &input.key) {
            (Some(var), _) => Some(var.trim().to_string()),
            (None, Some(k)) if !k.is_empty() => {
                let var = backend_key_variable(&name);
                keep = Some((var.clone(), k.clone()));
                Some(var)
            }
            _ => None,
        };
        if input.kind == ANTHROPIC_API {
            BackendSetting::Anthropic {
                base_url: url,
                api_key_env: env,
                timeout_seconds: timeout,
            }
        } else {
            BackendSetting::OpenAiCompatible {
                base_url: url,
                api_key_env: env,
                timeout_seconds: timeout,
            }
        }
    };
    let share = input.share && side == Side::Runner;
    // The key first, so that the running process finds it when it uses the
    // backend; taken out again when the backend is not kept.
    if let Some((var, key)) = &keep {
        c.keep_key(var, Some(key))?;
    }
    let kept = c.change_backends(change, &mut |k| {
        k.backends.insert(name.clone(), setting.clone());
        if share {
            k.shares.get_or_insert_default().push(name.clone());
        }
        Ok(())
    });
    let note = match kept {
        Ok(note) => note,
        Err(e) => {
            if let Some((var, _)) = &keep {
                let _ = c.keep_key(var, None);
            }
            return Err(e);
        }
    };
    let serves = match &models {
        Some(m) => format!(", which serves {}", listed(m)),
        None => String::new(),
    };
    Ok(json!({
        "said": format!("added backend {name}{serves}"),
        "name": name,
        "models": models,
        "note": note,
    }))
}

fn remove<C: WithBackends>(c: &C, name: &str, change: Change) -> Result<Value, Error> {
    let Some(was) = c.kept_backends()?.backends.remove(name) else {
        return Err(Error::not_found(format!("there is no backend '{name}'")).on("name"));
    };
    let ours = backend_key_variable(name);
    let kept = (was.api_key_env() == Some(ours.as_str())).then_some(ours);
    let note = c.change_backends(change, &mut |k| {
        k.backends.remove(name);
        if let Some(shares) = &mut k.shares {
            shares.retain(|s| s != name);
        }
        Ok(())
    })?;
    if let Some(var) = kept
        && c.can_keep_keys().is_ok()
    {
        c.keep_key(&var, None)?;
    }
    Ok(json!({ "said": format!("removed backend {name}"), "note": note }))
}

fn test<C: WithBackends>(c: &C, name: Option<&str>) -> Result<Value, Error> {
    let backends = c.backends()?;
    if let Some(name) = name
        && !backends.direct.contains_key(name)
        && !backends.relayed.contains_key(name)
    {
        return Err(Error::not_found(format!("there is no backend '{name}'")).on("name"));
    }
    let mut said = Vec::new();
    let mut rows = Vec::new();
    for (n, b) in &backends.direct {
        if name.is_some_and(|w| w != n) {
            continue;
        }
        match crate::models::models(b) {
            Ok(m) => {
                said.push(format!("{n}: answers, serves {}", listed(&m)));
                rows.push(json!({ "name": n, "ok": true, "models": m }));
            }
            Err(e) => {
                said.push(format!("{n}: {e}"));
                rows.push(json!({ "name": n, "ok": false, "error": e.to_string() }));
            }
        }
    }
    for (n, r) in &backends.relayed {
        if name.is_some_and(|w| w != n) {
            continue;
        }
        said.push(format!(
            "{n}: shared by {}; `runner backends` on the hub lists what connected runners share now",
            r.runner
        ));
        rows.push(json!({ "name": n, "ok": null }));
    }
    if said.is_empty() {
        said.push("no LLM backends yet".into());
    }
    Ok(json!({ "said": said.join("\n"), "backends": rows }))
}

fn set_key<C: WithBackends>(c: &C, input: SetKey, change: Change) -> Result<Value, Error> {
    let name = input.name.as_str();
    let backends = c.backends()?;
    let Some(backend) = backends.direct.get(name) else {
        let message = if backends.relayed.contains_key(name) {
            format!("'{name}' is shared by a runner, which keeps its key")
        } else {
            format!("there is no backend '{name}'")
        };
        return Err(Error::not_found(message).on("name"));
    };
    let now = c
        .kept_backends()?
        .backends
        .get(name)
        .and_then(BackendSetting::api_key_env)
        .map(String::from);
    let ours = backend_key_variable(name);
    let (env, keep): (Option<String>, Option<String>) = match (&input.key_env, &input.key) {
        (Some(_), Some(_)) => {
            return Err(Error::invalid("give `key` or `key_env`, not both").on("key_env"));
        }
        (Some(var), None) => (Some(var.trim().to_string()), None),
        (None, Some(k)) if k.is_empty() => (None, None),
        (None, Some(k)) => (Some(ours.clone()), Some(k.clone())),
        (None, None) => {
            return Err(Error::invalid("give the new key, or empty to take it out").on("key"));
        }
    };
    // Whether the key can be kept, before the backend is asked anything.
    if keep.is_some() {
        c.can_keep_keys()?;
    }
    let key = match (&keep, &env) {
        (Some(k), _) => Some(k.clone()),
        (None, Some(var)) => c.variable(var),
        (None, None) => None,
    };
    let mut models = None;
    if !input.no_check {
        let tried = LlmBackend {
            settings: with_key(&backend.settings, key.clone()),
            ..backend.clone()
        };
        models = Some(ask(&tried, false)?);
    }
    if let Some(k) = &keep {
        c.keep_key(&ours, Some(k))?;
    }
    let note = if env != now {
        c.change_backends(change, &mut |k| {
            match k.backends.get_mut(name) {
                Some(
                    BackendSetting::OpenAiCompatible { api_key_env, .. }
                    | BackendSetting::Anthropic { api_key_env, .. },
                ) => *api_key_env = env.clone(),
                _ => return Err(Error::not_found(format!("there is no backend '{name}'"))),
            }
            Ok(())
        })?
    } else {
        c.backends_changed()
    };
    if now.as_deref() == Some(ours.as_str())
        && env.as_deref() != Some(ours.as_str())
        && c.can_keep_keys().is_ok()
    {
        c.keep_key(&ours, None)?;
    }
    let what = match (&env, &keep) {
        (None, _) => "has no key now".to_string(),
        (Some(_), Some(_)) => "has its new key".to_string(),
        (Some(var), None) => format!("takes its key from ${var}"),
    };
    let serves = match &models {
        Some(m) => format!("; it serves {}", listed(m)),
        None => String::new(),
    };
    Ok(json!({ "said": format!("{name} {what}{serves}"), "note": note }))
}

fn share<C: WithBackends>(c: &C, name: &str, on: bool, change: Change) -> Result<Value, Error> {
    let backends = c.backends()?;
    if !backends.direct.contains_key(name) {
        return Err(
            Error::not_found(format!("the runner has no backend '{name}' of its own")).on("name"),
        );
    }
    let has = backends
        .shares
        .unwrap_or_default()
        .iter()
        .any(|s| s == name);
    if has == on {
        let state = if on {
            "is shared already"
        } else {
            "is not shared"
        };
        return Ok(json!({ "said": format!("{name} {state}") }));
    }
    let note = c.change_backends(change, &mut |k| {
        let shares = k.shares.get_or_insert_default();
        if on {
            shares.push(name.to_string());
        } else {
            shares.retain(|s| s != name);
        }
        Ok(())
    })?;
    let state = if on {
        "is shared through the hub"
    } else {
        "is no longer shared"
    };
    Ok(json!({ "said": format!("{name} {state}"), "note": note }))
}

/// Refuse a name that is not one word of letters, digits, '-' or '_'.
fn check_name(name: &str) -> Result<(), Error> {
    let fits = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if fits {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "'{name}' must be letters, digits, '-' or '_', such as central or team-gpu"
        ))
        .on("name"))
    }
}

/// The key a backend is tried with: the one given, or the value of the
/// variable named.
fn given_key<C: WithBackends>(
    c: &C,
    key: Option<&str>,
    key_env: Option<&str>,
) -> Result<Option<String>, Error> {
    match (key, key_env) {
        (Some(k), _) => Ok(Some(k.to_string()).filter(|k| !k.is_empty())),
        (None, Some(var)) => {
            let var = var.trim();
            let valid = var
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !valid {
                return Err(Error::invalid(format!("'{var}' is not a variable name")).on("key_env"));
            }
            Ok(c.variable(var))
        }
        (None, None) => Ok(None),
    }
}

fn backend_of(name: &str, kind: &str, url: &str, timeout: u64, key: Option<String>) -> LlmBackend {
    let settings = if kind == ANTHROPIC_API {
        BackendSettings::Anthropic {
            base_url: url.to_string(),
            api_key: key,
        }
    } else {
        BackendSettings::OpenAiCompatible {
            base_url: url.to_string(),
            api_key: key,
        }
    };
    LlmBackend {
        name: name.to_string(),
        timeout_seconds: timeout,
        settings,
    }
}

fn with_key(settings: &BackendSettings, key: Option<String>) -> BackendSettings {
    match settings {
        BackendSettings::OpenAiCompatible { base_url, .. } => BackendSettings::OpenAiCompatible {
            base_url: base_url.clone(),
            api_key: key,
        },
        BackendSettings::Anthropic { base_url, .. } => BackendSettings::Anthropic {
            base_url: base_url.clone(),
            api_key: key,
        },
    }
}

/// The models `backend` serves, or why it cannot be added: one that wants a
/// key it was not given has an interface ask for it, when `may_ask`.
fn ask(backend: &LlmBackend, may_ask: bool) -> Result<Vec<String>, Error> {
    use ensemble_core::port::Error as Port;
    crate::models::models(backend).map_err(|e| match e {
        Port::Api {
            status: 401 | 403, ..
        } if may_ask => {
            Error::invalid(format!("{} wants an API key", backend.settings.base_url())).on("key")
        }
        Port::Api {
            status: 401 | 403, ..
        } => Error::invalid(format!(
            "{} did not accept the key: {e}",
            backend.settings.base_url()
        ))
        .on("key"),
        e => Error::new(
            Code::Unavailable,
            format!("could not ask it which models it serves: {e}"),
        )
        .hint("check the address, or go on without asking with `no_check`"),
    })
}

/// A few model names, and how many more.
fn listed(models: &[String]) -> String {
    const SHOWN: usize = 5;
    match models.len() {
        0 => "no models".into(),
        n if n <= SHOWN => models.join(", "),
        n => format!("{} and {} more", models[..SHOWN].join(", "), n - SHOWN),
    }
}
