//! The hub's goal operations beside its backends (`ensemble_backends::ops`):
//! what a person means to change, each checked as it is made, so nobody edits
//! hub.yaml by hand nor knows its keys.
//!
//! | Operation | Does |
//! |---|---|
//! | `knowledge.models` | the models that turn shares into notes, each asked whether it serves the model |
//! | `knowledge.embedding` | the embedding model, asked to embed once, or none |
//! | `knowledge.recall` | how much one recall returns |
//! | `tracker.list`, `tracker.add`, `tracker.remove` | the trackers whose issues become tasks |
//! | `role.set`, `role.remove` | what each role may do |
//! | `registration.set` | whether people sign up, and how many may wait |
//!
//! Each writes hub.yaml through its settings file, so a change is loaded
//! before it is written, what Nix declares is refused, and the running hub
//! uses it at once.

use crate::accounts::HubContext;
use ensemble_backends::ops::WithBackends;
use ensemble_config::settings::{self, inline, inline_fields};
use ensemble_config::settings_file::WithSettings;
use ensemble_config::{LlmBackend, Permission};
use ensemble_ops::{Access, Caller, Code, Error, Field, Kind, Operation, Spec, operation};
use serde::Deserialize;
use serde_json::{Value, json};

const ADMIN: Access = Access::Permission("admin");
const KNOWLEDGE: &str = "connectors.knowledge";
const TRACKERS: &str = "connectors.tasks.trackers";
const ROLES: &str = "accounts.roles";
const PERMISSIONS: &[&str] = &["share", "recall", "tasks", "admin", "compute", "host"];

const NO_CHECK: Field = Field::new(
    "no_check",
    Kind::Flag,
    "Do not ask the backends whether they serve the models first.",
);
const DISTILL: &[Field] = &[
    Field::new(
        "models",
        Kind::List,
        "The models that turn shares into notes, in order, each <backend>:<model>, such as central:qwen3.5-72b.",
    )
    .required()
    .positional(),
    Field::new(
        "when_none",
        Kind::Choice(&["wait", "fail"]),
        "When none can be used: wait and try again, or fail the share into review/. Unchanged when left out.",
    ),
    Field::new(
        "max_tokens",
        Kind::Number,
        "Most tokens of one answer. Unchanged when left out.",
    ),
    NO_CHECK,
];
const EMBED: &[Field] = &[
    Field::text(
        "model",
        "The embedding model, <backend>:<model>, such as central:nomic-embed-text.",
    )
    .positional(),
    Field::text(
        "min_similarity",
        "How alike, by cosine from 0 to 1, a note must be to be found by its meaning; 0.5 when there was none.",
    ),
    Field::new(
        "off",
        Kind::Flag,
        "No embedding model: notes are found by their words alone.",
    ),
    NO_CHECK,
];
const RECALL: &[Field] = &[
    Field::new("max_notes", Kind::Number, "Most notes one recall returns."),
    Field::new(
        "max_chars",
        Kind::Number,
        "Most characters one recall returns, all notes together.",
    ),
];
const TRACKER_NAME: Field = Field::text("name", "The tracker's name: letters, digits, '-' or '_'.")
    .required()
    .positional();
const TRACKER_ADD: &[Field] = &[
    TRACKER_NAME,
    Field::new(
        "type",
        Kind::Choice(&["local_markdown"]),
        "What it is: local_markdown, a folder of markdown issues.",
    )
    .required(),
    Field::text(
        "dir",
        "Its folder, relative to the hub's or absolute, for local_markdown.",
    ),
    Field::text(
        "label",
        "The label that hands an issue to a helper team; `ensemble` when left out.",
    ),
    Field::new(
        "poll_seconds",
        Kind::Number,
        "Seconds between two looks at it; 30 when left out.",
    ),
];
const NAMED_TRACKER: &[Field] = &[TRACKER_NAME];
const ROLE_NAME: Field = Field::text("name", "The role's name, one word.")
    .required()
    .positional();
const ROLE_SET: &[Field] = &[
    ROLE_NAME,
    Field::new(
        "permissions",
        Kind::List,
        "What it may do: share, recall, tasks, admin, compute, host.",
    )
    .required()
    .positional(),
];
const NAMED_ROLE: &[Field] = &[ROLE_NAME];
const REGISTRATION: &[Field] = &[
    Field::new(
        "type",
        Kind::Choice(&["approval", "closed"]),
        "approval: anyone signs up and an admin lets them in; closed: only an admin adds people.",
    )
    .required()
    .positional(),
    Field::new(
        "max_pending",
        Kind::Number,
        "Most sign-ups waiting at once, for approval; 20 when left out.",
    ),
];

#[derive(Deserialize)]
struct Distill {
    models: Vec<String>,
    when_none: Option<String>,
    max_tokens: Option<u64>,
    #[serde(default)]
    no_check: bool,
}

#[derive(Deserialize)]
struct Embed {
    model: Option<String>,
    min_similarity: Option<String>,
    #[serde(default)]
    off: bool,
    #[serde(default)]
    no_check: bool,
}

#[derive(Deserialize)]
struct Recall {
    max_notes: Option<u64>,
    max_chars: Option<u64>,
}

#[derive(Deserialize)]
struct TrackerAdd {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    dir: Option<String>,
    label: Option<String>,
    poll_seconds: Option<u64>,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

#[derive(Deserialize)]
struct RoleSet {
    name: String,
    permissions: Vec<String>,
}

#[derive(Deserialize)]
struct Registration {
    #[serde(rename = "type")]
    kind: String,
    max_pending: Option<u64>,
}

#[derive(Deserialize)]
struct Nothing {}

/// The goal operations of this module.
pub fn operations() -> Vec<Operation<HubContext>> {
    vec![
        operation(
            Spec {
                name: "knowledge.models",
                summary: "Choose the models that turn shares into notes, in order: each is asked whether it serves its model.",
                access: ADMIN,
                input: DISTILL,
            },
            |c: &HubContext, _: &Caller, input: Distill| distill(c, input),
        ),
        operation(
            Spec {
                name: "knowledge.embedding",
                summary: "Choose the embedding model, which finds notes by what they mean, or none.",
                access: ADMIN,
                input: EMBED,
            },
            |c: &HubContext, _: &Caller, input: Embed| embed(c, input),
        ),
        operation(
            Spec {
                name: "knowledge.recall",
                summary: "Say how much one recall returns.",
                access: ADMIN,
                input: RECALL,
            },
            |c: &HubContext, _: &Caller, input: Recall| recall(c, input),
        ),
        operation(
            Spec {
                name: "tracker.list",
                summary: "List the trackers whose issues become tasks.",
                access: ADMIN,
                input: &[],
            },
            |c: &HubContext, _: &Caller, _: Nothing| trackers(c),
        ),
        operation(
            Spec {
                name: "tracker.add",
                summary: "Add a tracker whose issues, with its label, become tasks of the people they are assigned to.",
                access: ADMIN,
                input: TRACKER_ADD,
            },
            |c: &HubContext, _: &Caller, input: TrackerAdd| add_tracker(c, input),
        ),
        operation(
            Spec {
                name: "tracker.remove",
                summary: "Stop watching a tracker.",
                access: ADMIN,
                input: NAMED_TRACKER,
            },
            |c: &HubContext, _: &Caller, input: Named| {
                change(
                    c,
                    vec![(format!("{TRACKERS}.{}", input.name), None)],
                    format!("the hub no longer watches the tracker {}", input.name),
                    |names| names.trackers.contains(&input.name),
                    &input.name,
                )
            },
        ),
        operation(
            Spec {
                name: "role.set",
                summary: "Make a role, or change what it may do.",
                access: ADMIN,
                input: ROLE_SET,
            },
            |c: &HubContext, _: &Caller, input: RoleSet| set_role(c, input),
        ),
        operation(
            Spec {
                name: "role.remove",
                summary: "Take a role out; refused while an account has it.",
                access: ADMIN,
                input: NAMED_ROLE,
            },
            |c: &HubContext, _: &Caller, input: Named| remove_role(c, &input.name),
        ),
        operation(
            Spec {
                name: "registration.set",
                summary: "Say whether people sign up themselves, for an admin to let in, or only an admin adds them.",
                access: ADMIN,
                input: REGISTRATION,
            },
            |c: &HubContext, _: &Caller, input: Registration| registration(c, input),
        ),
    ]
}

fn distill(c: &HubContext, input: Distill) -> Result<Value, Error> {
    if input.models.is_empty() {
        return Err(Error::invalid("name at least one model").on("models"));
    }
    let mut candidates = Vec::new();
    for given in &input.models {
        let (backend, model) = model_of(c, given, "models", input.no_check)?;
        candidates.push(json!({ "backend": backend, "model": model }));
    }
    let mut edits = vec![(
        format!("{KNOWLEDGE}.distill.try"),
        Some(format!(
            "[{}]",
            candidates
                .iter()
                .map(|v| inline_fields(&[
                    ("backend", v["backend"].clone()),
                    ("model", v["model"].clone())
                ]))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    )];
    if let Some(when) = &input.when_none {
        edits.push((format!("{KNOWLEDGE}.distill.when_none"), Some(when.clone())));
    }
    if let Some(n) = input.max_tokens {
        if n == 0 {
            return Err(Error::invalid("`max_tokens` must be greater than 0").on("max_tokens"));
        }
        edits.push((
            format!("{KNOWLEDGE}.distill.max_tokens"),
            Some(n.to_string()),
        ));
    }
    let note = commit(c, &edits)?;
    Ok(json!({
        "said": format!("shares are distilled by {}", input.models.join(", then ")),
        "note": note,
    }))
}

fn embed(c: &HubContext, input: Embed) -> Result<Value, Error> {
    let key = format!("{KNOWLEDGE}.embed");
    if input.off {
        if input.model.is_some() {
            return Err(Error::invalid("give a model, or `off`, not both").on("off"));
        }
        let text = c.settings_file()?.read()?;
        if settings::get(&text, &key).map_err(Error::failed)?.is_none() {
            return Ok(
                json!({ "said": "there is no embedding model: notes are found by their words" }),
            );
        }
        let note = commit(c, &[(key, None)])?;
        return Ok(
            json!({ "said": "no embedding model: notes are found by their words alone", "note": note }),
        );
    }
    let given = input.model.as_deref().ok_or_else(|| {
        Error::invalid("name the model, <backend>:<model>, or say `off`").on("model")
    })?;
    let (backend, model) = split(given, "model")?;
    let text = c.settings_file()?.read()?;
    let now = settings::get(&text, &format!("{key}.min_similarity"))
        .map_err(Error::failed)?
        .and_then(|v| v.parse::<f64>().ok());
    let min = match &input.min_similarity {
        Some(m) => m
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|m| (0.0..=1.0).contains(m))
            .ok_or_else(|| {
                Error::invalid("`min_similarity` must be from 0 to 1, such as 0.5")
                    .on("min_similarity")
            })?,
        None => now.unwrap_or(0.5),
    };
    let backends = c.backends()?;
    if let Some(b) = backends.direct.get(backend) {
        if !input.no_check {
            ensemble_backends::embed::embed(b, model, &["check".to_string()]).map_err(|e| {
                Error::new(
                    Code::Unavailable,
                    format!("{model} on {backend} did not embed: {e}"),
                )
                .on("model")
                .hint("check the model's name, or go on without asking with `no_check`")
            })?;
        }
    } else if !backends.relayed.contains_key(backend) {
        return Err(no_backend(backend, "model"));
    }
    let value = inline_fields(&[
        ("backend", json!(backend)),
        ("model", json!(model)),
        ("min_similarity", json!(min)),
    ]);
    let note = commit(c, &[(key, Some(value))])?;
    Ok(json!({
        "said": format!("notes are found by their meaning too, with {model} on {backend}"),
        "note": note,
    }))
}

fn recall(c: &HubContext, input: Recall) -> Result<Value, Error> {
    let mut edits = Vec::new();
    for (value, field) in [
        (input.max_notes, "max_notes"),
        (input.max_chars, "max_chars"),
    ] {
        if let Some(n) = value {
            if n == 0 {
                return Err(Error::invalid(format!("`{field}` must be greater than 0")).on(field));
            }
            edits.push((format!("{KNOWLEDGE}.recall.{field}"), Some(n.to_string())));
        }
    }
    if edits.is_empty() {
        return Err(Error::invalid("give `max_notes`, `max_chars`, or both").on("max_notes"));
    }
    let note = commit(c, &edits)?;
    Ok(json!({ "said": "recall is changed", "note": note }))
}

fn trackers(c: &HubContext) -> Result<Value, Error> {
    let hub = c.hub()?;
    let mut said = Vec::new();
    let mut rows = Vec::new();
    for (name, t) in &hub.connectors.tasks.trackers {
        let ensemble_config::IssueTracker::LocalMarkdown { dir } = &t.kind;
        said.push(format!(
            "{name}  local_markdown in {}, label {}, every {}s",
            dir.display(),
            t.label,
            t.poll_seconds
        ));
        rows.push(json!({
            "name": name,
            "type": "local_markdown",
            "dir": dir,
            "label": t.label,
            "poll_seconds": t.poll_seconds,
        }));
    }
    if said.is_empty() {
        said.push(
            "no trackers: tasks come from client agents alone; `tracker add` adds one".into(),
        );
    }
    Ok(json!({ "said": said.join("\n"), "trackers": rows }))
}

fn add_tracker(c: &HubContext, input: TrackerAdd) -> Result<Value, Error> {
    let name = input.name.trim();
    word(name, "name")?;
    let hub = c.hub()?;
    if hub.connectors.tasks.trackers.contains_key(name) {
        return Err(Error::new(
            Code::Conflict,
            format!("there is a tracker '{name}' already"),
        )
        .on("name"));
    }
    let dir = input
        .dir
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .ok_or_else(|| {
            Error::invalid("`dir` is required for local_markdown: the folder of its issues")
                .on("dir")
        })?;
    let value = inline_fields(&[
        ("type", json!(input.kind)),
        ("dir", json!(dir)),
        (
            "label",
            json!(input.label.as_deref().unwrap_or("ensemble").trim()),
        ),
        ("poll_seconds", json!(input.poll_seconds.unwrap_or(30))),
    ]);
    let note = commit(c, &[(format!("{TRACKERS}.{name}"), Some(value))])?;
    Ok(json!({ "said": format!("the hub watches the tracker {name}"), "note": note }))
}

fn set_role(c: &HubContext, input: RoleSet) -> Result<Value, Error> {
    let name = input.name.trim();
    if name.is_empty() || name.contains(char::is_whitespace) || name.contains('.') {
        return Err(Error::invalid(format!("'{name}' must be one word, with no '.'")).on("name"));
    }
    let mut permissions = Vec::new();
    for p in &input.permissions {
        let p = p.trim();
        if serde_json::from_value::<Permission>(json!(p)).is_err() {
            return Err(Error::invalid(format!(
                "'{p}' is not a permission: {}",
                PERMISSIONS.join(", ")
            ))
            .on("permissions"));
        }
        if !permissions.contains(&p) {
            permissions.push(p);
        }
    }
    let note = commit(
        c,
        &[(format!("{ROLES}.{name}"), Some(inline(&json!(permissions))))],
    )?;
    Ok(json!({
        "said": format!("{name} may {}", if permissions.is_empty() { "do nothing".to_string() } else { permissions.join(", ") }),
        "note": note,
    }))
}

fn remove_role(c: &HubContext, name: &str) -> Result<Value, Error> {
    let users = c.accounts()?.users()?;
    let holders: Vec<&str> = users
        .list()
        .filter(|u| u.role == name)
        .map(|u| u.name.as_str())
        .collect();
    if !holders.is_empty() {
        return Err(Error::new(
            Code::Conflict,
            format!(
                "{} {} the role {name}",
                holders.join(", "),
                if holders.len() == 1 { "has" } else { "have" }
            ),
        )
        .on("name")
        .hint("give them another role first (`user role`)"));
    }
    change(
        c,
        vec![(format!("{ROLES}.{name}"), None)],
        format!("the role {name} is gone"),
        |names| names.roles.contains(&name.to_string()),
        name,
    )
}

fn registration(c: &HubContext, input: Registration) -> Result<Value, Error> {
    let value = match input.kind.as_str() {
        "closed" => {
            if input.max_pending.is_some() {
                return Err(
                    Error::invalid("a closed hub takes no sign-ups to wait").on("max_pending")
                );
            }
            inline_fields(&[("type", json!("closed"))])
        }
        _ => {
            let max = input.max_pending.unwrap_or(20);
            if max == 0 {
                return Err(
                    Error::invalid("`max_pending` must be greater than 0").on("max_pending")
                );
            }
            inline_fields(&[("type", json!("approval")), ("max_pending", json!(max))])
        }
    };
    let note = commit(c, &[("accounts.registration".into(), Some(value))])?;
    let said = match input.kind.as_str() {
        "closed" => "only an admin adds people now".to_string(),
        _ => "people sign up, and an admin lets them in".to_string(),
    };
    Ok(json!({ "said": said, "note": note }))
}

/// What hub.yaml names, for an operation to find what it is about.
struct Names {
    trackers: Vec<String>,
    roles: Vec<String>,
}

/// Make `edits`, once `there` finds `name` in what hub.yaml names.
fn change(
    c: &HubContext,
    edits: Vec<(String, Option<String>)>,
    said: String,
    there: impl Fn(&Names) -> bool,
    name: &str,
) -> Result<Value, Error> {
    let hub = c.hub()?;
    let names = Names {
        trackers: hub.connectors.tasks.trackers.keys().cloned().collect(),
        roles: hub.accounts.roles.keys().cloned().collect(),
    };
    if !there(&names) {
        return Err(Error::not_found(format!("there is no '{name}'")).on("name"));
    }
    let note = commit(c, &edits)?;
    Ok(json!({ "said": said, "note": note }))
}

/// Write `edits` to hub.yaml, and say what the running hub did.
fn commit(c: &HubContext, edits: &[(String, Option<String>)]) -> Result<Option<String>, Error> {
    let file = c.settings_file()?;
    let text = file.plan(edits)?;
    file.commit(&text)
}

/// `given`, `<backend>:<model>`, as one of the hub's backends and a model it
/// serves, unless not asked to check, or the backend is a runner's, which
/// only the running hub reaches.
fn model_of<'g>(
    c: &HubContext,
    given: &'g str,
    field: &str,
    no_check: bool,
) -> Result<(&'g str, &'g str), Error> {
    let (backend, model) = split(given, field)?;
    let backends = c.backends()?;
    match backends.direct.get(backend) {
        Some(b) if !no_check => serves(b, model, field)?,
        Some(_) => {}
        None if backends.relayed.contains_key(backend) => {}
        None => return Err(no_backend(backend, field)),
    }
    Ok((backend, model))
}

fn serves(backend: &LlmBackend, model: &str, field: &str) -> Result<(), Error> {
    let models = ensemble_backends::models::models(backend).map_err(|e| {
        Error::new(
            Code::Unavailable,
            format!("could not ask {} which models it serves: {e}", backend.name),
        )
        .on(field)
        .hint("go on without asking with `no_check`")
    })?;
    if models.iter().any(|m| m == model) {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "{} does not serve '{model}'; it serves {}",
        backend.name,
        if models.is_empty() {
            "none".to_string()
        } else {
            models.join(", ")
        }
    ))
    .on(field))
}

/// `<backend>:<model>`: the backend's name has no ':', the model's may.
fn split<'g>(given: &'g str, field: &str) -> Result<(&'g str, &'g str), Error> {
    given
        .trim()
        .split_once(':')
        .map(|(b, m)| (b.trim(), m.trim()))
        .filter(|(b, m)| !b.is_empty() && !m.is_empty())
        .ok_or_else(|| {
            Error::invalid(format!(
                "'{given}' must be <backend>:<model>, such as central:qwen3.5-72b"
            ))
            .on(field)
        })
}

fn no_backend(backend: &str, field: &str) -> Error {
    Error::not_found(format!("there is no backend '{backend}'"))
        .on(field)
        .hint("`backend list` lists them; `backend add` adds one")
}

/// Refuse a name hub.yaml could not hold as one key.
fn word(name: &str, field: &str) -> Result<(), Error> {
    let fits = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if fits {
        Ok(())
    } else {
        Err(Error::invalid(format!("'{name}' must be letters, digits, '-' or '_'")).on(field))
    }
}
