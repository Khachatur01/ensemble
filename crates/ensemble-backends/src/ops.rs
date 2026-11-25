//! The operations on the ANN pool a hub or a runner keeps in its settings,
//! for every interface to reach: its providers, `provider.list`, `provider.add`,
//! `provider.remove`, `provider.test`, `provider.key` and `provider.refresh`, and
//! the models a person added of them, `model.list`, `model.add`,
//! `model.remove` and `model.set`, and, for a runner, `model.share` and
//! `model.unshare`; and what a provider serves now, `provider.models`.
//!
//! A person gives what a provider is and its key, never the name of a
//! variable: the key is kept under a variable Ensemble names after the
//! provider ([`provider_key_variable`](ensemble_config::provider_key_variable)),
//! and the provider names that variable. Where it is kept is the context's
//! ([`WithPool::keep_key`]): sealed in its database. One who keeps the key
//! elsewhere already, such as a secrets manager that sets a variable, names
//! that variable instead (`key_env`).
//!
//! Nothing a provider serves is in the pool until a person adds it: a model
//! is added by its name, once its provider lists it, as what the provider
//! says it is, which the person may correct. A provider is asked which
//! models it serves when it is added, so a wrong address or key is said at
//! once and an answer that wants a key has an interface ask for it; on
//! `provider.refresh`, and when the process starts ([`refresh_all`]), it is
//! asked again only to mark a model added that it no longer lists missing,
//! never forgotten, and one it lists again no longer missing.

use crate::models::{Found, discover, llm};
use ensemble_config::ann::{RUNNER_PROVIDER, split_name, valid_runner_id};
use ensemble_config::kept::Change;
use ensemble_config::{
    ANTHROPIC_API, AnnPool, AnnPoolSetting, BackendSettings, DECISION_API, LlmBackend, ModelKind,
    OPENAI_COMPATIBLE_API, PoolModel, ProviderSetting, Tier, provider_key_variable,
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
    /// not, as when a key one of its providers names did; say what it did.
    fn pool_changed(&self) -> Option<String>;

    /// The pool as it is now, with its keys.
    fn pool(&self) -> Result<AnnPool, Error>;

    /// The value of the variable `name`, wherever it looks for one.
    fn variable(&self, name: &str) -> Option<String>;

    /// The models the runner `runner` shares, as the hub knows them now:
    /// what a provider that is that runner serves. Why they cannot be asked
    /// for now, if they cannot.
    fn shared_by(&self, runner: &str) -> Result<Vec<Found>, Error>;
}

/// Whose pool it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Hub,
    Runner,
}

const TYPES: &[&str] = &[
    OPENAI_COMPATIBLE_API,
    ANTHROPIC_API,
    DECISION_API,
    RUNNER_PROVIDER,
];
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
    "The provider's name, as a model of it is named, <provider>:<model>: letters, digits, '-' or '_'.",
)
.required()
.positional();
const MODEL: Field = Field::text(
    "model",
    "The model, as <provider>:<model>, such as local:qwen3.5:9b.",
)
.required()
.positional();
const KEY: Field = Field::secret("key", "The provider's API key (empty for none)");
const KEY_ENV: Field = Field::text(
    "key_env",
    "A variable that holds the key already, such as one a secrets manager sets, in place of a key Ensemble keeps.",
);
const NO_CHECK: Field = Field::new(
    "no_check",
    Kind::Flag,
    "Do not ask the provider which models it serves first.",
);
const TIER: Field = Field::new(
    "tier",
    Kind::Choice(Tier::NAMES),
    "How strong the LLM is, for choosing one that is enough.",
);
const ABILITIES: Field = Field::text(
    "abilities",
    "What the LLM is good and bad at, in your words, such as \"strong at Rust, weak at UI, slow\": what the hub chooses a model for a task by. Empty takes it away.",
);
const ADD: &[Field] = &[
    NAME,
    Field::new("type", Kind::Choice(TYPES), "What it is: an OpenAI-compatible server, Anthropic's API, a decision model's API (TypeSafe's Jev, or Laya's laya-serve; a hub's alone), or another runner of yours, whose shared models are reached through the hub.").required(),
    Field::text("url", "Where the server is, such as http://127.0.0.1:11434/v1, https://api.anthropic.com or https://api.typesafe.ai."),
    Field::text("runner", "The runner, as the hub knows it: <owner>/<name>."),
    KEY,
    KEY_ENV,
    Field::new("max_concurrent", Kind::Number, "The most calls it takes at once, its own and those shared through the hub together: 1 for a model server on one GPU."),
    Field::new("timeout", Kind::Number, "Seconds one request may take; 300 when left out."),
    NO_CHECK,
];
const NAMED: &[Field] = &[NAME];
const SOME_SOURCE: &[Field] =
    &[Field::text("name", "One provider; every one when left out.").positional()];
const KEY_FIELDS: &[Field] = &[
    NAME,
    Field::secret(
        "key",
        "The provider's new API key (empty takes its key out)",
    ),
    KEY_ENV,
    NO_CHECK,
];
const LIST_MODELS: &[Field] =
    &[Field::text("provider", "The models of this provider alone.").positional()];
const ADD_MODEL: &[Field] = &[
    MODEL,
    TIER,
    Field::new(
        "context_window",
        Kind::Number,
        "Tokens of context its server runs it with, when the provider does not say.",
    ),
    Field::new(
        "tools",
        Kind::Choice(YES_NO),
        "Whether it makes tool calls, when the provider does not say.",
    ),
    ABILITIES,
];
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
    ABILITIES,
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
    provider: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct AddModel {
    model: String,
    tier: Option<String>,
    context_window: Option<u32>,
    tools: Option<String>,
    abilities: Option<String>,
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
    abilities: Option<String>,
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
                "provider.list",
                "List the providers of the ANN pool: what each is, where, whether it has a key, and its models.",
                &[],
            ),
            move |c: &C, _: &Caller, _: Nothing| list(c, side),
        ),
        changing(
            spec(
                "provider.add",
                "Add a provider of models, once it answers: its key is kept for you, never written in the settings. None of its models is in the pool until you add it.",
                ADD,
            ),
            move |c: &C, input: Add, change| add(c, side, input, change),
        ),
        changing(
            spec(
                "provider.remove",
                "Take a provider out, with its models and the key Ensemble kept for it; refused while something uses one of its models.",
                NAMED,
            ),
            |c: &C, input: Named, change| remove(c, &input.name, change),
        ),
        operation(
            spec(
                "provider.test",
                "Ask the providers which models they serve.",
                SOME_SOURCE,
            ),
            |c: &C, _: &Caller, input: SomeSource| test(c, input.name.as_deref()),
        ),
        operation(
            spec(
                "provider.models",
                "List what a provider serves now, and what each is as far as it says; nothing of it is kept, and a model is in the pool only once you add it.",
                NAMED,
            ),
            |c: &C, _: &Caller, input: Named| served(c, &input.name),
        ),
        changing(
            spec(
                "provider.key",
                "Give a provider a new API key, or take its key out.",
                KEY_FIELDS,
            ),
            |c: &C, input: SetKey, change| set_key(c, input, change),
        ),
        changing(
            spec(
                "provider.refresh",
                "Ask the providers again which models they serve: a model added that one no longer serves is marked missing. Nothing is added.",
                SOME_SOURCE,
            ),
            |c: &C, input: SomeSource, change| refresh(c, input.name.as_deref(), change),
        ),
        operation(
            spec(
                "model.list",
                "List the models of the ANN pool, those you added: what each is, and whether its provider still serves it.",
                LIST_MODELS,
            ),
            move |c: &C, _: &Caller, input: ListModels| {
                list_models(c, side, input.provider.as_deref())
            },
        ),
        changing(
            spec(
                "model.add",
                "Add a model a provider serves to the pool, as the provider says it is: an LLM or an embedding model. One whose provider does not say is asked once.",
                ADD_MODEL,
            ),
            |c: &C, input: AddModel, change| add_model(c, input, change),
        ),
        changing(
            spec(
                "model.remove",
                "Take a model out of the pool; refused while something uses it.",
                ONE_MODEL,
            ),
            |c: &C, input: OneModel, change| remove_model(c, &input.model, change),
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

/// Each provider: what it is, where, whether it has a key, and its models.
fn list<C: WithPool>(c: &C, side: Side) -> Result<Value, Error> {
    let pool = c.pool()?;
    let kept = c.kept_pool()?;
    let mut rows = Vec::new();
    let mut said = Vec::new();
    for (name, s) in &kept.providers {
        let models: Vec<&PoolModel> = kept.models.iter().filter(|m| m.provider == *name).collect();
        let counted = format!("{} model(s) added", models.len());
        match s {
            ProviderSetting::Runner { runner } => {
                said.push(format!(
                    "{name}  {RUNNER_PROVIDER} {runner}, through the hub  ({counted})"
                ));
                rows.push(json!({
                    "name": name,
                    "type": RUNNER_PROVIDER,
                    "runner": runner,
                    "models": models.len(),
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
                    (Some(var), Some(_)) if *var == provider_key_variable(name) => {
                        "key kept".into()
                    }
                    (Some(var), Some(_)) => format!("key from ${var}"),
                    (Some(var), None) => format!("no key: ${var} is not set"),
                };
                let (url, most) = match s {
                    ProviderSetting::OpenAiCompatible {
                        base_url,
                        max_concurrent,
                        ..
                    }
                    | ProviderSetting::Anthropic {
                        base_url,
                        max_concurrent,
                        ..
                    }
                    | ProviderSetting::Decision {
                        base_url,
                        max_concurrent,
                        ..
                    } => (base_url.as_str(), *max_concurrent),
                    ProviderSetting::Runner { .. } => ("", 0),
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
                }));
            }
        }
    }
    if said.is_empty() {
        said.push(format!(
            "no providers yet; `{} provider add` adds one",
            side.program()
        ));
    }
    Ok(json!({ "said": said.join("\n"), "providers": rows }))
}

fn add<C: WithPool>(c: &C, side: Side, input: Add, change: Change) -> Result<Value, Error> {
    let name = input.name.trim().to_string();
    check_name(&name)?;
    if input.kind == DECISION_API && side == Side::Runner {
        return Err(Error::invalid(
            "a decision model's API is a provider of the hub's pool alone: the hub decides what works on a task",
        )
        .on("type"));
    }
    let kept = c.kept_pool()?;
    if kept.providers.contains_key(&name) {
        return Err(Error::new(
            Code::Conflict,
            format!("there is a provider '{name}' already"),
        )
        .on("name")
        .hint("`provider remove` it first, or choose another name"));
    }
    if !TYPES.contains(&input.kind.as_str()) {
        return Err(Error::invalid(format!(
            "'{}' is not a type of provider: {}",
            input.kind,
            TYPES.join(", ")
        ))
        .on("type"));
    }
    let mut found = None;
    let mut keep = None;
    let setting = if input.kind == RUNNER_PROVIDER {
        for (given, field) in [
            (input.url.is_some(), "url"),
            (input.key.is_some(), "key"),
            (input.key_env.is_some(), "key_env"),
            (input.max_concurrent.is_some(), "max_concurrent"),
            (input.timeout.is_some(), "timeout"),
        ] {
            if given {
                return Err(Error::invalid(format!(
                    "a provider that is another runner takes no `{field}`: that runner keeps its own"
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
        ProviderSetting::Runner {
            runner: runner.to_string(),
        }
    } else {
        if input.runner.is_some() {
            return Err(
                Error::invalid("`runner` is for a provider that is another runner").on("runner"),
            );
        }
        let url = input
            .url
            .as_deref()
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                Error::invalid(format!("`url` is required for an {} provider", input.kind))
                    .on("url")
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
        // Where a key given would go, before the provider is asked anything.
        if input.key.as_deref().is_some_and(|k| !k.is_empty()) {
            c.can_keep_keys()?;
        }
        let provider = source_of(&name, &input.kind, &url, timeout, key.clone());
        if input.kind == ANTHROPIC_API && key.is_none() && input.key.is_none() {
            return Err(Error::invalid("Anthropic's API wants an API key").on("key"));
        }
        if !input.no_check {
            found = Some(ask(&provider, key.is_none())?);
        }
        let env = match (&input.key_env, &input.key) {
            (Some(var), _) => Some(var.trim().to_string()),
            (None, Some(k)) if !k.is_empty() => {
                let var = provider_key_variable(&name);
                keep = Some((var.clone(), k.clone()));
                Some(var)
            }
            _ => None,
        };
        if input.kind == ANTHROPIC_API {
            ProviderSetting::Anthropic {
                base_url: url,
                api_key_env: env,
                timeout_seconds: timeout,
                max_concurrent,
            }
        } else if input.kind == DECISION_API {
            ProviderSetting::Decision {
                base_url: url,
                api_key_env: env,
                timeout_seconds: timeout,
                max_concurrent,
            }
        } else {
            ProviderSetting::OpenAiCompatible {
                base_url: url,
                api_key_env: env,
                timeout_seconds: timeout,
                max_concurrent,
            }
        }
    };
    // The key first, so that the running process finds it when it uses the
    // provider; taken out again when the provider is not kept.
    if let Some((var, key)) = &keep {
        c.keep_key(var, Some(key))?;
    }
    let kept = c.change_pool(change, &mut |p| {
        p.providers.insert(name.clone(), setting.clone());
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
    let names: Vec<String> = found
        .iter()
        .flatten()
        .map(|m| format!("{name}:{}", m.id))
        .collect();
    let serves = match &found {
        Some(_) if names.is_empty() => ", which serves no models yet".to_string(),
        Some(_) => format!(
            ", which serves {}; add the ones to use, as `model add {}`",
            listed(&names),
            names[0]
        ),
        None => String::new(),
    };
    Ok(json!({
        "said": format!("added provider {name}{serves}"),
        "name": name,
        "models": names,
        "note": note,
    }))
}

fn remove<C: WithPool>(c: &C, name: &str, change: Change) -> Result<Value, Error> {
    let Some(was) = c.kept_pool()?.providers.remove(name) else {
        return Err(Error::not_found(format!("there is no provider '{name}'")).on("name"));
    };
    let ours = provider_key_variable(name);
    let kept = (was.api_key_env() == Some(ours.as_str())).then_some(ours);
    let note = c.change_pool(change, &mut |p| {
        p.providers.remove(name);
        p.models.retain(|m| m.provider != name);
        Ok(())
    })?;
    if let Some(var) = kept
        && c.can_keep_keys().is_ok()
    {
        c.keep_key(&var, None)?;
    }
    Ok(
        json!({ "said": format!("removed provider {name}, with the models added of it"), "note": note }),
    )
}

fn test<C: WithPool>(c: &C, name: Option<&str>) -> Result<Value, Error> {
    let pool = c.pool()?;
    if let Some(name) = name
        && !pool.direct.contains_key(name)
        && !pool.relayed.contains_key(name)
    {
        return Err(Error::not_found(format!("there is no provider '{name}'")).on("name"));
    }
    let mut said = Vec::new();
    let mut rows = Vec::new();
    for (n, b) in &pool.direct {
        if name.is_some_and(|w| w != n) {
            continue;
        }
        match crate::models::models(b).and_then(|m| asks(b, &m).map(|()| m)) {
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
        said.push("no providers yet".into());
    }
    Ok(json!({ "said": said.join("\n"), "providers": rows }))
}

/// For a decision model's API, that it answers a question, a yes or no, with
/// the first of the `models` it serves; nothing for any other provider.
fn asks(provider: &LlmBackend, models: &[String]) -> Result<(), ensemble_core::port::Error> {
    let (BackendSettings::Decision { .. }, Some(model)) = (&provider.settings, models.first())
    else {
        return Ok(());
    };
    let questions = std::collections::BTreeMap::from([(
        "answers".to_string(),
        crate::decide::Question::Noul {
            instructions: "Is this a request to check that you answer?".into(),
        },
    )]);
    let state = json!("Ensemble checks that this provider answers questions.");
    crate::decide::decide(provider, model, &state, &questions).map(|_| ())
}

fn set_key<C: WithPool>(c: &C, input: SetKey, change: Change) -> Result<Value, Error> {
    let name = input.name.as_str();
    let pool = c.pool()?;
    let Some(provider) = pool.direct.get(name) else {
        let message = if pool.relayed.contains_key(name) {
            format!("'{name}' is another runner, which keeps its own keys")
        } else {
            format!("there is no provider '{name}'")
        };
        return Err(Error::not_found(message).on("name"));
    };
    let now = c
        .kept_pool()?
        .providers
        .get(name)
        .and_then(ProviderSetting::api_key_env)
        .map(String::from);
    let ours = provider_key_variable(name);
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
    // Whether the key can be kept, before the provider is asked anything.
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
            settings: with_key(&provider.settings, key.clone()),
            ..provider.clone()
        };
        let found = ask(&tried, false)?;
        models = Some(found.into_iter().map(|f| f.id).collect::<Vec<_>>());
    }
    if let Some(k) = &keep {
        c.keep_key(&ours, Some(k))?;
    }
    let note = if env != now {
        c.change_pool(change, &mut |p| {
            match p.providers.get_mut(name) {
                Some(
                    ProviderSetting::OpenAiCompatible { api_key_env, .. }
                    | ProviderSetting::Anthropic { api_key_env, .. },
                ) => *api_key_env = env.clone(),
                _ => return Err(Error::not_found(format!("there is no provider '{name}'"))),
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

/// Ask the providers (`name` alone, or every one) which models they serve,
/// and keep what changed of the models added with `change`: one no longer
/// listed, missing; one listed again, no longer missing.
fn refresh<C: WithPool>(c: &C, name: Option<&str>, change: Change) -> Result<Value, Error> {
    let kept = c.kept_pool()?;
    if let Some(name) = name
        && !kept.providers.contains_key(name)
    {
        return Err(Error::not_found(format!("there is no provider '{name}'")).on("name"));
    }
    let pool = c.pool()?;
    let mut said = Vec::new();
    let mut answers: Vec<(String, Vec<Found>)> = Vec::new();
    for (n, s) in &kept.providers {
        if name.is_some_and(|w| w != n) {
            continue;
        }
        let asked = match s {
            ProviderSetting::Runner { runner } => c.shared_by(runner),
            _ => match pool.direct.get(n) {
                Some(provider) => ask(provider, false),
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
        mark(&mut now, n, found);
        let changed = |gone: bool| {
            now.models
                .iter()
                .filter(|m| {
                    m.provider == *n
                        && m.missing == gone
                        && before.model(n, &m.id).is_some_and(|b| b.missing != gone)
                })
                .count()
        };
        said.push(match (changed(true), changed(false)) {
            (0, 0) => format!("{n}: no change"),
            (gone, 0) => format!("{n}: {gone} model(s) no longer served, marked missing"),
            (0, back) => format!("{n}: {back} model(s) served again"),
            (gone, back) => format!(
                "{n}: {gone} model(s) no longer served, marked missing; {back} served again"
            ),
        });
    }
    let note = if now != kept {
        c.change_pool(change, &mut |p| {
            for (n, found) in &answers {
                if p.providers.contains_key(n) {
                    mark(p, n, found);
                }
            }
            Ok(())
        })?
    } else {
        None
    };
    if said.is_empty() {
        said.push("no providers yet".into());
    }
    Ok(json!({ "said": said.join("\n"), "note": note }))
}

/// Ask every provider which models it serves, as the process starts, and keep
/// what changed; say what was found, or why nothing was asked.
pub fn refresh_all<C: WithPool>(c: &C) -> String {
    match refresh(c, None, Change::by_this_computer("provider.refresh")) {
        Ok(out) => out["said"].as_str().unwrap_or_default().to_string(),
        Err(e) => e.message,
    }
}

/// Mark the models added of `provider` in `pool` as what it answered it
/// serves now says: one it no longer lists, missing; one it lists, not. What
/// a person said of a model stays as it is, and nothing is added.
fn mark(pool: &mut AnnPoolSetting, provider: &str, found: &[Found]) {
    for m in pool.models.iter_mut().filter(|m| m.provider == provider) {
        m.missing = !found.iter().any(|f| f.id == m.id);
    }
}

/// What the provider `name` serves now, as it answers: another runner's
/// shared models, or what its server lists.
fn serving<C: WithPool>(c: &C, kept: &AnnPoolSetting, name: &str) -> Result<Vec<Found>, Error> {
    match kept.providers.get(name) {
        Some(ProviderSetting::Runner { runner }) => c.shared_by(runner),
        Some(_) => match c.pool()?.direct.get(name) {
            Some(provider) => ask(provider, false),
            None => Err(Error::new(
                Code::Unavailable,
                format!("{name} is not usable now"),
            )),
        },
        None => Err(Error::not_found(format!("there is no provider '{name}'")).on("name")),
    }
}

/// What the provider `name` serves now, each with what it is as far as it
/// says and whether it is in the pool; nothing is kept.
fn served<C: WithPool>(c: &C, name: &str) -> Result<Value, Error> {
    let kept = c.kept_pool()?;
    let found = serving(c, &kept, name)?;
    let mut said = Vec::new();
    let mut rows = Vec::new();
    for f in &found {
        let added = kept.model(name, &f.id).is_some();
        let what = f
            .kind
            .map_or_else(|| "what it is is not said".to_string(), |k| k.describe());
        said.push(format!(
            "{name}:{}  {what}{}",
            f.id,
            if added { "  (in the pool)" } else { "" }
        ));
        let mut row = json!({
            "name": format!("{name}:{}", f.id),
            "id": f.id,
            "type": f.kind.map(|k| k.type_id()),
            "added": added,
        });
        if let Some(ModelKind::Llm { tier, .. }) = f.kind {
            row["tier"] = json!(tier.as_str());
        }
        rows.push(row);
    }
    if said.is_empty() {
        said.push(format!("{name} serves no models"));
    }
    Ok(json!({ "said": said.join("\n"), "models": rows }))
}

/// Each model added, or those of `provider`: what it is, and whether its
/// provider still serves it.
fn list_models<C: WithPool>(c: &C, side: Side, provider: Option<&str>) -> Result<Value, Error> {
    let kept = c.kept_pool()?;
    if let Some(s) = provider
        && !kept.providers.contains_key(s)
    {
        return Err(Error::not_found(format!("there is no provider '{s}'")).on("provider"));
    }
    let mut said = Vec::new();
    let mut rows = Vec::new();
    for m in kept
        .models
        .iter()
        .filter(|m| provider.is_none_or(|s| m.provider == s))
    {
        let mut state = Vec::new();
        if m.missing {
            state.push("missing");
        }
        if m.shared {
            state.push("shared through the hub");
        }
        let what = format!("{}{}", m.kind.describe(), described(m.abilities.as_deref()));
        said.push(if state.is_empty() {
            format!("{}  {what}", m.name())
        } else {
            format!("{}  {what}  ({})", m.name(), state.join(", "))
        });
        let mut row = json!({
            "name": m.name(),
            "provider": m.provider,
            "id": m.id,
            "type": m.kind.type_id(),
            "missing": m.missing,
            "shared": m.shared,
            "abilities": m.abilities,
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
            ModelKind::Decision => {}
        }
        rows.push(row);
    }
    if said.is_empty() {
        said.push(match provider {
            Some(s) => format!(
                "no model of {s} is in the pool; `{p} provider models {s}` lists what it serves, and `{p} model add {s}:<model>` adds one",
                p = side.program()
            ),
            None => format!(
                "no models yet; `{p} provider add` adds a provider, and `{p} model add <provider>:<model>` a model it serves",
                p = side.program()
            ),
        });
    }
    Ok(json!({ "said": said.join("\n"), "models": rows }))
}

/// The model `given` names, `<provider>:<id>`, as kept.
fn model_of<'a>(pool: &'a AnnPoolSetting, given: &str) -> Result<&'a PoolModel, Error> {
    let (provider, id) = split_name(given).ok_or_else(|| {
        Error::invalid(format!(
            "'{given}' must be <provider>:<model>, such as local:qwen3.5:9b"
        ))
        .on("model")
    })?;
    pool.model(provider, id).ok_or_else(|| {
        Error::not_found(format!("there is no model '{provider}:{id}'"))
            .on("model")
            .hint("`model list` lists those in the pool; `model add` adds one")
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

/// `yes` or `no`, as `field` takes it.
fn yes_no(given: Option<&str>, field: &'static str) -> Result<Option<bool>, Error> {
    match given.map(str::trim) {
        None => Ok(None),
        Some("yes") => Ok(Some(true)),
        Some("no") => Ok(Some(false)),
        Some(other) => Err(Error::invalid(format!("'{other}' must be yes or no")).on(field)),
    }
}

fn add_model<C: WithPool>(c: &C, input: AddModel, change: Change) -> Result<Value, Error> {
    let (provider, id) = split_name(&input.model).ok_or_else(|| {
        Error::invalid(format!(
            "'{}' must be <provider>:<model>, such as local:qwen3.5:9b",
            input.model
        ))
        .on("model")
    })?;
    let kept = c.kept_pool()?;
    if !kept.providers.contains_key(provider) {
        return Err(
            Error::not_found(format!("there is no provider '{provider}'"))
                .on("model")
                .hint("`provider add` adds one"),
        );
    }
    let name = format!("{provider}:{id}");
    if kept.model(provider, id).is_some() {
        return Ok(json!({ "said": format!("{name} is in the pool already") }));
    }
    let tier = tier_of(input.tier.as_deref())?;
    let tools = yes_no(input.tools.as_deref(), "tools")?;
    if input.context_window == Some(0) {
        return Err(Error::invalid("`context_window` must be greater than 0").on("context_window"));
    }
    let found = serving(c, &kept, provider)?;
    let Some(said) = found.into_iter().find(|f| f.id == id) else {
        return Err(Error::not_found(format!("{provider} does not serve {id}"))
            .on("model")
            .hint(format!("`provider models {provider}` lists what it serves")));
    };
    let mut kind = match said.kind {
        Some(kind) => kind,
        None => asked_kind(c, provider, id)?,
    };
    match &mut kind {
        ModelKind::Llm {
            tier: t,
            context_window,
            tools: makes_calls,
        } => {
            if let Some(given) = tier {
                *t = given;
            }
            if input.context_window.is_some() {
                *context_window = input.context_window;
            }
            if tools.is_some() {
                *makes_calls = tools;
            }
        }
        other => {
            for (given, field) in [
                (tier.is_some(), "tier"),
                (input.context_window.is_some(), "context_window"),
                (tools.is_some(), "tools"),
                (input.abilities.is_some(), "abilities"),
            ] {
                if given {
                    return Err(Error::invalid(format!(
                        "{name} is not an LLM: it is {}",
                        other.describe()
                    ))
                    .on(field));
                }
            }
        }
    }
    let abilities = written(input.abilities.as_deref());
    let note = c.change_pool(change, &mut |p| {
        p.add(provider, id, kind);
        if let Some(m) = p.model_mut(provider, id) {
            m.abilities = abilities.clone();
        }
        Ok(())
    })?;
    Ok(json!({
        "said": format!("{name} is in the pool: {}", kind.describe()),
        "note": note,
    }))
}

/// What the model `id` of `provider`, which did not say, is: an embedding
/// model when the provider embeds a text with it, else an LLM.
fn asked_kind<C: WithPool>(c: &C, provider: &str, id: &str) -> Result<ModelKind, Error> {
    let pool = c.pool()?;
    let Some(server) = pool.direct.get(provider) else {
        return Err(Error::new(
            Code::Unavailable,
            format!(
                "what {provider}:{id} is is not said, and only the runner that shares it can ask"
            ),
        ));
    };
    if matches!(server.settings, BackendSettings::Anthropic { .. }) {
        return Ok(llm(id));
    }
    Ok(
        match crate::embed::embed(server, id, &["check".to_string()]) {
            Ok(vectors) => match vectors.first().map(Vec::len) {
                Some(n) if n > 0 => ModelKind::Embedding {
                    dimensions: u32::try_from(n).unwrap_or(u32::MAX),
                },
                _ => llm(id),
            },
            Err(_) => llm(id),
        },
    )
}

fn remove_model<C: WithPool>(c: &C, given: &str, change: Change) -> Result<Value, Error> {
    let kept = c.kept_pool()?;
    let model = model_of(&kept, given)?.clone();
    let note = c.change_pool(change, &mut |p| {
        p.models
            .retain(|m| !(m.provider == model.provider && m.id == model.id));
        Ok(())
    })?;
    Ok(json!({ "said": format!("{} is out of the pool", model.name()), "note": note }))
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
    let tools_now = yes_no(input.tools.as_deref(), "tools")?.or(tools);
    let kind = ModelKind::Llm {
        tier: tier_of(input.tier.as_deref())?.unwrap_or(tier),
        context_window: input.context_window.or(context_window),
        tools: tools_now,
    };
    let abilities = match input.abilities.as_deref() {
        Some(given) => written(Some(given)),
        None => model.abilities.clone(),
    };
    if kind == model.kind && abilities == model.abilities {
        return Ok(json!({ "said": format!("{} is as it was", model.name()) }));
    }
    let note = c.change_pool(change, &mut |p| {
        if let Some(m) = p.model_mut(&model.provider, &model.id) {
            m.kind = kind;
            m.abilities = abilities.clone();
        }
        Ok(())
    })?;
    Ok(json!({
        "said": format!("{} is {}{}", model.name(), kind.describe(), described(abilities.as_deref())),
        "note": note,
    }))
}

/// Abilities as a person wrote them: none when they wrote nothing.
pub(crate) fn written(given: Option<&str>) -> Option<String> {
    given
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(String::from)
}

/// A model's abilities, as a list says them after what it is.
fn described(abilities: Option<&str>) -> String {
    abilities.map_or(String::new(), |a| format!(": {a}"))
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
    if on
        && !matches!(
            kept.providers.get(&model.provider),
            Some(ProviderSetting::OpenAiCompatible { .. })
        )
    {
        return Err(Error::invalid(format!(
            "{} is not of an {OPENAI_COMPATIBLE_API} provider of the runner's own, the one kind a runner shares",
            model.name()
        ))
        .on("model"));
    }
    let note = c.change_pool(change, &mut |p| {
        if let Some(m) = p.model_mut(&model.provider, &model.id) {
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

/// The key a provider is tried with: the one given, or the value of the
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
    let base_url = url.to_string();
    let settings = match kind {
        ANTHROPIC_API => BackendSettings::Anthropic {
            base_url,
            api_key: key,
        },
        DECISION_API => BackendSettings::Decision {
            base_url,
            api_key: key,
        },
        _ => BackendSettings::OpenAiCompatible {
            base_url,
            api_key: key,
        },
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
        BackendSettings::Decision { base_url, .. } => BackendSettings::Decision {
            base_url: base_url.clone(),
            api_key: key,
        },
    }
}

/// The models `provider` serves, each with what it is, or why it cannot be
/// added: one that wants a key it was not given has an interface ask for it,
/// when `may_ask`.
fn ask(provider: &LlmBackend, may_ask: bool) -> Result<Vec<Found>, Error> {
    use ensemble_core::port::Error as Port;
    discover(provider).map_err(|e| match e {
        Port::Api {
            status: 401 | 403, ..
        } if may_ask => {
            Error::invalid(format!("{} wants an API key", provider.settings.base_url())).on("key")
        }
        Port::Api {
            status: 401 | 403, ..
        } => Error::invalid(format!(
            "{} did not accept the key: {e}",
            provider.settings.base_url()
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

    fn found(id: &str) -> Found {
        Found {
            id: id.into(),
            kind: None,
        }
    }

    #[test]
    fn a_model_no_longer_listed_is_missing_and_nothing_listed_is_added() {
        let mut pool = AnnPoolSetting::default();
        let small = ModelKind::Llm {
            tier: Tier::Small,
            context_window: None,
            tools: None,
        };
        pool.add("local", "b", small);
        mark(&mut pool, "local", &[found("a"), found("b")]);
        assert_eq!(
            pool.models.iter().map(PoolModel::name).collect::<Vec<_>>(),
            ["local:b"],
            "only what a person added"
        );
        assert!(!pool.models[0].missing);
        // `b` is gone.
        mark(&mut pool, "local", &[found("a")]);
        assert!(pool.model("local", "b").unwrap().missing);
        // Back again, and what a person said of it is kept.
        mark(&mut pool, "local", &[found("b")]);
        let b = pool.model("local", "b").unwrap();
        assert!(!b.missing);
        assert_eq!(b.kind, small);
    }
}
