//! The hub's settings operations beside its ANN pool
//! (`ensemble_backends::ops`): what a person means to change, each checked as
//! it is made, so nobody edits the hub's settings nor knows its keys.
//!
//! | Operation | Does |
//! |---|---|
//! | `settings.show` | every setting |
//! | `settings.history` | who changed the settings, when, and how |
//! | `settings.import` | settings exported from a hub, whole, on its computer alone |
//! | `settings.owner` | the hub's name |
//! | `settings.sessions`, `settings.lockout` | how long a login lasts, and when failed logins lock a name |
//! | `knowledge.models` | the models that turn shares into notes, each asked whether it serves the model |
//! | `knowledge.embedding` | the embedding model, asked to embed once, or none |
//! | `knowledge.recall` | how much one recall returns |
//! | `tracker.list`, `tracker.add`, `tracker.remove` | the trackers whose issues become tasks |
//! | `role.set`, `role.remove` | what each role may do |
//! | `registration.set` | whether people sign up, and how many may wait |
//! | `source.filter` | the model that collects and filters what sources bring in, and what is confidential, or none |
//! | `source.try` | what the filter would leave of a text, shared nowhere |
//!
//! Each changes the hub's settings through [`Kept`], in its database, so a
//! change is loaded before it is kept, kept in the audit log with who made
//! it, and used by the running hub at once.
//!
//! [`Kept`]: ensemble_config::kept::Kept

use crate::accounts::HubContext;
use ensemble_backends::ops::WithPool;
use ensemble_config::ann::Use;
use ensemble_config::kept::{Change, WithKept};
use ensemble_config::{
    EmbedSetting, HubSettings, LlmBackend, ModelSetting, ModelsSetting, Permission,
    RegistrationSetting, SourceFilterSetting, TrackerSetting, WhenNone,
};
use ensemble_ops::{Access, Caller, Code, Error, Field, Kind, Operation, Spec, operation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const ADMIN: Access = Access::Permission("admin");
const PERMISSIONS: &[&str] = Permission::NAMES;

const NO_CHECK: Field = Field::new(
    "no_check",
    Kind::Flag,
    "Do not ask the providers whether they serve the models now.",
);
const DISTILL: &[Field] = &[
    Field::new(
        "models",
        Kind::List,
        "The models that turn shares into notes, in order, each <provider>:<model>, an LLM of the hub's ANN pool that is on, such as central:qwen3.5-72b.",
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
        "The embedding model, <provider>:<model>, an embedding model of the hub's ANN pool that is on, such as central:nomic-embed-text.",
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
        Kind::Choices(PERMISSIONS),
        "What it may do: share, recall, tasks, admin.",
    )
    .required()
    .positional(),
];
const NAMED_ROLE: &[Field] = &[ROLE_NAME];
const SOURCE_FILTER: &[Field] = &[
    Field::new(
        "models",
        Kind::List,
        "The models that collect and filter, in order, each <provider>:<model>, an LLM of the hub's ANN pool that is on; unchanged when left out.",
    )
    .positional(),
    Field::text(
        "prompt",
        "What is confidential, in your words, such as salaries and clients' names; needed the first time.",
    ),
    Field::new(
        "when_none",
        Kind::Choice(&["wait", "fail"]),
        "When none can be used: wait and try again, or drop what came in; wait the first time when left out.",
    ),
    Field::new(
        "max_tokens",
        Kind::Number,
        "Most tokens of one answer; 2000 the first time when left out.",
    ),
    Field::new(
        "off",
        Kind::Flag,
        "No filter: no source may be connected.",
    ),
    NO_CHECK,
];
const SOURCE_TRY: &[Field] = &[Field::text(
    "text",
    "A text as a source might bring it, such as a few messages of a chat.",
)
.required()
.positional()];
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

#[derive(Deserialize, Serialize)]
struct Distill {
    models: Vec<String>,
    when_none: Option<String>,
    max_tokens: Option<u64>,
    #[serde(default)]
    no_check: bool,
}

#[derive(Deserialize, Serialize)]
struct Embed {
    model: Option<String>,
    min_similarity: Option<String>,
    #[serde(default)]
    off: bool,
    #[serde(default)]
    no_check: bool,
}

#[derive(Deserialize, Serialize)]
struct Recall {
    max_notes: Option<u64>,
    max_chars: Option<u64>,
}

#[derive(Deserialize, Serialize)]
struct TrackerAdd {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    dir: Option<String>,
    label: Option<String>,
    poll_seconds: Option<u64>,
}

#[derive(Deserialize, Serialize)]
struct Named {
    name: String,
}

#[derive(Deserialize, Serialize)]
struct RoleSet {
    name: String,
    permissions: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct Registration {
    #[serde(rename = "type")]
    kind: String,
    max_pending: Option<u64>,
}

#[derive(Deserialize, Serialize)]
struct Nothing {}

#[derive(Deserialize, Serialize)]
struct Owner {
    name: String,
}

#[derive(Deserialize, Serialize)]
struct Sessions {
    hours: u64,
}

#[derive(Deserialize, Serialize)]
struct Lockout {
    failures: u64,
    minutes: u64,
}

#[derive(Deserialize, Serialize)]
struct Import {
    settings: String,
}

const OWNER: &[Field] = &[Field::text(
    "name",
    "The hub's name, for its log and its listings, such as acme or ana.",
)
.required()
.positional()];
const SESSIONS: &[Field] = &[Field::new("hours", Kind::Number, "Hours a login lasts.")
    .required()
    .positional()];
const LOCKOUT: &[Field] = &[
    Field::new(
        "failures",
        Kind::Number,
        "Failed logins before a name is refused.",
    )
    .required(),
    Field::new(
        "minutes",
        Kind::Number,
        "For how many minutes it is refused.",
    )
    .required(),
];
const IMPORT: &[Field] = &[Field::text(
    "settings",
    "Settings as `settings show --json` prints them, whole.",
)
.required()
.positional()];

#[derive(Deserialize, Serialize)]
struct SourceFilter {
    #[serde(default)]
    models: Vec<String>,
    prompt: Option<String>,
    when_none: Option<String>,
    max_tokens: Option<u64>,
    #[serde(default)]
    off: bool,
    #[serde(default)]
    no_check: bool,
}

#[derive(Deserialize, Serialize)]
struct SourceTry {
    text: String,
}

/// The goal operations of this module.
pub fn operations() -> Vec<Operation<HubContext>> {
    vec![
        operation(
            Spec {
                name: "settings.show",
                summary: "Show every setting of the hub; its keys are never among them.",
                access: ADMIN,
                input: &[],
            },
            |c: &HubContext, _: &Caller, _: Nothing| show(c),
        )
        .shown(shown),
        operation(
            Spec {
                name: "settings.history",
                summary: "Show who changed the hub's settings, when, and with which operation.",
                access: ADMIN,
                input: &[],
            },
            |c: &HubContext, _: &Caller, _: Nothing| history(c),
        ),
        operation(
            Spec {
                name: "settings.import",
                summary: "Replace every setting with settings another hub exported, once they load.",
                access: Access::Local,
                input: IMPORT,
            },
            |c: &HubContext, caller: &Caller, input: Import| {
                let change = Change::new(caller, &IMPORT_SPEC, json!({}));
                import(c, &input.settings, change)
            },
        ),
        operation(
            Spec {
                name: "settings.owner",
                summary: "Name the hub.",
                access: ADMIN,
                input: OWNER,
            },
            |c: &HubContext, caller: &Caller, input: Owner| {
                let change = changed(caller, "settings.owner", &input);
                let name = input.name.trim().to_string();
                edit(c, change, format!("the hub is {name}"), |s| {
                    s.owner = name.clone();
                    Ok(())
                })
            },
        ),
        operation(
            Spec {
                name: "settings.sessions",
                summary: "Say how many hours a login lasts.",
                access: ADMIN,
                input: SESSIONS,
            },
            |c: &HubContext, caller: &Caller, input: Sessions| {
                let change = changed(caller, "settings.sessions", &input);
                let hours = positive(input.hours, "hours")?;
                edit(c, change, format!("a login lasts {hours} hours"), |s| {
                    s.session_hours = hours;
                    Ok(())
                })
            },
        ),
        operation(
            Spec {
                name: "settings.lockout",
                summary: "Say after how many failed logins a name is refused, and for how long.",
                access: ADMIN,
                input: LOCKOUT,
            },
            |c: &HubContext, caller: &Caller, input: Lockout| {
                let change = changed(caller, "settings.lockout", &input);
                let failures = positive(input.failures, "failures")?;
                let minutes = positive(input.minutes, "minutes")?;
                let said = format!("{failures} failed logins refuse a name for {minutes} minutes");
                edit(c, change, said, |s| {
                    s.lockout_failures = failures;
                    s.lockout_minutes = minutes;
                    Ok(())
                })
            },
        ),
        operation(
            Spec {
                name: "knowledge.models",
                summary: "Choose the models that turn shares into notes, in order: each is asked whether it serves its model.",
                access: ADMIN,
                input: DISTILL,
            },
            |c: &HubContext, caller: &Caller, input: Distill| {
                let change = changed(caller, "knowledge.models", &input);
                distill(c, input, change)
            },
        ),
        operation(
            Spec {
                name: "knowledge.embedding",
                summary: "Choose the embedding model, which finds notes by what they mean, or none.",
                access: ADMIN,
                input: EMBED,
            },
            |c: &HubContext, caller: &Caller, input: Embed| {
                let change = changed(caller, "knowledge.embedding", &input);
                embed(c, input, change)
            },
        ),
        operation(
            Spec {
                name: "knowledge.recall",
                summary: "Say how much one recall returns.",
                access: ADMIN,
                input: RECALL,
            },
            |c: &HubContext, caller: &Caller, input: Recall| {
                let change = changed(caller, "knowledge.recall", &input);
                recall(c, input, change)
            },
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
            |c: &HubContext, caller: &Caller, input: TrackerAdd| {
                let change = changed(caller, "tracker.add", &input);
                add_tracker(c, input, change)
            },
        ),
        operation(
            Spec {
                name: "tracker.remove",
                summary: "Stop watching a tracker.",
                access: ADMIN,
                input: NAMED_TRACKER,
            },
            |c: &HubContext, caller: &Caller, input: Named| {
                let change = changed(caller, "tracker.remove", &input);
                edit(
                    c,
                    change,
                    format!("the hub no longer watches the tracker {}", input.name),
                    |s| {
                        s.trackers
                            .remove(&input.name)
                            .map(|_| ())
                            .ok_or_else(|| no_such("tracker", &input.name))
                    },
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
            |c: &HubContext, caller: &Caller, input: RoleSet| {
                let change = changed(caller, "role.set", &input);
                set_role(c, input, change)
            },
        ),
        operation(
            Spec {
                name: "role.remove",
                summary: "Take a role out; refused while an account has it.",
                access: ADMIN,
                input: NAMED_ROLE,
            },
            |c: &HubContext, caller: &Caller, input: Named| {
                let change = changed(caller, "role.remove", &input);
                remove_role(c, &input.name, change)
            },
        ),
        operation(
            Spec {
                name: "registration.set",
                summary: "Say whether people sign up themselves, for an admin to let in, or only an admin adds them.",
                access: ADMIN,
                input: REGISTRATION,
            },
            |c: &HubContext, caller: &Caller, input: Registration| {
                let change = changed(caller, "registration.set", &input);
                registration(c, input, change)
            },
        ),
        operation(
            Spec {
                name: "source.filter",
                summary: "Choose the model that collects and filters what sources bring in, and say what is confidential.",
                access: ADMIN,
                input: SOURCE_FILTER,
            },
            |c: &HubContext, caller: &Caller, input: SourceFilter| {
                let change = changed(caller, "source.filter", &input);
                source_filter(c, input, change)
            },
        ),
        operation(
            Spec {
                name: "source.try",
                summary: "Show what the source filter would leave of a text, without sharing it.",
                access: ADMIN,
                input: SOURCE_TRY,
            },
            |c: &HubContext, _: &Caller, input: SourceTry| source_try(c, input),
        ),
    ]
}

fn distill(c: &HubContext, input: Distill, change: Change) -> Result<Value, Error> {
    if input.models.is_empty() {
        return Err(Error::invalid("name at least one model").on("models"));
    }
    let mut candidates = Vec::new();
    for given in &input.models {
        let (provider, model) = model_of(c, given, "models", Use::Llm, input.no_check)?;
        candidates.push(ModelSetting {
            provider: provider.to_string(),
            model: model.to_string(),
        });
    }
    let when_none = input.when_none.as_deref().map(when_none).transpose()?;
    if input.max_tokens == Some(0) {
        return Err(Error::invalid("`max_tokens` must be greater than 0").on("max_tokens"));
    }
    let said = format!("shares are distilled by {}", input.models.join(", then "));
    edit(c, change, said, |s| {
        s.distill.candidates = candidates.clone();
        if let Some(when) = when_none {
            s.distill.when_none = when;
        }
        if let Some(n) = input.max_tokens {
            s.distill.max_tokens = clamp(n);
        }
        Ok(())
    })
}

fn source_filter(c: &HubContext, input: SourceFilter, change: Change) -> Result<Value, Error> {
    if input.off {
        if !input.models.is_empty() || input.prompt.is_some() {
            return Err(Error::invalid("give models and a prompt, or `off`, not both").on("off"));
        }
        return edit(c, change, "no source may be connected now".into(), |s| {
            s.source_filter = None;
            Ok(())
        });
    }
    let mut candidates = Vec::new();
    for given in &input.models {
        let (provider, model) = model_of(c, given, "models", Use::Llm, input.no_check)?;
        candidates.push(ModelSetting {
            provider: provider.to_string(),
            model: model.to_string(),
        });
    }
    let prompt = input.prompt.as_deref().map(str::trim);
    if prompt.is_some_and(str::is_empty) {
        return Err(Error::invalid("say what is confidential").on("prompt"));
    }
    if input.max_tokens == Some(0) {
        return Err(Error::invalid("`max_tokens` must be greater than 0").on("max_tokens"));
    }
    let when = input.when_none.as_deref().map(when_none).transpose()?;
    let now = c.kept()?.read()?.source_filter;
    if candidates.is_empty()
        && now.is_some()
        && prompt.is_none()
        && when.is_none()
        && input.max_tokens.is_none()
    {
        return Err(Error::invalid(
            "give models, a prompt, `when_none` or `max_tokens`",
        ));
    }
    // What is not given is kept; the first time, models and a prompt are needed.
    let filter = match now {
        Some(mut f) => {
            if !candidates.is_empty() {
                f.models.candidates = candidates;
            }
            if let Some(p) = prompt {
                f.prompt = p.to_string();
            }
            if let Some(w) = when {
                f.models.when_none = w;
            }
            if let Some(n) = input.max_tokens {
                f.models.max_tokens = clamp(n);
            }
            f
        }
        None => {
            if candidates.is_empty() {
                return Err(Error::invalid("name at least one model").on("models"));
            }
            let Some(p) = prompt else {
                return Err(Error::invalid("say what is confidential").on("prompt"));
            };
            SourceFilterSetting {
                models: ModelsSetting {
                    candidates,
                    when_none: when.unwrap_or(WhenNone::Wait),
                    max_tokens: clamp(input.max_tokens.unwrap_or(2000)),
                },
                prompt: p.to_string(),
            }
        }
    };
    edit(
        c,
        change,
        "what sources bring in is filtered so now".into(),
        |s| {
            s.source_filter = Some(filter.clone());
            Ok(())
        },
    )
}

fn source_try(c: &HubContext, input: SourceTry) -> Result<Value, Error> {
    let memory = ensemble_knowledge::Memory::new(&c.hub()?, None);
    let message = ensemble_knowledge::sources::Message {
        id: "try".into(),
        from: "someone".into(),
        at: ensemble_core::time::utc(std::time::SystemTime::now()),
        place: "a try".into(),
        text: input.text,
    };
    let sifted = memory
        .sift("try", None, &[message])
        .map_err(|e| Error::new(Code::Unavailable, e))?;
    let said = match (&sifted.collected, &sifted.clean) {
        (None, _) => "Nothing in it is worth keeping.".to_string(),
        (Some(_), None) => {
            "What is worth keeping is all confidential: nothing would be shared.".to_string()
        }
        (Some(_), Some(clean)) => format!("This would be shared, for the whole hub:\n\n{clean}"),
    };
    Ok(json!({ "collected": sifted.collected, "clean": sifted.clean, "said": said }))
}

fn embed(c: &HubContext, input: Embed, change: Change) -> Result<Value, Error> {
    if input.off {
        if input.model.is_some() {
            return Err(Error::invalid("give a model, or `off`, not both").on("off"));
        }
        if c.kept()?.read()?.embed.is_none() {
            return Ok(
                json!({ "said": "there is no embedding model: notes are found by their words" }),
            );
        }
        return edit(
            c,
            change,
            "no embedding model: notes are found by their words alone".into(),
            |s| {
                s.embed = None;
                Ok(())
            },
        );
    }
    let given = input.model.as_deref().ok_or_else(|| {
        Error::invalid("name the model, <provider>:<model>, or say `off`").on("model")
    })?;
    let (provider, model) = model_of(c, given, "model", Use::Embedding, true)?;
    let now = c.kept()?.read()?.embed.map(|e| e.min_similarity);
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
    if !input.no_check
        && let Some(b) = c.pool()?.direct.get(provider)
    {
        ensemble_backends::embed::embed(b, model, &["check".to_string()]).map_err(|e| {
            Error::new(
                Code::Unavailable,
                format!("{provider}:{model} did not embed: {e}"),
            )
            .on("model")
            .hint("check the model's name, or go on without asking with `no_check`")
        })?;
    }
    let said = format!("notes are found by their meaning too, with {provider}:{model}");
    edit(c, change, said, |s| {
        s.embed = Some(EmbedSetting {
            provider: provider.to_string(),
            model: model.to_string(),
            min_similarity: min,
        });
        Ok(())
    })
}

fn recall(c: &HubContext, input: Recall, change: Change) -> Result<Value, Error> {
    if input.max_notes.is_none() && input.max_chars.is_none() {
        return Err(Error::invalid("give `max_notes`, `max_chars`, or both").on("max_notes"));
    }
    let notes = input
        .max_notes
        .map(|n| positive(n, "max_notes"))
        .transpose()?;
    let chars = input
        .max_chars
        .map(|n| positive(n, "max_chars"))
        .transpose()?;
    edit(c, change, "recall is changed".into(), |s| {
        if let Some(n) = notes {
            s.recall_max_notes = n;
        }
        if let Some(n) = chars {
            s.recall_max_chars = n;
        }
        Ok(())
    })
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

fn add_tracker(c: &HubContext, input: TrackerAdd, change: Change) -> Result<Value, Error> {
    let name = input.name.trim().to_string();
    word(&name, "name")?;
    if c.kept()?.read()?.trackers.contains_key(&name) {
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
    let tracker = TrackerSetting::LocalMarkdown {
        dir: dir.to_string(),
        label: input
            .label
            .as_deref()
            .unwrap_or("ensemble")
            .trim()
            .to_string(),
        poll_seconds: clamp(input.poll_seconds.unwrap_or(30)),
    };
    let said = format!("the hub watches the tracker {name}");
    edit(c, change, said, |s| {
        s.trackers.insert(name.clone(), tracker.clone());
        Ok(())
    })
}

fn set_role(c: &HubContext, input: RoleSet, change: Change) -> Result<Value, Error> {
    let name = input.name.trim();
    if name.is_empty() || name.contains(char::is_whitespace) || name.contains('.') {
        return Err(Error::invalid(format!("'{name}' must be one word, with no '.'")).on("name"));
    }
    let mut permissions: Vec<Permission> = Vec::new();
    for p in &input.permissions {
        let p = p.trim();
        let Ok(permission) = serde_json::from_value::<Permission>(json!(p)) else {
            return Err(Error::invalid(format!(
                "'{p}' is not a permission: {}",
                PERMISSIONS.join(", ")
            ))
            .on("permissions"));
        };
        if !permissions.contains(&permission) {
            permissions.push(permission);
        }
    }
    let can: Vec<&str> = permissions.iter().map(|p| p.as_str()).collect();
    let said = format!(
        "{name} may {}",
        if can.is_empty() {
            "do nothing".to_string()
        } else {
            can.join(", ")
        }
    );
    let name = name.to_string();
    edit(c, change, said, |s| {
        s.roles.insert(name.clone(), permissions.clone());
        Ok(())
    })
}

fn remove_role(c: &HubContext, name: &str, change: Change) -> Result<Value, Error> {
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
    edit(c, change, format!("the role {name} is gone"), |s| {
        s.roles
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| no_such("role", name))
    })
}

fn registration(c: &HubContext, input: Registration, change: Change) -> Result<Value, Error> {
    let (registration, said) = match input.kind.as_str() {
        "closed" => {
            if input.max_pending.is_some() {
                return Err(
                    Error::invalid("a closed hub takes no sign-ups to wait").on("max_pending")
                );
            }
            (RegistrationSetting::Closed, "only an admin adds people now")
        }
        _ => {
            let max_pending = positive(input.max_pending.unwrap_or(20), "max_pending")?;
            (
                RegistrationSetting::Approval { max_pending },
                "people sign up, and an admin lets them in",
            )
        }
    };
    edit(c, change, said.into(), |s| {
        s.registration = registration;
        Ok(())
    })
}

/// The change `caller` makes with the operation `name` and `input`.
fn changed(caller: &Caller, name: &str, input: &impl Serialize) -> Change {
    let spec = operations()
        .into_iter()
        .find(|o| o.spec.name == name)
        .map(|o| o.spec)
        .expect("one of these operations");
    Change::new(caller, &spec, input)
}

/// The spec of `settings.import`, whose input is never kept: the settings
/// it brings are.
const IMPORT_SPEC: Spec = Spec {
    name: "settings.import",
    summary: "",
    access: Access::Local,
    input: IMPORT,
};

/// Make `edit` to the hub's settings and keep them with `change`, once they
/// load; say `said`, and what the running hub did with them.
fn edit(
    c: &HubContext,
    change: Change,
    said: String,
    edit: impl FnOnce(&mut HubSettings) -> Result<(), Error>,
) -> Result<Value, Error> {
    let note = c.kept()?.change(change, edit)?;
    Ok(json!({ "said": said, "note": note }))
}

fn no_such(what: &str, name: &str) -> Error {
    Error::not_found(format!("there is no {what} '{name}'")).on("name")
}

/// `n` as a setting greater than 0.
fn positive(n: u64, field: &str) -> Result<u32, Error> {
    match u32::try_from(n) {
        Ok(0) => Err(Error::invalid(format!("`{field}` must be greater than 0")).on(field)),
        Ok(n) => Ok(n),
        Err(_) => Err(Error::invalid(format!("`{field}` is too large")).on(field)),
    }
}

fn clamp(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn when_none(given: &str) -> Result<WhenNone, Error> {
    serde_json::from_value(json!(given))
        .map_err(|_| Error::invalid(format!("'{given}' is not wait or fail")).on("when_none"))
}

/// Every setting, as data, and as lines to read.
fn show(c: &HubContext) -> Result<Value, Error> {
    let settings =
        serde_json::to_value(c.kept()?.read()?).map_err(|e| Error::failed(e.to_string()))?;
    Ok(json!({ "settings": settings }))
}

fn shown(output: &Value) -> ensemble_ops::Said {
    ensemble_ops::Said {
        out: ensemble_ops::lines(&output["settings"]),
        note: None,
    }
}

/// Who changed the settings, when, and how.
fn history(c: &HubContext) -> Result<Value, Error> {
    let log = ensemble_store::kept::audit(&c.dir().core()).map_err(Error::failed)?;
    let said: Vec<String> = log
        .iter()
        .map(|a| format!("{}  {}  {}  {}", a.at, a.who, a.operation, a.input))
        .collect();
    Ok(json!({
        "said": if said.is_empty() { "no changes yet".to_string() } else { said.join("\n") },
        "changes": log,
    }))
}

/// Replace every setting with `text`, settings as `settings show --json`
/// prints them, once they load.
fn import(c: &HubContext, text: &str, change: Change) -> Result<Value, Error> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| Error::invalid(format!("not JSON: {e}")).on("settings"))?;
    let settings: HubSettings =
        serde_json::from_value(value.get("settings").cloned().unwrap_or(value))
            .map_err(|e| Error::invalid(format!("not the hub's settings: {e}")).on("settings"))?;
    let note = c.kept()?.keep(&settings, &change)?;
    Ok(json!({ "said": "the hub's settings are those imported", "note": note }))
}

/// `given`, `<provider>:<model>`, as a model of the hub's ANN pool that is on
/// and of `use_`, and, unless not asked to check, one its provider serves now;
/// a model of a runner only the running hub reaches.
fn model_of<'g>(
    c: &HubContext,
    given: &'g str,
    field: &str,
    use_: Use,
    no_check: bool,
) -> Result<(&'g str, &'g str), Error> {
    let (provider, model) = split(given, field)?;
    let pool = c.kept_pool()?;
    let Some(found) = pool.model(provider, model) else {
        return Err(Error::not_found(format!(
            "there is no model '{provider}:{model}' in the hub's ANN pool"
        ))
        .on(field)
        .hint("`model list` lists them; `provider add` adds a provider and `provider refresh` asks the providers again"));
    };
    if !found.on {
        return Err(Error::invalid(format!(
            "{provider}:{model} is off; turn it on first with `model on {provider}:{model}`"
        ))
        .on(field));
    }
    let fits = match use_ {
        Use::Llm => found.usable_llm(),
        Use::Embedding => found.usable_embedding(),
    };
    if !fits {
        let wanted = match use_ {
            Use::Llm => "an LLM",
            Use::Embedding => "an embedding model",
        };
        return Err(Error::invalid(format!(
            "{provider}:{model} is not {wanted}: it is {}",
            found.kind.describe()
        ))
        .on(field));
    }
    if !no_check && let Some(b) = c.pool()?.direct.get(provider) {
        serves(b, model, field)?;
    }
    Ok((provider, model))
}

fn serves(provider: &LlmBackend, model: &str, field: &str) -> Result<(), Error> {
    let models = ensemble_backends::models::models(provider).map_err(|e| {
        Error::new(
            Code::Unavailable,
            format!(
                "could not ask {} which models it serves: {e}",
                provider.name
            ),
        )
        .on(field)
        .hint("go on without asking with `no_check`")
    })?;
    if models.iter().any(|m| m == model) {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "{} does not serve '{model}' now; it serves {}",
        provider.name,
        if models.is_empty() {
            "none".to_string()
        } else {
            models.join(", ")
        }
    ))
    .on(field))
}

/// `<provider>:<model>`: the provider's name has no ':', the model's may.
fn split<'g>(given: &'g str, field: &str) -> Result<(&'g str, &'g str), Error> {
    given
        .trim()
        .split_once(':')
        .map(|(s, m)| (s.trim(), m.trim()))
        .filter(|(s, m)| !s.is_empty() && !m.is_empty())
        .ok_or_else(|| {
            Error::invalid(format!(
                "'{given}' must be <provider>:<model>, such as central:qwen3.5-72b"
            ))
            .on(field)
        })
}

/// Refuse a name that is not one word of letters, digits, '-' or '_'.
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
