//! The hub's settings, a section each, with what each is now: the hub and
//! its logins and sign-ups, the roles, the ANN pool (its providers and their
//! models), the models of the knowledge vault, the source filter, the trackers, and who changed what.
//! Each change is an operation's form, filled in with the setting as it is.
//! An operation of the settings no section shows has a page of its own, so a
//! new one is reached before a section shows it.

use crate::Route;
use crate::api::{self, Failure};
use crate::form::{Catalog, Json, OpForm, Pick, When};
use crate::lookup::{self, PERMISSIONS, use_op};
use crate::pages::{label, loaded, text, value, when};
use dioxus::prelude::*;
use serde_json::{Value, json};

/// The sections, by their paths, with their titles.
const SECTIONS: &[(&str, &str)] = &[
    ("general", "Hub and logins"),
    ("roles", "Roles"),
    ("models", "Models"),
    ("knowledge", "Knowledge"),
    ("sources", "Sources"),
    ("trackers", "Trackers"),
    ("history", "History"),
    ("other", "Other operations"),
];

/// The operations a section shows, or another page does.
const SHOWN: &[&str] = &[
    "settings.show",
    "settings.history",
    "settings.owner",
    "settings.sessions",
    "settings.lockout",
    "registration.set",
    "role.list",
    "role.set",
    "role.remove",
    "provider.list",
    "provider.add",
    "provider.remove",
    "provider.test",
    "provider.key",
    "provider.refresh",
    "provider.models",
    "model.list",
    "model.add",
    "model.remove",
    "model.set",
    "runner.models",
    "runner.list",
    "knowledge.models",
    "knowledge.embedding",
    "knowledge.recall",
    "knowledge.share",
    "knowledge.find",
    "source.filter",
    "source.try",
    "tracker.list",
    "tracker.add",
    "tracker.remove",
];

/// The groups of operations that are settings.
const GROUPS: &[&str] = &[
    "settings",
    "registration",
    "role",
    "model",
    "runner",
    "knowledge",
    "provider",
    "source",
    "tracker",
];

/// The hub's settings, as `settings.show` says, for every section: a type of
/// its own, as the context of the page, apart from the [`Catalog`].
type Shown = Resource<Result<Value, Failure>>;

#[derive(Clone, Copy)]
struct Kept(Shown);

#[component]
pub fn Settings() -> Element {
    rsx! { SettingsPage { section: "general" } }
}

#[component]
pub fn SettingsPage(section: String) -> Element {
    let shown: Shown = use_op("settings.show");
    use_context_provider(|| Kept(shown));
    let title = SECTIONS
        .iter()
        .find(|(s, _)| *s == section)
        .map(|(_, t)| *t)
        .unwrap_or("Not found");
    rsx! {
        div { class: "settings",
            nav { class: "side",
                h1 { "Settings" }
                for (path, name) in SECTIONS.iter().copied() {
                    Link {
                        key: "{path}",
                        class: if path == section { "active" } else { "" },
                        to: Route::SettingsPage { section: path.into() },
                        "{name}"
                    }
                }
            }
            div { class: "section",
                h2 { class: "title", "{title}" }
                match section.as_str() {
                    "general" => rsx! { General {} },
                    "roles" => rsx! { Roles {} },
                    "models" => rsx! { Models {} },
                    "knowledge" => rsx! { KnowledgeModels {} },
                    "sources" => rsx! { Sources {} },
                    "trackers" => rsx! { Trackers {} },
                    "history" => rsx! { History {} },
                    "other" => rsx! { Other {} },
                    _ => rsx! { p { "There is no such section." } },
                }
            }
        }
    }
}

/// The settings as they are, and how to have them read again once one
/// changed.
fn settings() -> (Shown, Option<Value>) {
    let Kept(shown) = use_context::<Kept>();
    let now = match &*shown.read() {
        Some(Ok(out)) => Some(out["settings"].clone()),
        _ => None,
    };
    (shown, now)
}

/// What a section shows while the settings load, or once they failed.
fn waiting(shown: &Shown) -> Element {
    match &*shown.read() {
        Some(Err(e)) => rsx! { p { class: "error", "{e}" } },
        _ => rsx! { p { class: "loading", "Loading…" } },
    }
}

/// Models as a list of `<provider>:<model>`, one per line, from the settings'
/// `[{provider, model}]`.
fn model_lines(tried: &Value) -> String {
    tried
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| format!("{}:{}", text(m, "provider"), text(m, "model")))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The words for what is done when no model can be used.
fn when_none(fail: &str) -> Pick {
    Pick::named(
        "when_none",
        &[("wait", "Wait and try again"), ("fail", fail)],
    )
}

#[component]
fn General() -> Element {
    let (mut shown, now) = settings();
    let Some(now) = now else {
        return waiting(&shown);
    };
    let registration = &now["registration"];
    rsx! {
        section { class: "panel",
            h3 { "The hub" }
            OpForm {
                op: "settings.owner",
                values: vec![value("name", text(&now, "owner"))],
                labels: vec![label("name", "name")],
                submit: "Save",
                saved: true,
                on_done: move |_| shown.restart(),
            }
        }
        section { class: "panel",
            h3 { "Logins" }
            OpForm {
                op: "settings.sessions",
                values: vec![value("hours", text(&now, "session_hours"))],
                labels: vec![label("hours", "a login lasts, in hours")],
                submit: "Save",
                saved: true,
                on_done: move |_| shown.restart(),
            }
            OpForm {
                op: "settings.lockout",
                values: vec![
                    value("failures", text(&now, "lockout_failures")),
                    value("minutes", text(&now, "lockout_minutes")),
                ],
                labels: vec![label("failures", "failed logins before a name is refused"), label("minutes", "refused for, in minutes")],
                submit: "Save",
                saved: true,
                on_done: move |_| shown.restart(),
            }
        }
        section { class: "panel",
            h3 { "Sign-ups" }
            OpForm {
                op: "registration.set",
                values: vec![
                    value("type", text(registration, "type")),
                    value("max_pending", text(registration, "max_pending")),
                ],
                picks: vec![Pick::named("type", &[
                    ("closed", "Closed: only an admin adds people"),
                    ("approval", "Open: anyone signs up, and an admin lets them in"),
                ])],
                when: vec![When::new("max_pending", "type", &["approval"])],
                labels: vec![label("type", "who may sign up"), label("max_pending", "most sign-ups waiting at once")],
                submit: "Save",
                saved: true,
                on_done: move |_| shown.restart(),
            }
        }
    }
}

#[component]
fn Roles() -> Element {
    let mut roles = use_op("role.list");
    let users = use_op("user.list");
    let (mut shown, _) = settings();
    let held = |role: &str| -> usize {
        match &*users.read() {
            Some(Ok(out)) => out["users"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|u| u["role"] == role)
                .count(),
            _ => 0,
        }
    };
    let mut changed = move || {
        roles.restart();
        shown.restart();
    };
    rsx! {
        p { class: "muted", "What each role may do. An account has one role; a token may do less than its account's role." }
        {loaded(&roles.read(), |_| {
            let list = lookup::roles(&roles);
            rsx! {
                div { class: "scroll",
                    table { class: "matrix",
                        thead {
                            tr {
                                th { "Role" }
                                for (p, about) in PERMISSIONS.iter().copied() {
                                    th { key: "{p}", title: "{about}", "{p}" }
                                }
                                th { "Accounts" }
                                th {}
                            }
                        }
                        tbody {
                            for (name, may) in list {
                                RoleRow {
                                    key: "{name}",
                                    name: name.clone(),
                                    may,
                                    accounts: held(&name),
                                    on_changed: move |_| changed(),
                                }
                            }
                        }
                    }
                }
            }
        })}
        dl { class: "legend",
            for (p, about) in PERMISSIONS.iter().copied() {
                div { key: "{p}",
                    dt { "{p}" }
                    dd { "{about}" }
                }
            }
        }
        section { class: "panel",
            h3 { "New role" }
            OpForm {
                op: "role.set",
                labels: vec![label("permissions", "may")],
                submit: "Make the role",
                clear: true,
                on_done: move |_| changed(),
            }
        }
    }
}

/// A role, with a box for each permission: saved once changed.
#[component]
fn RoleRow(
    name: String,
    may: Vec<String>,
    accounts: usize,
    on_changed: EventHandler<()>,
) -> Element {
    let first = may.clone();
    let mut chosen = use_signal(|| may.clone());
    let mut failure = use_signal(|| None::<Failure>);
    let mut asking = use_signal(|| false);
    let mut busy = use_signal(|| false);
    // Kept in the order of the permissions.
    let now: Vec<String> = PERMISSIONS
        .iter()
        .map(|(p, _)| p.to_string())
        .filter(|p| chosen.read().contains(p))
        .collect();
    let mut sorted = first.clone();
    sorted.sort();
    let mut also = now.clone();
    also.sort();
    let dirty = sorted != also;
    let save = {
        let (name, now) = (name.clone(), now.clone());
        move |_| {
            let (name, now) = (name.clone(), now.clone());
            spawn(async move {
                busy.set(true);
                failure.set(None);
                match api::op("role.set", json!({ "name": name, "permissions": now })).await {
                    Ok(_) => on_changed.call(()),
                    Err(e) => failure.set(Some(e)),
                }
                busy.set(false);
            });
        }
    };
    let remove = {
        let name = name.clone();
        move |_| {
            let name = name.clone();
            spawn(async move {
                match api::op("role.remove", json!({ "name": name })).await {
                    Ok(_) => on_changed.call(()),
                    Err(e) => failure.set(Some(e)),
                }
                asking.set(false);
            });
        }
    };
    rsx! {
        tr { class: if dirty { "dirty" } else { "" },
            td { strong { "{name}" } }
            for (p, about) in PERMISSIONS.iter().copied() {
                td { key: "{p}", class: "box",
                    input {
                        r#type: "checkbox",
                        title: "{name} may {about}",
                        checked: now.iter().any(|n| n == p),
                        onchange: move |e: FormEvent| {
                            let mut c = chosen.write();
                            c.retain(|x| x != p);
                            if e.checked() {
                                c.push(p.to_string());
                            }
                        },
                    }
                }
            }
            td { "{accounts}" }
            td { class: "end",
                if dirty {
                    button { class: "small", disabled: busy(), onclick: save, "Save" }
                    " "
                    button { class: "small plain", onclick: move |_| chosen.set(first.clone()), "Undo" }
                } else if asking() {
                    span { class: "muted", "Remove {name}? " }
                    button { class: "link danger", onclick: remove, "Yes" }
                    " "
                    button { class: "link", onclick: move |_| asking.set(false), "No" }
                } else {
                    button {
                        class: "link danger",
                        disabled: accounts > 0,
                        title: if accounts > 0 { "Accounts have it: give them another role first" } else { "" },
                        onclick: move |_| asking.set(true),
                        "Remove"
                    }
                }
                if let Some(f) = failure.read().as_ref() {
                    div { class: "error", "{f}" }
                }
            }
        }
    }
}

/// What each type of provider is, as a person reads it.
const SOURCE_TYPES: &[(&str, &str)] = &[
    ("openai-compatible-api", "OpenAI-compatible server"),
    ("anthropic-api", "Anthropic's API"),
    (
        "runner",
        "Another runner, whose shared models are reached through the hub",
    ),
];

/// How strong an LLM is, as a person reads it.
const TIERS: &[(&str, &str)] = &[
    ("small", "Small: quick work, such as summing up"),
    ("medium", "Medium"),
    ("strong", "Strong: the hardest work"),
];

#[component]
fn Models() -> Element {
    let mut providers = use_op("provider.list");
    let mut models = use_op("model.list");
    let offered = use_op("runner.models");
    let (mut shown, _) = settings();
    let mut changed = move || {
        providers.restart();
        models.restart();
        shown.restart();
    };
    let mut runners: Vec<String> = Vec::new();
    if let Some(Ok(out)) = &*offered.read() {
        for o in out["shared"].as_array().into_iter().flatten() {
            let runner = text(o, "runner");
            if !runners.contains(&runner) {
                runners.push(runner);
            }
        }
    }
    let all_models = match &*models.read() {
        Some(Ok(out)) => out["models"].as_array().cloned().unwrap_or_default(),
        _ => Vec::new(),
    };
    let api_types = ["openai-compatible-api", "anthropic-api"];
    rsx! {
        p { class: "muted",
            "The hub's ANN pool: the providers of models it calls, and the models of each you added. Nothing a provider serves is in the pool until you add it. Keys are kept sealed, never in the settings."
        }
        {loaded(&providers.read(), |out| {
            let list = out["providers"].as_array().cloned().unwrap_or_default();
            rsx! {
                if list.is_empty() {
                    p { class: "muted", "No provider yet: add one below." }
                } else {
                    for s in list {
                        ProviderCard {
                            key: "{text(&s, \"name\")}",
                            provider: s.clone(),
                            models: all_models
                                .iter()
                                .filter(|m| m["provider"] == s["name"])
                                .cloned()
                                .collect::<Vec<_>>(),
                            on_changed: move |_| changed(),
                        }
                    }
                }
            }
        })}
        {loaded(&offered.read(), |out| {
            let list = out["shared"].as_array().cloned().unwrap_or_default();
            rsx! {
                if !list.is_empty() {
                    h3 { "Shared by the runners connected now" }
                    table {
                        thead { tr { th { "Runner" } th { "Models" } th { "Calls at once" } } }
                        tbody {
                            for o in list {
                                tr { key: "{text(&o, \"runner\")}/{text(&o, \"provider\")}",
                                    td { "{text(&o, \"runner\")}" }
                                    td {
                                        {o["models"]
                                            .as_array()
                                            .into_iter()
                                            .flatten()
                                            .map(|m| format!("{}:{}", text(&o, "provider"), text(m, "id")))
                                            .collect::<Vec<_>>()
                                            .join(", ")}
                                    }
                                    td { "{text(&o, \"max_concurrent\")}" }
                                }
                            }
                        }
                    }
                }
            }
        })}
        section { class: "panel",
            h3 { "Add a provider" }
            OpForm {
                op: "provider.add",
                picks: vec![
                    Pick::named("type", SOURCE_TYPES),
                    Pick::suggest("runner", runners),
                ],
                when: vec![
                    When::new("url", "type", &api_types),
                    When::new("key", "type", &api_types),
                    When::new("key_env", "type", &api_types),
                    When::new("max_concurrent", "type", &api_types),
                    When::new("timeout", "type", &api_types),
                    When::new("runner", "type", &["runner"]),
                ],
                more: vec!["key_env".to_string(), "timeout".to_string()],
                labels: vec![
                    label("url", "URL"),
                    label("key", "API key"),
                    label("key_env", "key from a variable"),
                    label("max_concurrent", "calls it takes at once"),
                    label("timeout", "timeout, in seconds"),
                    label("no_check", "do not ask it which models it serves first"),
                ],
                submit: "Add the provider",
                clear: true,
                on_done: move |_| changed(),
            }
        }
    }
}

/// A provider: what it is, where, its key, and the models of it added; tested,
/// given a model it serves, asked again which models it serves, given a key,
/// or taken out.
#[component]
fn ProviderCard(provider: Value, models: Vec<Value>, on_changed: EventHandler<()>) -> Element {
    let name = text(&provider, "name");
    let mut served = {
        let name = name.clone();
        use_resource(move || api::op("provider.models", json!({ "name": name.clone() })))
    };
    let addable: Vec<String> = match &*served.read() {
        Some(Ok(out)) => out["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["added"] != true)
            .map(|m| text(m, "name"))
            .collect(),
        _ => Vec::new(),
    };
    let mut tested = use_signal(|| None::<Result<Value, Failure>>);
    let mut keying = use_signal(|| false);
    let kind = text(&provider, "type");
    let shown_kind = SOURCE_TYPES
        .iter()
        .find(|(t, _)| *t == kind)
        .map(|(_, l)| l.to_string())
        .unwrap_or(kind.clone());
    let place = match text(&provider, "url") {
        u if !u.is_empty() => u,
        _ => text(&provider, "runner"),
    };
    let key = match (provider["key"] == true, text(&provider, "key_env")) {
        (_, env) if !env.is_empty() => format!("from ${env}"),
        (true, _) => "kept sealed".into(),
        (false, _) => "none".into(),
    };
    let test = {
        let name = name.clone();
        move |_| {
            let name = name.clone();
            spawn(async move {
                tested.set(None);
                let out = api::op("provider.test", json!({ "name": name })).await;
                tested.set(Some(out));
            });
        }
    };
    let named = vec![("name".to_string(), json!(name))];
    let direct = kind != "runner";
    rsx! {
        section { class: "panel provider",
            div { class: "head",
                h3 { "{name}" }
                span { class: "muted", "{shown_kind}, {place}" }
            }
            dl { class: "facts",
                if direct {
                    dt { "Key" } dd { "{key}" }
                    dt { "Calls at once" } dd { "{text(&provider, \"max_concurrent\")}" }
                }
            }
            if models.is_empty() {
                p { class: "muted", "None of its models is in the pool yet: add one below." }
            } else {
                table {
                    thead { tr { th { "Model" } th { "What it is" } th { "" } } }
                    tbody {
                        for m in models {
                            ModelRow { key: "{text(&m, \"name\")}", model: m.clone(), on_changed: move |_| on_changed.call(()) }
                        }
                    }
                }
            }
            OpForm {
                op: "model.add",
                picks: vec![Pick::suggest("model", addable), Pick::named("tier", TIERS)],
                labels: vec![
                    label("model", "model, as <provider>:<model>"),
                    label("tier", "how strong it is, for an LLM"),
                    label("context_window", "tokens of context its server runs it with"),
                    label("tools", "makes tool calls"),
                ],
                more: vec!["tier".to_string(), "context_window".to_string(), "tools".to_string()],
                submit: "Add the model",
                clear: true,
                on_done: move |_| {
                    served.restart();
                    on_changed.call(());
                },
            }
            div { class: "actions",
                button { class: "small plain", onclick: test, "Test" }
                if direct {
                    button { class: "small plain", onclick: move |_| keying.toggle(), "Change the key" }
                }
            }
            match &*tested.read() {
                Some(Ok(out)) => {
                    let row = out["providers"].get(0).cloned().unwrap_or(Value::Null);
                    match row["ok"].as_bool() {
                        Some(true) => rsx! { p { class: "ok", "Answers; serves {lookup::strings(&row[\"models\"]).join(\", \")}" } },
                        Some(false) => rsx! { p { class: "error", "{text(&row, \"error\")}" } },
                        None => rsx! { p { class: "muted", "{text(out, \"said\")}" } },
                    }
                }
                Some(Err(e)) => rsx! { p { class: "error", "{e}" } },
                None => rsx! {},
            }
            if keying() {
                OpForm {
                    op: "provider.key",
                    fixed: named.clone(),
                    labels: vec![label("key", "new API key (empty takes it out)"), label("key_env", "key from a variable")],
                    more: vec!["key_env".to_string()],
                    submit: "Save the key",
                    saved: true,
                    clear: true,
                    on_done: move |_| on_changed.call(()),
                }
            }
            OpForm {
                op: "provider.refresh",
                fixed: named.clone(),
                submit: "Ask it again which of its models it still serves",
                saved: true,
                on_done: move |_| on_changed.call(()),
            }
            OpForm {
                op: "provider.remove",
                fixed: named.clone(),
                submit: "Remove",
                danger: true,
                confirm: "Remove {name}, with the models of it added and the key kept for it? Refused while something uses one of its models.",
                quiet: true,
                on_done: move |_| on_changed.call(()),
            }
        }
    }
}

/// A model of a provider in the pool: what it is; said how strong it is, or
/// taken out.
#[component]
fn ModelRow(model: Value, on_changed: EventHandler<()>) -> Element {
    let name = text(&model, "name");
    let named = vec![("model".to_string(), json!(name))];
    let llm = text(&model, "type") == "llm";
    let mut what = match text(&model, "type").as_str() {
        "llm" => {
            let mut said = format!("LLM, {}", text(&model, "tier"));
            if let Some(n) = model["context_window"].as_u64() {
                said.push_str(&format!(", {n} tokens"));
            }
            match model["tools"].as_bool() {
                Some(true) => said.push_str(", makes tool calls"),
                Some(false) => said.push_str(", no tool calls"),
                None => {}
            }
            said
        }
        _ => format!("embedding model, {} dimensions", text(&model, "dimensions")),
    };
    if model["missing"] == true {
        what.push_str("; its provider no longer lists it");
    }
    rsx! {
        tr {
            td { "{name}" }
            td { "{what}" }
            td {
                if llm {
                        OpForm {
                            op: "model.set",
                            fixed: named.clone(),
                            values: vec![value("tier", text(&model, "tier"))],
                            picks: vec![Pick::named("tier", TIERS)],
                            labels: vec![
                                label("tier", "how strong it is"),
                                label("context_window", "tokens of context its server runs it with"),
                                label("tools", "makes tool calls"),
                            ],
                            more: vec!["context_window".to_string(), "tools".to_string()],
                            submit: "Save",
                            saved: true,
                            on_done: move |_| on_changed.call(()),
                        }
                }
                OpForm {
                    op: "model.remove",
                    fixed: named.clone(),
                    submit: "Remove",
                    danger: true,
                    confirm: "Take {name} out of the pool? Refused while something uses it.",
                    quiet: true,
                    on_done: move |_| on_changed.call(()),
                }
            }
        }
    }
}

#[component]
fn KnowledgeModels() -> Element {
    let (mut shown, now) = settings();
    let pool = use_op("model.list");
    let Some(now) = now else {
        return waiting(&shown);
    };
    let llms = lookup::pool_models(&pool, "llm");
    let embeddings = lookup::pool_models(&pool, "embedding");
    let distill = &now["distill"];
    let embed = &now["embed"];
    let embed_model = if embed.is_null() {
        String::new()
    } else {
        format!("{}:{}", text(embed, "provider"), text(embed, "model"))
    };
    let pick = |field: &str, options: &Vec<(String, String)>| Pick {
        field: field.into(),
        options: options.clone(),
        strict: false,
    };
    rsx! {
        section { class: "panel",
            h3 { "Notes" }
            p { class: "muted", "The models that turn shares into notes, tried in order." }
            OpForm {
                op: "knowledge.models",
                values: vec![
                    value("models", model_lines(&distill["try"])),
                    value("when_none", text(distill, "when_none")),
                    value("max_tokens", text(distill, "max_tokens")),
                ],
                picks: vec![pick("models", &llms), when_none("Fail the share into review/")],
                labels: vec![label("when_none", "when none can be used"), label("max_tokens", "most tokens of one answer")],
                more: vec!["max_tokens".to_string()],
                submit: "Save",
                saved: true,
                on_done: move |_| shown.restart(),
            }
        }
        section { class: "panel",
            h3 { "Finding notes by meaning" }
            OpForm {
                op: "knowledge.embedding",
                values: vec![
                    value("off", embed.is_null()),
                    value("model", embed_model),
                    value("min_similarity", if embed.is_null() { String::new() } else { text(embed, "min_similarity") }),
                ],
                picks: vec![pick("model", &embeddings)],
                when: vec![When::new("model", "off", &[""]), When::new("min_similarity", "off", &[""])],
                labels: vec![
                    label("off", "no embedding model: find notes by their words alone"),
                    label("model", "embedding model"),
                    label("min_similarity", "least similarity, 0 to 1"),
                ],
                submit: "Save",
                saved: true,
                on_done: move |_| shown.restart(),
            }
        }
        section { class: "panel",
            h3 { "Recall" }
            OpForm {
                op: "knowledge.recall",
                values: vec![
                    value("max_notes", text(&now, "recall_max_notes")),
                    value("max_chars", text(&now, "recall_max_chars")),
                ],
                labels: vec![label("max_notes", "most notes"), label("max_chars", "most characters, all notes together")],
                submit: "Save",
                saved: true,
                on_done: move |_| shown.restart(),
            }
        }
    }
}

#[component]
fn Sources() -> Element {
    let (mut shown, now) = settings();
    let pool = use_op("model.list");
    let Some(now) = now else {
        return waiting(&shown);
    };
    let models = lookup::pool_models(&pool, "llm");
    let filter = &now["source_filter"];
    let used = &filter["models"];
    rsx! {
        section { class: "panel",
            h3 { "Filter" }
            p { class: "muted",
                "What sources bring in is collected by a model, which first takes out what is confidential. Without a filter, no source may be connected."
            }
            OpForm {
                op: "source.filter",
                values: vec![
                    value("off", filter.is_null()),
                    value("models", model_lines(&used["try"])),
                    value("prompt", text(filter, "prompt")),
                    value("when_none", text(used, "when_none")),
                    value("max_tokens", text(used, "max_tokens")),
                ],
                picks: vec![
                    Pick { field: "models".into(), options: models.clone(), strict: false },
                    when_none("Drop what came in"),
                ],
                when: ["models", "prompt", "when_none", "max_tokens"]
                    .iter()
                    .map(|f| When::new(f, "off", &[""]))
                    .collect::<Vec<_>>(),
                labels: vec![
                    label("off", "no filter"),
                    label("prompt", "what is confidential"),
                    label("when_none", "when none can be used"),
                    label("max_tokens", "most tokens of one answer"),
                ],
                more: vec!["max_tokens".to_string()],
                submit: "Save",
                saved: true,
                on_done: move |_| shown.restart(),
            }
        }
        if !filter.is_null() {
            section { class: "panel",
                h3 { "Try it" }
                OpForm { op: "source.try", submit: "Filter this text", summary: true }
            }
        }
    }
}

/// What each kind of tracker is, as a person reads it.
const TRACKER_TYPES: &[(&str, &str)] = &[("local_markdown", "A folder of markdown issues")];

#[component]
fn Trackers() -> Element {
    let mut trackers = use_op("tracker.list");
    let (mut shown, _) = settings();
    let mut changed = move || {
        trackers.restart();
        shown.restart();
    };
    rsx! {
        p { class: "muted", "Issues with the tracker's label become tasks of the people they are assigned to." }
        {loaded(&trackers.read(), |out| {
            let list = out["trackers"].as_array().cloned().unwrap_or_default();
            rsx! {
                if list.is_empty() {
                    p { class: "muted", "No tracker: tasks come from client agents alone." }
                } else {
                    table {
                        thead { tr { th { "Name" } th { "Kind" } th { "Where" } th { "Label" } th { "Every" } th {} } }
                        tbody {
                            for t in list {
                                tr { key: "{text(&t, \"name\")}",
                                    td { strong { "{text(&t, \"name\")}" } }
                                    td { "{text(&t, \"type\")}" }
                                    td { code { "{text(&t, \"dir\")}" } }
                                    td { "{text(&t, \"label\")}" }
                                    td { "{text(&t, \"poll_seconds\")}s" }
                                    td { class: "end",
                                        OpForm {
                                            op: "tracker.remove",
                                            fixed: vec![("name".to_string(), json!(text(&t, "name")))],
                                            submit: "Remove",
                                            danger: true,
                                            confirm: "Stop watching {text(&t, \"name\")}? The hub starts again.",
                                            quiet: true,
                                            on_done: move |_| changed(),
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        })}
        section { class: "panel",
            h3 { "Add a tracker" }
            p { class: "muted", "The hub starts again to use it; runners connect again by themselves." }
            OpForm {
                op: "tracker.add",
                values: vec![value("type", "local_markdown")],
                picks: vec![Pick::named("type", TRACKER_TYPES)],
                when: vec![When::new("dir", "type", &["local_markdown"])],
                labels: vec![label("dir", "folder"), label("poll_seconds", "look every, in seconds")],
                more: vec!["label".to_string(), "poll_seconds".to_string()],
                submit: "Add the tracker",
                clear: true,
                on_done: move |_| changed(),
            }
        }
    }
}

#[component]
fn History() -> Element {
    let history = use_op("settings.history");
    let (shown, now) = settings();
    rsx! {
        {loaded(&history.read(), |out| {
            let mut list = out["changes"].as_array().cloned().unwrap_or_default();
            list.reverse();
            rsx! {
                table {
                    thead { tr { th { "When" } th { "Who" } th { "Operation" } th { "With" } } }
                    tbody {
                        for (i, c) in list.into_iter().enumerate() {
                            tr { key: "{i}",
                                td { "{when(&text(&c, \"at\"))}" }
                                td { "{text(&c, \"who\")}" }
                                td { code { "{text(&c, \"operation\")}" } }
                                td { class: "muted small", "{c[\"input\"]}" }
                            }
                        }
                    }
                }
            }
        })}
        details {
            summary { "Every setting, as the hub keeps it" }
            match now {
                Some(now) => rsx! { Json { value: now } },
                None => waiting(&shown),
            }
        }
    }
}

#[component]
fn Other() -> Element {
    let catalog = use_context::<Catalog>();
    rsx! {
        p { class: "muted", "Operations of the settings no section shows yet." }
        {loaded(&catalog.read(), |listed| {
            let ops: Vec<Value> = listed["operations"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|o| {
                    let name = text(o, "name");
                    GROUPS.contains(&name.split('.').next().unwrap_or(""))
                        && !SHOWN.contains(&name.as_str())
                })
                .cloned()
                .collect();
            rsx! {
                if ops.is_empty() {
                    p { "None: every one has its section." }
                }
                ul { class: "list",
                    for op in ops {
                        li { key: "{text(&op, \"name\")}",
                            Link { to: Route::OpPage { op: text(&op, "name") }, "{text(&op, \"name\")}" }
                            span { class: "muted", " {text(&op, \"summary\")}" }
                        }
                    }
                }
            }
        })}
    }
}

#[component]
pub fn OpPage(op: String) -> Element {
    rsx! {
        p { class: "crumbs",
            Link { to: Route::Settings {}, "Settings" }
            " / "
            Link { to: Route::SettingsPage { section: "other".into() }, "Other operations" }
            " / {op}"
        }
        h1 { "{op}" }
        // A form of its own for each operation, not one that keeps what was
        // typed for another.
        {rsx! { OpForm { key: "{op}", op: op.clone(), summary: true } }}
    }
}
