//! The hub's settings, a section each, with what each is now: the hub and
//! its logins and sign-ups, the roles, the LLM backends, the models of the
//! knowledge vault, the source filter, the trackers, and who changed what.
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
    ("backends", "Model backends"),
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
    "backend.list",
    "backend.add",
    "backend.remove",
    "backend.test",
    "backend.key",
    "runner.backends",
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
    "backend",
    "runner",
    "knowledge",
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
                    "backends" => rsx! { Backends {} },
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

/// Models as a list of `<backend>:<model>`, one per line, from the settings'
/// `[{backend, model}]`.
fn model_lines(tried: &Value) -> String {
    tried
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| format!("{}:{}", text(m, "backend"), text(m, "model")))
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

/// What each kind of backend is, as a person reads it.
const BACKEND_TYPES: &[(&str, &str)] = &[
    ("openai-compatible-api", "OpenAI-compatible server"),
    ("anthropic-api", "Anthropic's API"),
    ("runner", "A runner's, shared through the hub"),
];

#[component]
fn Backends() -> Element {
    let mut backends = use_op("backend.list");
    let offered = use_op("runner.backends");
    let (mut shown, _) = settings();
    let mut changed = move || {
        backends.restart();
        shown.restart();
    };
    let mut runners: Vec<String> = Vec::new();
    let mut shared: Vec<String> = Vec::new();
    if let Some(Ok(out)) = &*offered.read() {
        for o in out["backends"].as_array().into_iter().flatten() {
            let (runner, backend) = (text(o, "runner"), text(o, "backend"));
            if !runners.contains(&runner) {
                runners.push(runner);
            }
            if !shared.contains(&backend) {
                shared.push(backend);
            }
        }
    }
    let api_types = ["openai-compatible-api", "anthropic-api"];
    rsx! {
        p { class: "muted", "The LLM servers the hub's models are on. Keys are kept sealed, never in the settings." }
        {loaded(&backends.read(), |out| {
            let list = out["backends"].as_array().cloned().unwrap_or_default();
            rsx! {
                if list.is_empty() {
                    p { class: "muted", "No backend yet: add one below." }
                } else {
                    for b in list {
                        BackendCard { key: "{text(&b, \"name\")}", backend: b.clone(), on_changed: move |_| changed() }
                    }
                }
            }
        })}
        {loaded(&offered.read(), |out| {
            let list = out["backends"].as_array().cloned().unwrap_or_default();
            rsx! {
                if !list.is_empty() {
                    h3 { "Shared by the runners connected now" }
                    table {
                        thead { tr { th { "Runner" } th { "Backend" } th { "Works for" } th { "Models" } } }
                        tbody {
                            for o in list {
                                tr { key: "{text(&o, \"runner\")}/{text(&o, \"backend\")}",
                                    td { "{text(&o, \"runner\")}" }
                                    td { "{text(&o, \"backend\")}" }
                                    td { "{text(&o, \"works_for\")}" }
                                    td { "{lookup::strings(&o[\"models\"]).join(\", \")}" }
                                }
                            }
                        }
                    }
                }
            }
        })}
        section { class: "panel",
            h3 { "Add a backend" }
            OpForm {
                op: "backend.add",
                picks: vec![
                    Pick::named("type", BACKEND_TYPES),
                    Pick::suggest("runner", runners),
                    Pick::suggest("runner_backend", shared),
                ],
                when: vec![
                    When::new("url", "type", &api_types),
                    When::new("key", "type", &api_types),
                    When::new("key_env", "type", &api_types),
                    When::new("runner", "type", &["runner"]),
                    When::new("runner_backend", "type", &["runner"]),
                    When::new("timeout", "type", &["openai-compatible-api", "anthropic-api", "runner"]),
                ],
                more: vec!["key_env".to_string(), "timeout".to_string()],
                labels: vec![
                    label("url", "URL"),
                    label("key", "API key"),
                    label("key_env", "key from a variable"),
                    label("runner_backend", "its name on the runner"),
                    label("timeout", "timeout, in seconds"),
                    label("no_check", "do not ask it which models it serves first"),
                ],
                submit: "Add the backend",
                clear: true,
                on_done: move |_| changed(),
            }
        }
    }
}

/// A backend: what it is, where, its key; tested, given a key, or taken out.
#[component]
fn BackendCard(backend: Value, on_changed: EventHandler<()>) -> Element {
    let name = text(&backend, "name");
    let mut tested = use_signal(|| None::<Result<Value, Failure>>);
    let mut keying = use_signal(|| false);
    let kind = text(&backend, "type");
    let shown_kind = BACKEND_TYPES
        .iter()
        .find(|(t, _)| *t == kind)
        .map(|(_, l)| l.to_string())
        .unwrap_or(kind.clone());
    let place = match text(&backend, "url") {
        u if !u.is_empty() => u,
        _ => format!("on {}", text(&backend, "runner")),
    };
    let key = match (backend["key"] == true, text(&backend, "key_env")) {
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
                let out = api::op("backend.test", json!({ "name": name })).await;
                tested.set(Some(out));
            });
        }
    };
    let named = vec![("name".to_string(), json!(name))];
    let direct = kind != "runner";
    rsx! {
        section { class: "panel backend",
            div { class: "head",
                h3 { "{name}" }
                span { class: "muted", "{shown_kind}, {place}" }
            }
            dl { class: "facts",
                dt { "Key" } dd { "{key}" }
                if backend["shared"] == true {
                    dt { "Shared" } dd { "offered to runners through the hub" }
                }
            }
            div { class: "actions",
                button { class: "small plain", onclick: test, "Test" }
                if direct {
                    button { class: "small plain", onclick: move |_| keying.toggle(), "Change the key" }
                }
            }
            match &*tested.read() {
                Some(Ok(out)) => {
                    let row = out["backends"].get(0).cloned().unwrap_or(Value::Null);
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
                    op: "backend.key",
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
                op: "backend.remove",
                fixed: named.clone(),
                submit: "Remove",
                danger: true,
                confirm: "Remove {name}, and the key kept for it? Refused while something uses it.",
                quiet: true,
                on_done: move |_| on_changed.call(()),
            }
        }
    }
}

#[component]
fn KnowledgeModels() -> Element {
    let (mut shown, now) = settings();
    let backends = use_op("backend.list");
    let tested = use_op("backend.test");
    let Some(now) = now else {
        return waiting(&shown);
    };
    let models = lookup::models(&lookup::backends(&backends), &tested);
    let distill = &now["distill"];
    let embed = &now["embed"];
    let embed_model = if embed.is_null() {
        String::new()
    } else {
        format!("{}:{}", text(embed, "backend"), text(embed, "model"))
    };
    let pick = |field: &str| Pick {
        field: field.into(),
        options: models.clone(),
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
                picks: vec![pick("models"), when_none("Fail the share into review/")],
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
                picks: vec![pick("model")],
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
    let backends = use_op("backend.list");
    let tested = use_op("backend.test");
    let Some(now) = now else {
        return waiting(&shown);
    };
    let models = lookup::models(&lookup::backends(&backends), &tested);
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
