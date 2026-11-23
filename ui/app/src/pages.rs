//! The app's pages. Each reads and changes the hub through its operations
//! ([`api`]); the forms are made from them ([`OpForm`]).

use crate::api::{self, Failure};
use crate::form::{Answer, Json, OpForm};
use crate::{Me, Route};
use dioxus::prelude::*;
use serde_json::{Value, json};

/// What an operation's answer is shown as while it loads, once it failed,
/// and once it came.
fn loaded(
    answer: &Option<Result<Value, Failure>>,
    show: impl FnOnce(&Value) -> Element,
) -> Element {
    match answer {
        None => rsx! { p { class: "loading", "Loading…" } },
        Some(Err(e)) => rsx! { p { class: "error", "{e}" } },
        Some(Ok(value)) => show(value),
    }
}

/// A text field of a value, or empty.
fn text(value: &Value, key: &str) -> String {
    match &value[key] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn me() -> Me {
    use_context::<Signal<Option<Me>>>()
        .read()
        .clone()
        .unwrap_or(Me(Value::Null))
}

#[component]
pub fn Login() -> Element {
    let mut session = use_context::<Signal<Option<Me>>>();
    let mut name = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut failure = use_signal(|| None::<Failure>);
    let nav = navigator();
    let submit = move |e: FormEvent| {
        e.prevent_default();
        spawn(async move {
            failure.set(None);
            match api::sign_in(&name.read(), &password.read()).await {
                Ok(_) => match api::me().await {
                    Ok(value) => {
                        session.set(Some(Me(value)));
                        nav.replace(Route::Home {});
                    }
                    Err(e) => failure.set(Some(e)),
                },
                Err(e) => failure.set(Some(e)),
            }
        });
    };
    rsx! {
        main { class: "login",
            h1 { "Ensemble" }
            form { class: "op", onsubmit: submit,
                label { class: "field",
                    span { class: "name", "Name" }
                    input {
                        autocomplete: "username",
                        value: "{name}",
                        oninput: move |e| name.set(e.value()),
                    }
                }
                label { class: "field",
                    span { class: "name", "Password" }
                    input {
                        r#type: "password",
                        autocomplete: "current-password",
                        value: "{password}",
                        oninput: move |e| password.set(e.value()),
                    }
                }
                div { class: "actions", button { r#type: "submit", "Sign in" } }
                if let Some(f) = failure.read().as_ref() {
                    p { class: "error", "{f}" }
                }
            }
            p { class: "muted",
                "No password yet? An admin sets one with "
                code { "ensemble-hub user password <name>" }
                "."
            }
        }
    }
}

#[component]
pub fn Home() -> Element {
    let me = me();
    let permissions: Vec<String> = me.0["permissions"]
        .as_array()
        .map(|p| {
            p.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    rsx! {
        h1 { "Hello, {me.name()}" }
        p { class: "muted", "Role {text(&me.0, \"role\")}; you may: {permissions.join(\", \")}." }
        div { class: "cards",
            if me.can("tasks") {
                Link { class: "card", to: Route::Projects {},
                    h2 { "Tasks" }
                    p { "What your helper team works on, by project." }
                }
                Link { class: "card", to: Route::Runners {},
                    h2 { "Runners" }
                    p { "The runners connected now that work for you, and their agents." }
                }
            }
            if me.can("recall") || me.can("share") {
                Link { class: "card", to: Route::Knowledge {},
                    h2 { "Knowledge" }
                    p { "Recall notes about a topic, or share what is worth keeping." }
                }
            }
            Link { class: "card", to: Route::Account {},
                h2 { "Account" }
                p { "Your tokens, join codes and password." }
            }
            if me.can("admin") {
                Link { class: "card", to: Route::Users {},
                    h2 { "Users" }
                    p { "Accounts, sign-ups and roles." }
                }
                Link { class: "card", to: Route::Settings {},
                    h2 { "Settings" }
                    p { "The hub's settings, backends, models and trackers." }
                }
            }
        }
    }
}

#[component]
pub fn Projects() -> Element {
    let mut projects = use_resource(|| api::op("task.projects", json!({})));
    rsx! {
        h1 { "Tasks" }
        {loaded(&projects.read(), |out| {
            let list = out["projects"].as_array().cloned().unwrap_or_default();
            rsx! {
                if list.is_empty() {
                    p { class: "muted", "No tasks yet." }
                }
                ul { class: "list",
                    for p in list {
                        li { key: "{text(&p, \"project\")}",
                            Link {
                                to: Route::ProjectTasks { project: text(&p, "project") },
                                "{text(&p, \"project\")}"
                            }
                            span { class: "muted", " {text(&p, \"tasks\")} task(s)" }
                        }
                    }
                }
            }
        })}
        h2 { "New task" }
        OpForm {
            op: "task.create",
            submit: "Make the task",
            quiet: true,
            on_done: move |_| projects.restart(),
        }
    }
}

/// A task's state, in a word.
fn status(task: &Value) -> String {
    if task["blocked"] == true {
        return "blocked".into();
    }
    if !task["question"].is_null() {
        return "asks".into();
    }
    if !task["running_on"].is_null() {
        return "running".into();
    }
    match task["status"].as_str() {
        Some(s) => s.to_string(),
        None if text(&task["ticket"], "agent").is_empty() => "backlog".into(),
        None => "waiting".into(),
    }
}

#[component]
pub fn ProjectTasks(project: String) -> Element {
    let shown = project.clone();
    let mut tasks = use_resource(move || {
        let project = shown.clone();
        async move { api::op("task.list", json!({ "project": project })).await }
    });
    rsx! {
        p { class: "crumbs", Link { to: Route::Projects {}, "Tasks" } " / {project}" }
        h1 { "{project}" }
        {loaded(&tasks.read(), |out| {
            let list = out["tasks"].as_array().cloned().unwrap_or_default();
            rsx! {
                table {
                    thead { tr { th { "Task" } th { "Title" } th { "Agent" } th { "State" } } }
                    tbody {
                        for task in list {
                            tr { key: "{text(&task[\"ticket\"], \"id\")}",
                                td {
                                    Link {
                                        to: Route::TaskPage {
                                            project: project.clone(),
                                            id: text(&task["ticket"], "id"),
                                        },
                                        "{text(&task[\"ticket\"], \"id\")}"
                                    }
                                }
                                td { "{text(&task[\"ticket\"], \"title\")}" }
                                td { "{text(&task[\"ticket\"], \"agent\")}" }
                                td { span { class: "state {status(&task)}", "{status(&task)}" } }
                            }
                        }
                    }
                }
            }
        })}
        h2 { "New task in {project}" }
        OpForm {
            op: "task.create",
            fixed: vec![("project".to_string(), json!(project))],
            submit: "Make the task",
            quiet: true,
            on_done: move |_| tasks.restart(),
        }
    }
}

#[component]
pub fn TaskPage(project: String, id: String) -> Element {
    let (p, i) = (project.clone(), id.clone());
    let mut task = use_resource(move || {
        let (project, id) = (p.clone(), i.clone());
        async move { api::op("task.show", json!({ "project": project, "id": id })).await }
    });
    let on_task = vec![
        ("project".to_string(), json!(project)),
        ("id".to_string(), json!(id)),
    ];
    rsx! {
        p { class: "crumbs",
            Link { to: Route::Projects {}, "Tasks" }
            " / "
            Link { to: Route::ProjectTasks { project: project.clone() }, "{project}" }
            " / {id}"
        }
        {loaded(&task.read(), |out| {
            let detail = &out["task"];
            let task = if detail["task"].is_null() { detail } else { &detail["task"] };
            let ticket = &task["ticket"];
            let agent = match text(ticket, "agent") {
                agent if !agent.is_empty() => format!("for {agent}"),
                _ if status(task) == "backlog" => "in the backlog until it is given to an agent".into(),
                _ => "for no agent".into(),
            };
            let question = text(task, "question");
            rsx! {
                h1 { "{text(ticket, \"title\")}" }
                p { class: "muted",
                    span { class: "state {status(task)}", "{status(task)}" }
                    " {id}, {agent}"
                }
                if !question.is_empty() {
                    p { class: "notice", strong { "It asks: " } "{question}" }
                }
                pre { class: "text", "{text(ticket, \"text\")}" }
                details {
                    summary { "Everything the hub keeps of it" }
                    Json { value: detail.clone() }
                }
            }
        })}
        div { class: "columns",
            section {
                h2 { "Answer its question" }
                OpForm {
                    op: "task.answer",
                    fixed: on_task.clone(),
                    submit: "Answer",
                    on_done: move |_| task.restart(),
                }
            }
            section {
                h2 { "Give it to an agent" }
                OpForm {
                    op: "task.assign",
                    fixed: on_task.clone(),
                    submit: "Assign",
                    on_done: move |_| task.restart(),
                }
            }
            section {
                h2 { "Cancel it" }
                OpForm {
                    op: "task.cancel",
                    fixed: on_task.clone(),
                    submit: "Cancel the task",
                    on_done: move |_| task.restart(),
                }
            }
        }
    }
}

#[component]
pub fn Runners() -> Element {
    let runners = use_resource(|| api::op("runner.list", json!({})));
    rsx! {
        h1 { "Runners" }
        {loaded(&runners.read(), |out| {
            let list = out["runners"].as_array().cloned().unwrap_or_default();
            rsx! {
                if list.is_empty() {
                    p { class: "muted",
                        "No runner that works for you is connected. On a computer that should work: "
                        code { "ensemble-runner join <hub> <code>" }
                        ", with a code from your account."
                    }
                }
                for runner in list {
                    section { class: "card", key: "{text(&runner, \"runner\")}",
                        h2 { "{text(&runner, \"runner\")}" }
                        p { class: "muted", "works for its {text(&runner, \"works_for\")}" }
                        ul {
                            for agent in runner["agents"].as_array().cloned().unwrap_or_default() {
                                li { key: "{text(&agent, \"name\")}",
                                    strong { "{text(&agent, \"name\")}" }
                                    " {text(&agent, \"about\")}"
                                }
                            }
                        }
                    }
                }
            }
        })}
    }
}

#[component]
pub fn Knowledge() -> Element {
    let me = me();
    let mut found = use_signal(|| None::<Value>);
    rsx! {
        h1 { "Knowledge" }
        if me.can("recall") {
            h2 { "Recall" }
            OpForm {
                op: "knowledge.find",
                submit: "Recall",
                quiet: true,
                on_done: move |out: Value| found.set(Some(out)),
            }
            if let Some(out) = found.read().as_ref() {
                pre { class: "text", "{text(out, \"text\")}" }
            }
        }
        if me.can("share") {
            h2 { "Share" }
            OpForm { op: "knowledge.share", submit: "Share" }
        }
    }
}

#[component]
pub fn Account() -> Element {
    let me = me();
    let mut tokens = use_resource(|| api::op("me.tokens", json!({})));
    rsx! {
        h1 { "{me.name()}" }
        p { class: "muted",
            "Role {text(&me.0, \"role\")}; in the tracker, {text(&me.0, \"tracker_name\")}."
        }
        h2 { "Access tokens" }
        p { class: "muted",
            "For your agents (as the API key of the hub's MCP), runners and scripts. Each is shown once."
        }
        {loaded(&tokens.read(), |out| {
            let list = out["tokens"].as_array().cloned().unwrap_or_default();
            rsx! {
                table {
                    thead { tr { th { "Name" } th { "Scopes" } th { "Made" } th { "Expires" } th {} } }
                    tbody {
                        for token in list {
                            TokenRow { key: "{text(&token, \"id\")}", token: token.clone(), on_revoked: move |_| tokens.restart() }
                        }
                    }
                }
            }
        })}
        div { class: "columns",
            section {
                h3 { "New token" }
                OpForm { op: "me.token", submit: "Make a token", on_done: move |_| tokens.restart() }
            }
            section {
                h3 { "Join code" }
                p { class: "muted", "Signs one client in, such as a runner, within 24 hours." }
                OpForm { op: "me.join-code", submit: "Make a join code" }
            }
        }
        div { class: "columns",
            section {
                h3 { "Password" }
                OpForm { op: "me.password", submit: "Change the password" }
            }
            section {
                h3 { "Tracker name" }
                OpForm { op: "me.tracker", submit: "Save" }
            }
        }
    }
}

#[component]
fn TokenRow(token: Value, on_revoked: EventHandler<()>) -> Element {
    let id = text(&token, "id");
    let mut failure = use_signal(|| None::<Failure>);
    let revoke = move |_| {
        let id = id.clone();
        spawn(async move {
            match api::op("me.revoke", json!({ "id": id })).await {
                Ok(_) => on_revoked.call(()),
                Err(e) => failure.set(Some(e)),
            }
        });
    };
    let scopes = token["scopes"]
        .as_array()
        .map(|s| {
            s.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| "its role".into());
    rsx! {
        tr {
            td { "{text(&token, \"name\")}" }
            td { "{scopes}" }
            td { "{text(&token, \"made\")}" }
            td { "{text(&token, \"expires\")}" }
            td {
                button { class: "link danger", onclick: revoke, "Revoke" }
                if let Some(f) = failure.read().as_ref() {
                    span { class: "error", " {f}" }
                }
            }
        }
    }
}

#[component]
pub fn Users() -> Element {
    let mut users = use_resource(|| api::op("user.list", json!({})));
    let roles = use_resource(|| api::op("role.list", json!({})));
    rsx! {
        h1 { "Users" }
        {loaded(&users.read(), |out| {
            let list = out["users"].as_array().cloned().unwrap_or_default();
            rsx! {
                table {
                    thead { tr { th { "Name" } th { "Role" } th { "Status" } th { "Tokens" } } }
                    tbody {
                        for user in list {
                            tr { key: "{text(&user, \"name\")}",
                                td { Link { to: Route::UserPage { name: text(&user, "name") }, "{text(&user, \"name\")}" } }
                                td { "{text(&user, \"role\")}" }
                                td { span { class: "state {text(&user, \"status\")}", "{text(&user, \"status\")}" } }
                                td { "{text(&user, \"tokens\")}" }
                            }
                        }
                    }
                }
            }
        })}
        div { class: "columns",
            section {
                h2 { "Add a user" }
                OpForm { op: "user.add", submit: "Add", on_done: move |_| users.restart() }
            }
            section {
                h2 { "Roles" }
                {loaded(&roles.read(), |out| rsx! { Json { value: out.clone() } })}
            }
        }
    }
}

#[component]
pub fn UserPage(name: String) -> Element {
    let shown = name.clone();
    let mut user = use_resource(move || {
        let name = shown.clone();
        async move { api::op("user.show", json!({ "name": name })).await }
    });
    let named = vec![("name".to_string(), json!(name))];
    let actions: [(&str, &str, &str); 8] = [
        ("user.approve", "Approve the sign-up", "Approve"),
        ("user.role", "Give another role", "Change the role"),
        ("user.password", "Set the password", "Set"),
        ("user.tracker", "Name in the tracker", "Save"),
        ("user.token", "New token", "Make a token"),
        ("user.join-code", "Join code", "Make a join code"),
        ("user.revoke", "Revoke every token", "Revoke all"),
        ("user.remove", "Remove the account", "Remove"),
    ];
    rsx! {
        p { class: "crumbs", Link { to: Route::Users {}, "Users" } " / {name}" }
        h1 { "{name}" }
        {loaded(&user.read(), |out| rsx! { Answer { value: out.clone() } })}
        div { class: "grid",
            for (op, title, submit) in actions {
                section { key: "{op}",
                    h3 { "{title}" }
                    OpForm {
                        op: op,
                        fixed: named.clone(),
                        submit: submit,
                        on_done: move |_| user.restart(),
                    }
                }
            }
        }
    }
}

/// The groups of the hub's settings operations, as the settings page lists
/// them.
const GROUPS: &[(&str, &str)] = &[
    ("settings", "The hub"),
    ("registration", "Sign-ups"),
    ("role", "Roles"),
    ("backend", "Model backends"),
    ("knowledge", "Knowledge"),
    ("source", "Sources"),
    ("tracker", "Trackers"),
];

/// Operations a page of their own shows elsewhere.
const ELSEWHERE: &[&str] = &["knowledge.share", "knowledge.find", "settings.show"];

#[component]
pub fn Settings() -> Element {
    let shown = use_resource(|| api::op("settings.show", json!({})));
    let catalog = use_context::<crate::form::Catalog>();
    rsx! {
        h1 { "Settings" }
        {loaded(&catalog.read(), |listed| {
            let ops = listed["operations"].as_array().cloned().unwrap_or_default();
            rsx! {
                div { class: "grid",
                    for (prefix, title) in GROUPS.iter().copied() {
                        section { key: "{prefix}",
                            h2 { "{title}" }
                            ul { class: "list",
                                for op in ops.iter().filter(|o| {
                                    let name = text(o, "name");
                                    name.split('.').next() == Some(prefix) && !ELSEWHERE.contains(&name.as_str())
                                }).cloned() {
                                    li { key: "{text(&op, \"name\")}",
                                        Link { to: Route::OpPage { op: text(&op, "name") }, "{text(&op, \"name\")}" }
                                        span { class: "muted", " {text(&op, \"summary\")}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        })}
        h2 { "Every setting" }
        {loaded(&shown.read(), |out| rsx! { Json { value: out.clone() } })}
    }
}

#[component]
pub fn OpPage(op: String) -> Element {
    rsx! {
        p { class: "crumbs", Link { to: Route::Settings {}, "Settings" } " / {op}" }
        h1 { "{op}" }
        // A form of its own for each operation, not one that keeps what was
        // typed for another.
        {rsx! { OpForm { key: "{op}", op: op.clone(), summary: true } }}
    }
}
