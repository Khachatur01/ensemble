//! The operations on the ANN pool a hub or a runner keeps in its settings,
//! for every interface to reach: its sources, `source.list`, `source.add`,
//! `source.remove`, `source.test`, `source.key` and `source.refresh`, and
//! the models they serve, `model.list`, `model.on`, `model.off` and
//! `model.set`, and, for a runner, `model.share` and `model.unshare`.
//!
//! A person gives what a source is and its key, never the name of a
//! variable: the key is kept under a variable Ensemble names after the
//! source ([`source_key_variable`](ensemble_config::source_key_variable)),
//! and the source names that variable. Where it is kept is the context's
//! ([`WithPool::keep_key`]): sealed in its database. One who keeps the key
//! elsewhere already, such as a secrets manager that sets a variable, names
//! that variable instead (`key_env`).
//!
//! A source is asked which models it serves when it is added, on
//! `source.refresh`, and when the process starts ([`refresh_all`]), so a
//! wrong address or key is said at once, and an answer that wants a key has
//! an interface ask for it. A model it lists newly is kept off until a person
//! turns it on; one it no longer lists is marked missing, never forgotten, so
//! what was said of it comes back with it.

use crate::models::{Found, discover, llm};
use ensemble_config::ann::{RUNNER_SOURCE, split_name, valid_runner_id};
use ensemble_config::kept::Change;
use ensemble_config::{
    ANTHROPIC_API, AnnPool, AnnPoolSetting, BackendSettings, LlmBackend, ModelKind,
    OPENAI_COMPATIBLE_API, PoolModel, SourceSetting, Tier, source_key_variable,
};
use ensemble_ops::{Access, Caller, Code, Error, Field, Kind, Operation, Spec, operation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A context whose settings keep an ANN pool: a hub's or a runner's.
pub trait WithPool {
    /// Why it may keep no key, if it may not.
    fn can_keep_keys(&self) -> Result<(), Error>;

    /// Keep `key` as the value of the variable `name`, or with none, forget
    /// it.
    fn keep_key(&self, name: &str, key: Option<&str>) -> Result<(), Error>;

    /// The pool kept, as it is written.
    fn kept_pool(&self) -> Result<AnnPoolSetting, Error>;

    /// Make `edit` to the pool kept, and keep it with `change` once the
    /// settings still load, and what uses the pool, such as a runner's
    /// teams, still does; say what the running process did with it.
    fn change_pool(
        &self,
        change: Change,
        edit: &mut dyn FnMut(&mut AnnPoolSetting) -> Result<(), Error>,
    ) -> Result<Option<String>, Error>;

    /// Tell the running process its pool changed though the settings did
    /// not, as when a key one of its sources names did; say what it did.
    fn pool_changed(&self) -> Option<String>;

    /// The pool as it is now, with its keys.
    fn pool(&self) -> Result<AnnPool, Error>;

    /// The value of the variable `name`, wherever it looks for one.
    fn variable(&self, name: &str) -> Option<String>;

    /// The models the runner `runner` shares, as the hub knows them now:
    /// what a source that is that runner serves. Why they cannot be asked
    /// for now, if they cannot.
    fn shared_by(&self, runner: &str) -> Result<Vec<Found>, Error>;
}

/// Whose pool it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Hub,
    Runner,
}

const TYPES: &[&str] = &[OPENAI_COMPATIBLE_API, ANTHROPIC_API, RUNNER_SOURCE];
/// Seconds one request may take, when an addition does not say.
const TIMEOUT_SECONDS: u64 = 300;
const YES_NO: &[&str] = &["yes", "no"];

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
    "The source's name, as a model of it is named, <source>:<model>: letters, digits, '-' or '_'.",
)
.required()
.positional();
const MODEL: Field = Field::text(
    "model",
    "The model, as <source>:<model>, such as local:qwen3.5:9b.",
)
.required()
.positional();
const KEY: Field = Field::secret("key", "The source's API key (empty for none)");
const KEY_ENV: Field = Field::text(
    "key_env",
    "A variable that holds the key already, such as one a secrets manager sets, in place of a key Ensemble keeps.",
);
const NO_CHECK: Field = Field::new(
    "no_check",
    Kind::Flag,
    "Do not ask the source which models it serves first.",
);
const TIER: Field = Field::new(
    "tier",
    Kind::Choice(Tier::NAMES),
    "How strong the LLM is, for choosing one that is enough.",
);
const ADD: &[Field] = &[
    NAME,
    Field::new("type", Kind::Choice(TYPES), "What it is: an OpenAI-compatible server, Anthropic's API, or another runner of yours, whose shared models are reached through the hub.").required(),
    Field::text("url", "Where the server is, such as http://127.0.0.1:11434/v1 or https://api.anthropic.com."),
    Field::text("runner", "The runner, as the hub knows it: <owner>/<name>."),
    KEY,
    KEY_ENV,
    Field::new("max_concurrent", Kind::Number, "The most calls it takes at once, its own and those shared through the hub together: 1 for a model server on one GPU."),
    Field::new("timeout", Kind::Number, "Seconds one request may take; 300 when left out."),
    NO_CHECK,
];
const NAMED: &[Field] = &[NAME];
const SOME_SOURCE: &[Field] =
    &[Field::text("name", "One source; every one when left out.").positional()];
const KEY_FIELDS: &[Field] = &[
    NAME,
    Field::secret("key", "The source's new API key (empty takes its key out)"),
    KEY_ENV,
    NO_CHECK,
];
const LIST_MODELS: &[Field] =
    &[Field::text("source", "The models of this source alone.").positional()];
const ON: &[Field] = &[MODEL, TIER];
const ONE_MODEL: &[Field] = &[MODEL];
const SET: &[Field] = &[
    MODEL,
    TIER,
    Field::new(
        "context_window",
        Kind::Number,
        "Tokens of context its server runs it with.",
    ),
    Field::new(
        "tools",
        Kind::Choice(YES_NO),
        "Whether it makes tool calls.",
    ),
];

#[derive(Deserialize, Serialize)]
struct Add {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    url: Option<String>,
    runner: Option<String>,
    key: Option<String>,
    key_env: Option<String>,
    max_concurrent: Option<u32>,
    timeout: Option<u64>,
    #[serde(default)]
    no_check: bool,
}

#[derive(Deserialize, Serialize)]
struct Named {
    name: String,
}

#[derive(Deserialize, Serialize)]
struct SomeSource {
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
struct ListModels {
    source: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct On {
    model: String,
    tier: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct OneModel {
    model: String,
}

#[derive(Deserialize, Serialize)]
struct Set {
    model: String,
    tier: Option<String>,
    context_window: Option<u32>,
    tools: Option<String>,
}

#[derive(Deserialize)]
struct Nothing {}

/// An operation that changes the pool: `run` given the change it makes, for
/// the audit log.
fn changing<C: 'static, I>(
    spec: Spec,
    run: impl Fn(&C, I, Change) -> Result<Value, Error> + Send + Sync + 'static,
) -> Operation<C>
where
    I: serde::de::DeserializeOwned + Serialize,
{
    operation(spec, move |c: &C, caller: &Caller, input: I| {
        let change = Change::new(caller, &spec, &input);
        run(c, input, change)
    })
}

/// The operations on the pool of a hub or a runner, for those `access` lets.
pub fn operations<C: WithPool + 'static>(side: Side, access: Access) -> Vec<Operation<C>> {
    let spec = |name, summary, input| Spec {
        name,
        summary,
        access,
        input,
    };
    let mut all = vec![
        operation(
            spec(
                "source.list",
                "List the sources of the ANN pool: what each is, where, whether it has a key, and its models.",
                &[],
            ),
            move |c: &C, _: &Caller, _: Nothing| list(c, side),
        ),
        changing(
            spec(
                "source.add",
                "Add a source of models, once it answers: its key is kept for you, never written in the settings. Its models are off until you turn them on.",
                ADD,
            ),
            |c: &C, input: Add, change| add(c, input, change),
        ),
        changing(
            spec(
                "source.remove",
                "Take a source out, with its models and the key Ensemble kept for it; refused while something uses one of its models.",
                NAMED,
            ),
            |c: &C, input: Named, change| remove(c, &input.name, change),
        ),
        operation(
            spec(
                "source.test",
                "Ask the sources which models they serve.",
                SOME_SOURCE,
            ),
            |c: &C, _: &Caller, input: SomeSource| test(c, input.name.as_deref()),
        ),
        changing(
            spec(
                "source.key",
                "Give a source a new API key, or take its key out.",
                KEY_FIELDS,
            ),
            |c: &C, input: SetKey, change| set_key(c, input, change),
        ),
        changing(
            spec(
                "source.refresh",
                "Ask the sources again which models they serve: a new one is kept off, one no longer served is marked missing.",
                SOME_SOURCE,
            ),
            |c: &C, input: SomeSource, change| refresh(c, input.name.as_deref(), change),
        ),
        operation(
            spec(
                "model.list",
                "List the models of the ANN pool: what each is, and whether it is on.",
                LIST_MODELS,
            ),
            move |c: &C, _: &Caller, input: ListModels| {
                list_models(c, side, input.source.as_deref())
            },
        ),
        changing(
            spec(
                "model.on",
                "Turn a model on, so it may be used; one whose source did not say what it is is asked once.",
                ON,
            ),
            |c: &C, input: On, change| turn_on(c, input, change),
        ),
        changing(
            spec(
                "model.off",
                "Turn a model off; refused while something uses it.",
                ONE_MODEL,
            ),
            |c: &C, input: OneModel, change| turn_off(c, &input.model, change),
        ),
        changing(
            spec(
                "model.set",
                "Say how strong an LLM is, how much context its server runs it with, or whether it makes tool calls.",
                SET,
            ),
            |c: &C, input: Set, change| set(c, input, change),
        ),
    ];
    if side == Side::Runner {
        all.push(changing(
            spec(
                "model.share",
                "Offer a model of one of the runner's own servers through the hub, to the hub and your other runners.",
                ONE_MODEL,
            ),
            |c: &C, input: OneModel, change| share(c, &input.model, true, change),
        ));
        all.push(changing(
            spec(
                "model.unshare",
                "Stop offering a model through the hub.",
                ONE_MODEL,
            ),
            |c: &C, input: OneModel, change| share(c, &input.model, false, change),
        ));
    }
    all
}

/// Each source: what it is, where, whether it has a key, and its models.
fn list<C: WithPool>(c: &C, side: Side) -> Result<Value, Error> {
    let pool = c.pool()?;
    let kept = c.kept_pool()?;
    let mut rows = Vec::new();
    let mut said = Vec::new();
    for (name, s) in &kept.sources {
        let models: Vec<&PoolModel> = kept.models.iter().filter(|m| m.source == *name).collect();
        let on = models.iter().filter(|m| m.on).count();
        let counted = format!("{} model(s), {on} on", models.len());
        match s {
            SourceSetting::Runner { runner } => {
                said.push(format!(
                    "{name}  {RUNNER_SOURCE} {runner}, through the hub  ({counted})"
                ));
                rows.push(json!({
                    "name": name,
                    "type": RUNNER_SOURCE,
                    "runner": runner,
                    "models": models.len(),
                    "on": on,
                }));
            }
            _ => {
                let env = s.api_key_env().map(String::from);
                let key = pool
                    .direct
                    .get(name)
                    .and_then(|b| b.settings.api_key().map(String::from));
                let key_said = match (&env, &key) {
                    (None, _) => "no key".to_string(),
                    (Some(var), Some(_)) if *var == source_key_variable(name) => "key kept".into(),
                    (Some(var), Some(_)) => format!("key from ${var}"),
                    (Some(var), None) => format!("no key: ${var} is not set"),
                };
                let (url, most) = match s {
                    SourceSetting::OpenAiCompatible {
                        base_url,
                        max_concurrent,
                        ..
                    }
                    | SourceSetting::Anthropic {
                        base_url,
                        max_concurrent,
                        ..
                    } => (base_url.as_str(), *max_concurrent),
                    SourceSetting::Runner { .. } => ("", 0),
                };
                said.push(format!(
                    "{name}  {} at {url}  ({key_said}, {most} call(s) at once, {counted})",
                    s.type_id()
                ));
                rows.push(json!({
                    "name": name,
                    "type": s.type_id(),
                    "url": url,
                    "key": key.is_some(),
                    "key_env": env,
                    "max_concurrent": most,
                    "models": models.len(),
                    "on": on,
                }));
            }
        }
    }
    if said.is_empty() {
        said.push(format!(
            "no sources yet; `{} source add` adds one",
            side.program()
        ));
    }
    Ok(json!({ "said": said.join("\n"), "sources": rows }))
}

fn add<C: WithPool>(c: &C, input: Add, change: Change) -> Result<Value, Error> {
    let name = input.name.trim().to_string();
    check_name(&name)?;
    let kept = c.kept_pool()?;
    if kept.sources.contains_key(&name) {
        return Err(Error::new(
            Code::Conflict,
            format!("there is a source '{name}' already"),
        )
        .on("name")
        .hint("`source remove` it first, or choose another name"));
    }
    if !TYPES.contains(&input.kind.as_str()) {
        return Err(Error::invalid(format!(
            "'{}' is not a type of source: {}",
            input.kind,
            TYPES.join(", ")
        ))
        .on("type"));
    }
    let mut found = None;
    let mut keep = None;
    let setting = if input.kind == RUNNER_SOURCE {
        for (given, field) in [
            (input.url.is_some(), "url"),
            (input.key.is_some(), "key"),
            (input.key_env.is_some(), "key_env"),
            (input.max_concurrent.is_some(), "max_concurrent"),
            (input.timeout.is_some(), "timeout"),
        ] {
            if given {
                return Err(Error::invalid(format!(
                    "a source that is another runner takes no `{field}`: that runner keeps its own"
                ))
                .on(field));
            }
        }
        let runner =
            input.runner.as_deref().map(str::trim).ok_or_else(|| {
                Error::invalid("`runner` is required: <owner>/<name>").on("runner")
            })?;
        if !valid_runner_id(runner) {
            return Err(Error::invalid(format!(
                "'{runner}' is not a runner as the hub knows it: <owner>/<name>, such as ana/gpu-box"
            ))
            .on("runner"));
        }
        if !input.no_check {
            found = Some(c.shared_by(runner)?);
        }
        SourceSetting::Runner {
            runner: runner.to_string(),
        }
    } else {
        if input.runner.is_some() {
            return Err(
                Error::invalid("`runner` is for a source that is another runner").on("runner"),
            );
        }
        let url = input
            .url
            .as_deref()
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                Error::invalid(format!("`url` is required for an {} source", input.kind)).on("url")
            })?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(
                Error::invalid(format!("'{url}' must start with http:// or https://")).on("url"),
            );
        }
        let max_concurrent = match input.max_concurrent {
            Some(0) => {
                return Err(
                    Error::invalid("`max_concurrent` must be greater than 0").on("max_concurrent")
                );
            }
            Some(n) => n,
            None => {
                return Err(Error::invalid(
                    "`max_concurrent` is required: the most calls it takes at once, such as 1 for a model server on one GPU",
                )
                .on("max_concurrent"));
            }
        };
        if input.key.is_some() && input.key_env.is_some() {
            return Err(Error::invalid("give `key` or `key_env`, not both").on("key_env"));
        }
        let timeout = input.timeout.unwrap_or(TIMEOUT_SECONDS);
        if timeout == 0 {
            return Err(Error::invalid("`timeout` must be greater than 0").on("timeout"));
        }
        let key = given_key(c, input.key.as_deref(), input.key_env.as_deref())?;
        // Where a key given would go, before the source is asked anything.
        if input.key.as_deref().is_some_and(|k| !k.is_empty()) {
            c.can_keep_keys()?;
        }
        let source = source_of(&name, &input.kind, &url, timeout, key.clone());
        if input.kind == ANTHROPIC_API && key.is_none() && input.key.is_none() {
            return Err(Error::invalid("Anthropic's API wants an API key").on("key"));
        }
        if !input.no_check {
            found = Some(ask(&source, key.is_none())?);
        }
        let env = match (&input.key_env, &input.key) {
            (Some(var), _) => Some(var.trim().to_string()),
            (None, Some(k)) if !k.is_empty() => {
                let var = source_key_variable(&name);
                keep = Some((var.clone(), k.clone()));
                Some(var)
            }
            _ => None,
        };
        if input.kind == ANTHROPIC_API {
            SourceSetting::Anthropic {
                base_url: url,
                api_key_env: env,
                timeout_seconds: timeout,
                max_concurrent,
            }
        } else {
            SourceSetting::OpenAiCompatible {
                base_url: url,
                api_key_env: env,
                timeout_seconds: timeout,
                max_concurrent,
            }
        }
    };
    // The key first, so that the running process finds it when it uses the
    // source; taken out again when the source is not kept.
    if let Some((var, key)) = &keep {
        c.keep_key(var, Some(key))?;
    }
    let models = found.clone().unwrap_or_default();
    let kept = c.change_pool(change, &mut |p| {
        p.sources.insert(name.clone(), setting.clone());
        merge(p, &name, &models);
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
    let names: Vec<String> = models.iter().map(|m| format!("{name}:{}", m.id)).collect();
    let serves = match &found {
        Some(_) if names.is_empty() => ", which serves no models yet".to_string(),
        Some(_) => format!(
            ", which serves {}; they are off until you turn them on, as `model on {}`",
            listed(&names),
            names[0]
        ),
        None => String::new(),
    };
    Ok(json!({
        "said": format!("added source {name}{serves}"),
        "name": name,
        "models": names,
        "note": note,
    }))
}

fn remove<C: WithPool>(c: &C, name: &str, change: Change) -> Result<Value, Error> {
    let Some(was) = c.kept_pool()?.sources.remove(name) else {
        return Err(Error::not_found(format!("there is no source '{name}'")).on("name"));
    };
    let ours = source_key_variable(name);
    let kept = (was.api_key_env() == Some(ours.as_str())).then_some(ours);
    let note = c.change_pool(change, &mut |p| {
        p.sources.remove(name);
        p.models.retain(|m| m.source != name);
        Ok(())
    })?;
    if let Some(var) = kept
        && c.can_keep_keys().is_ok()
    {
        c.keep_key(&var, None)?;
    }
    Ok(json!({ "said": format!("removed source {name}, with its models"), "note": note }))
}

fn test<C: WithPool>(c: &C, name: Option<&str>) -> Result<Value, Error> {
    let pool = c.pool()?;
    if let Some(name) = name
        && !pool.direct.contains_key(name)
        && !pool.relayed.contains_key(name)
    {
        return Err(Error::not_found(format!("there is no source '{name}'")).on("name"));
    }
    let mut said = Vec::new();
    let mut rows = Vec::new();
    for (n, b) in &pool.direct {
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
    for (n, runner) in &pool.relayed {
        if name.is_some_and(|w| w != n) {
            continue;
        }
        match c.shared_by(runner) {
            Ok(found) => {
                let ids: Vec<String> = found.into_iter().map(|f| f.id).collect();
                said.push(format!("{n}: {runner} shares {}", listed(&ids)));
                rows.push(json!({ "name": n, "ok": true, "models": ids }));
            }
            Err(e) => {
                said.push(format!("{n}: {}", e.message));
                rows.push(json!({ "name": n, "ok": false, "error": e.message }));
            }
        }
    }
    if said.is_empty() {
        said.push("no sources yet".into());
    }
    Ok(json!({ "said": said.join("\n"), "sources": rows }))
}

fn set_key<C: WithPool>(c: &C, input: SetKey, change: Change) -> Result<Value, Error> {
    let name = input.name.as_str();
    let pool = c.pool()?;
    let Some(source) = pool.direct.get(name) else {
        let message = if pool.relayed.contains_key(name) {
            format!("'{name}' is another runner, which keeps its own keys")
        } else {
            format!("there is no source '{name}'")
        };
        return Err(Error::not_found(message).on("name"));
    };
    let now = c
        .kept_pool()?
        .sources
        .get(name)
        .and_then(SourceSetting::api_key_env)
        .map(String::from);
    let ours = source_key_variable(name);
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
    // Whether the key can be kept, before the source is asked anything.
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
            settings: with_key(&source.settings, key.clone()),
            ..source.clone()
        };
        let found = ask(&tried, false)?;
        models = Some(found.into_iter().map(|f| f.id).collect::<Vec<_>>());
    }
    if let Some(k) = &keep {
        c.keep_key(&ours, Some(k))?;
    }
    let note = if env != now {
        c.change_pool(change, &mut |p| {
            match p.sources.get_mut(name) {
                Some(
                    SourceSetting::OpenAiCompatible { api_key_env, .. }
                    | SourceSetting::Anthropic { api_key_env, .. },
                ) => *api_key_env = env.clone(),
                _ => return Err(Error::not_found(format!("there is no source '{name}'"))),
            }
            Ok(())
        })?
    } else {
        c.pool_changed()
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

/// Ask the sources (`name` alone, or every one) which models they serve,
/// and keep what changed with `change`: a model listed newly, off; one no
/// longer listed, missing.
fn refresh<C: WithPool>(c: &C, name: Option<&str>, change: Change) -> Result<Value, Error> {
    let kept = c.kept_pool()?;
    if let Some(name) = name
        && !kept.sources.contains_key(name)
    {
        return Err(Error::not_found(format!("there is no source '{name}'")).on("name"));
    }
    let pool = c.pool()?;
    let mut said = Vec::new();
    let mut answers: Vec<(String, Vec<Found>)> = Vec::new();
    for (n, s) in &kept.sources {
        if name.is_some_and(|w| w != n) {
            continue;
        }
        let asked = match s {
            SourceSetting::Runner { runner } => c.shared_by(runner),
            _ => match pool.direct.get(n) {
                Some(source) => ask(source, false),
                None => Err(Error::new(Code::Unavailable, "it is not usable now")),
            },
        };
        match asked {
            Ok(found) => answers.push((n.clone(), found)),
            Err(e) => said.push(format!("{n}: could not ask it: {}", e.message)),
        }
    }
    let mut now = kept.clone();
    for (n, found) in &answers {
        let before = now.clone();
        merge(&mut now, n, found);
        let new = now
            .models
            .iter()
            .filter(|m| m.source == *n && before.model(n, &m.id).is_none())
            .count();
        let gone = now
            .models
            .iter()
            .filter(|m| {
                m.source == *n && m.missing && !before.model(n, &m.id).is_some_and(|b| b.missing)
            })
            .count();
        said.push(match (new, gone) {
            (0, 0) => format!("{n}: no change"),
            (new, 0) => format!("{n}: {new} new model(s), off until you turn them on"),
            (0, gone) => format!("{n}: {gone} model(s) no longer served, marked missing"),
            (new, gone) => format!(
                "{n}: {new} new model(s), off until you turn them on; {gone} no longer served, marked missing"
            ),
        });
    }
    let note = if now != kept {
        c.change_pool(change, &mut |p| {
            for (n, found) in &answers {
                if p.sources.contains_key(n) {
                    merge(p, n, found);
                }
            }
            Ok(())
        })?
    } else {
        None
    };
    if said.is_empty() {
        said.push("no sources yet".into());
    }
    Ok(json!({ "said": said.join("\n"), "note": note }))
}

/// Ask every source which models it serves, as the process starts, and keep
/// what changed; say what was found, or why nothing was asked.
pub fn refresh_all<C: WithPool>(c: &C) -> String {
    match refresh(c, None, Change::by_this_computer("source.refresh")) {
        Ok(out) => out["said"].as_str().unwrap_or_default().to_string(),
        Err(e) => e.message,
    }
}

/// Keep in `pool` what `source` answered it serves now: a model new to it,
/// off; one it serves again, no longer missing, and known now if it was not;
/// one it no longer serves, missing. The models stay in the order of their
/// sources and ids.
fn merge(pool: &mut AnnPoolSetting, source: &str, found: &[Found]) {
    for f in found {
        match pool.model_mut(source, &f.id) {
            Some(m) => {
                m.missing = false;
                if m.kind == ModelKind::Unknown {
                    m.kind = f.kind;
                }
            }
            None => pool.models.push(PoolModel {
                source: source.to_string(),
                id: f.id.clone(),
                kind: f.kind,
                on: false,
                missing: false,
                shared: false,
            }),
        }
    }
    for m in pool.models.iter_mut().filter(|m| m.source == source) {
        if !found.iter().any(|f| f.id == m.id) {
            m.missing = true;
        }
    }
    pool.models
        .sort_by(|a, b| (&a.source, &a.id).cmp(&(&b.source, &b.id)));
}

/// Each model, or those of `source`: what it is, and whether it is on.
fn list_models<C: WithPool>(c: &C, side: Side, source: Option<&str>) -> Result<Value, Error> {
    let kept = c.kept_pool()?;
    if let Some(s) = source
        && !kept.sources.contains_key(s)
    {
        return Err(Error::not_found(format!("there is no source '{s}'")).on("source"));
    }
    let mut said = Vec::new();
    let mut rows = Vec::new();
    for m in kept
        .models
        .iter()
        .filter(|m| source.is_none_or(|s| m.source == s))
    {
        let mut state = vec![if m.on { "on" } else { "off" }];
        if m.missing {
            state.push("missing");
        }
        if m.shared {
            state.push("shared through the hub");
        }
        said.push(format!(
            "{}  {}  ({})",
            m.name(),
            m.kind.describe(),
            state.join(", ")
        ));
        let mut row = json!({
            "name": m.name(),
            "source": m.source,
            "id": m.id,
            "type": m.kind.type_id(),
            "on": m.on,
            "missing": m.missing,
            "shared": m.shared,
        });
        match m.kind {
            ModelKind::Llm {
                tier,
                context_window,
                tools,
            } => {
                row["tier"] = json!(tier.as_str());
                row["context_window"] = json!(context_window);
                row["tools"] = json!(tools);
            }
            ModelKind::Embedding { dimensions } => row["dimensions"] = json!(dimensions),
            ModelKind::Unknown => {}
        }
        rows.push(row);
    }
    if said.is_empty() {
        said.push(match source {
            Some(s) => format!("{s} serves no models; `source refresh {s}` asks it again"),
            None => format!(
                "no models yet; `{} source add` adds a source, and its models with it",
                side.program()
            ),
        });
    }
    Ok(json!({ "said": said.join("\n"), "models": rows }))
}

/// The model `given` names, `<source>:<id>`, as kept.
fn model_of<'a>(pool: &'a AnnPoolSetting, given: &str) -> Result<&'a PoolModel, Error> {
    let (source, id) = split_name(given).ok_or_else(|| {
        Error::invalid(format!(
            "'{given}' must be <source>:<model>, such as local:qwen3.5:9b"
        ))
        .on("model")
    })?;
    pool.model(source, id).ok_or_else(|| {
        Error::not_found(format!("there is no model '{source}:{id}'"))
            .on("model")
            .hint("`model list` lists them; `source refresh` asks the sources again")
    })
}

fn tier_of(given: Option<&str>) -> Result<Option<Tier>, Error> {
    given
        .map(|t| {
            Tier::parse(t).ok_or_else(|| {
                Error::invalid(format!("'{t}' is not a tier: {}", Tier::NAMES.join(", ")))
                    .on("tier")
            })
        })
        .transpose()
}

fn turn_on<C: WithPool>(c: &C, input: On, change: Change) -> Result<Value, Error> {
    let kept = c.kept_pool()?;
    let model = model_of(&kept, &input.model)?.clone();
    let tier = tier_of(input.tier.as_deref())?;
    let mut kind = model.kind;
    if kind == ModelKind::Unknown {
        kind = asked_kind(c, &model)?;
    }
    if let Some(t) = tier {
        match &mut kind {
            ModelKind::Llm { tier, .. } => *tier = t,
            other => {
                return Err(Error::invalid(format!(
                    "{} is not an LLM: it is {}",
                    model.name(),
                    other.describe()
                ))
                .on("tier"));
            }
        }
    }
    if model.on && kind == model.kind {
        return Ok(json!({ "said": format!("{} is on already", model.name()) }));
    }
    let note = c.change_pool(change, &mut |p| {
        if let Some(m) = p.model_mut(&model.source, &model.id) {
            m.kind = kind;
            m.on = true;
        }
        Ok(())
    })?;
    Ok(json!({
        "said": format!("{} is on: {}", model.name(), kind.describe()),
        "note": note,
    }))
}

/// What `model`, whose source did not say, is: an embedding model when its
/// source embeds a text with it, else an LLM.
fn asked_kind<C: WithPool>(c: &C, model: &PoolModel) -> Result<ModelKind, Error> {
    let pool = c.pool()?;
    let Some(source) = pool.direct.get(&model.source) else {
        return Err(Error::new(
            Code::Unavailable,
            format!(
                "what {} is is not known, and only the runner that shares it can ask",
                model.name()
            ),
        ));
    };
    if matches!(source.settings, BackendSettings::Anthropic { .. }) {
        return Ok(llm(&model.id));
    }
    Ok(
        match crate::embed::embed(source, &model.id, &["check".to_string()]) {
            Ok(vectors) => match vectors.first().map(Vec::len) {
                Some(n) if n > 0 => ModelKind::Embedding {
                    dimensions: u32::try_from(n).unwrap_or(u32::MAX),
                },
                _ => llm(&model.id),
            },
            Err(_) => llm(&model.id),
        },
    )
}

fn turn_off<C: WithPool>(c: &C, given: &str, change: Change) -> Result<Value, Error> {
    let kept = c.kept_pool()?;
    let model = model_of(&kept, given)?.clone();
    if !model.on {
        return Ok(json!({ "said": format!("{} is off already", model.name()) }));
    }
    let note = c.change_pool(change, &mut |p| {
        if let Some(m) = p.model_mut(&model.source, &model.id) {
            m.on = false;
            m.shared = false;
        }
        Ok(())
    })?;
    Ok(json!({ "said": format!("{} is off", model.name()), "note": note }))
}

fn set<C: WithPool>(c: &C, input: Set, change: Change) -> Result<Value, Error> {
    let kept = c.kept_pool()?;
    let model = model_of(&kept, &input.model)?.clone();
    let ModelKind::Llm {
        tier,
        context_window,
        tools,
    } = model.kind
    else {
        return Err(Error::invalid(format!(
            "{} is not an LLM: it is {}",
            model.name(),
            model.kind.describe()
        ))
        .on("model"));
    };
    if input.context_window == Some(0) {
        return Err(Error::invalid("`context_window` must be greater than 0").on("context_window"));
    }
    let tools_now = match input.tools.as_deref().map(str::trim) {
        None => tools,
        Some("yes") => Some(true),
        Some("no") => Some(false),
        Some(other) => {
            return Err(Error::invalid(format!("'{other}' must be yes or no")).on("tools"));
        }
    };
    let kind = ModelKind::Llm {
        tier: tier_of(input.tier.as_deref())?.unwrap_or(tier),
        context_window: input.context_window.or(context_window),
        tools: tools_now,
    };
    if kind == model.kind {
        return Ok(json!({ "said": format!("{} is as it was", model.name()) }));
    }
    let note = c.change_pool(change, &mut |p| {
        if let Some(m) = p.model_mut(&model.source, &model.id) {
            m.kind = kind;
        }
        Ok(())
    })?;
    Ok(json!({
        "said": format!("{} is {}", model.name(), kind.describe()),
        "note": note,
    }))
}

fn share<C: WithPool>(c: &C, given: &str, on: bool, change: Change) -> Result<Value, Error> {
    let kept = c.kept_pool()?;
    let model = model_of(&kept, given)?.clone();
    if model.shared == on {
        let state = if on {
            "is shared already"
        } else {
            "is not shared"
        };
        return Ok(json!({ "said": format!("{} {state}", model.name()) }));
    }
    if on && !model.on {
        return Err(Error::invalid(format!(
            "{} is off; turn it on first with `model on {}`",
            model.name(),
            model.name()
        ))
        .on("model"));
    }
    if on
        && !matches!(
            kept.sources.get(&model.source),
            Some(SourceSetting::OpenAiCompatible { .. })
        )
    {
        return Err(Error::invalid(format!(
            "{} is not of an {OPENAI_COMPATIBLE_API} source of the runner's own, the one kind a runner shares",
            model.name()
        ))
        .on("model"));
    }
    let note = c.change_pool(change, &mut |p| {
        if let Some(m) = p.model_mut(&model.source, &model.id) {
            m.shared = on;
        }
        Ok(())
    })?;
    let state = if on {
        "is shared through the hub"
    } else {
        "is no longer shared"
    };
    Ok(json!({ "said": format!("{} {state}", model.name()), "note": note }))
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

/// The key a source is tried with: the one given, or the value of the
/// variable named.
fn given_key<C: WithPool>(
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

fn source_of(name: &str, kind: &str, url: &str, timeout: u64, key: Option<String>) -> LlmBackend {
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

/// The models `source` serves, each with what it is, or why it cannot be
/// added: one that wants a key it was not given has an interface ask for it,
/// when `may_ask`.
fn ask(source: &LlmBackend, may_ask: bool) -> Result<Vec<Found>, Error> {
    use ensemble_core::port::Error as Port;
    discover(source).map_err(|e| match e {
        Port::Api {
            status: 401 | 403, ..
        } if may_ask => {
            Error::invalid(format!("{} wants an API key", source.settings.base_url())).on("key")
        }
        Port::Api {
            status: 401 | 403, ..
        } => Error::invalid(format!(
            "{} did not accept the key: {e}",
            source.settings.base_url()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn found(id: &str, kind: ModelKind) -> Found {
        Found {
            id: id.into(),
            kind,
        }
    }

    #[test]
    fn a_model_listed_newly_is_off_and_one_no_longer_listed_is_missing() {
        let mut pool = AnnPoolSetting::default();
        let small = ModelKind::Llm {
            tier: Tier::Small,
            context_window: None,
            tools: None,
        };
        merge(
            &mut pool,
            "local",
            &[found("b", small), found("a", ModelKind::Unknown)],
        );
        assert_eq!(
            pool.models.iter().map(PoolModel::name).collect::<Vec<_>>(),
            ["local:a", "local:b"],
            "in the order of their ids"
        );
        assert!(pool.models.iter().all(|m| !m.on && !m.missing));
        pool.model_mut("local", "b").unwrap().on = true;
        // `b` is gone, `a` is known now.
        let embedding = ModelKind::Embedding { dimensions: 8 };
        merge(&mut pool, "local", &[found("a", embedding)]);
        let b = pool.model("local", "b").unwrap();
        assert!(b.missing && b.on, "missing, and on as it was");
        assert_eq!(pool.model("local", "a").unwrap().kind, embedding);
        // Back again, and what a person said of it is kept.
        merge(&mut pool, "local", &[found("b", ModelKind::Unknown)]);
        let b = pool.model("local", "b").unwrap();
        assert!(!b.missing);
        assert_eq!(b.kind, small);
    }
}
