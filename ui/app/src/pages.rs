//! The app's pages but the settings ([`crate::settings`]). Each reads and
//! changes the hub through its operations ([`api`]); the forms are made from
//! them ([`OpForm`]), with what to pick from read from the hub ([`lookup`]).

use crate::api::{self, Failure};
use crate::form::{Json, OpForm, Pick, may_run};
use crate::lookup::{self, use_op};
use crate::{Me, Route};
use dioxus::prelude::*;
use serde_json::{Value, json};

/// What an operation's answer is shown as while it loads, once it failed,
/// and once it came.
pub fn loaded(
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
pub fn text(value: &Value, key: &str) -> String {
    match &value[key] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A field's name and its value, as a form starts with it.
pub fn value(field: &str, value: impl ToString) -> (String, String) {
    (field.to_string(), value.to_string())
}

/// A field's name and the words a form shows for it.
pub fn label(field: &str, label: &str) -> (String, String) {
    (field.to_string(), label.to_string())
}

/// A time as the hub writes it, as a person reads it: `2026-11-23 08:03`.
pub fn when(at: &str) -> String {
    at.get(..16).unwrap_or(at).replace('T', " ")
}

pub fn me() -> Me {
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
    let mut busy = use_signal(|| false);
    let nav = navigator();
    let submit = move |e: FormEvent| {
        e.prevent_default();
        if name.read().trim().is_empty() || password.read().is_empty() {
            failure.set(Some(Failure {
                status: 400,
                message: "Type your name and password.".into(),
                field: None,
            }));
            return;
        }
        spawn(async move {
            busy.set(true);
            failure.set(None);
            match api::sign_in(name.read().trim(), &password.read()).await {
                Ok(_) => match api::me().await {
                    Ok(value) => {
                        session.set(Some(Me(value)));
                        nav.replace(Route::Home {});
                    }
                    Err(e) => failure.set(Some(e)),
                },
                Err(e) => failure.set(Some(e)),
            }
            busy.set(false);
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
                        autofocus: true,
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
                div { class: "actions",
                    button { r#type: "submit", disabled: busy(),
                        if busy() { "Signing in…" } else { "Sign in" }
                    }
                }
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
    let permissions = lookup::strings(&me.0["permissions"]);
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
                    p { "Accounts and sign-ups." }
                }
                Link { class: "card", to: Route::Settings {},
                    h2 { "Settings" }
                    p { "The hub, roles, backends, knowledge, sources and trackers." }
                }
            }
        }
    }
}

/// The fields of a new task seldom needed, folded away.
fn task_more() -> Vec<String> {
    [
        "repository",
        "context",
        "goal",
        "constraints",
        "files",
        "after",
        "gate",
        "gate_attempts",
    ]
    .map(String::from)
    .to_vec()
}

/// The words a task's form shows for its fields.
fn task_labels() -> Vec<(String, String)> {
    vec![
        label("task", "what to do"),
        label("agent", "agent (empty: the backlog)"),
        label("done_when", "done when"),
        label("gate_attempts", "gate attempts"),
    ]
}

/// The agents to give a task to, suggested: one not connected now may still
/// be named.
fn agent_pick(agents: &lookup::Asked) -> Pick {
    let mut pick = Pick::suggest("agent", []);
    pick.options = lookup::agents(agents);
    pick
}

#[component]
pub fn Projects() -> Element {
    let mut projects = use_op("task.projects");
    let agents = use_op("runner.list");
    let known = lookup::projects(&projects);
    rsx! {
        h1 { "Tasks" }
        {loaded(&projects.read(), |out| {
            let list = out["projects"].as_array().cloned().unwrap_or_default();
            rsx! {
                if list.is_empty() {
                    p { class: "muted", "No tasks yet: make the first below." }
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
            picks: vec![Pick::suggest("project", known), agent_pick(&agents)],
            more: task_more(),
            labels: task_labels(),
            submit: "Make the task",
            clear: true,
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

/// Is the task over, so nothing more is done to it?
fn finished(task: &Value) -> bool {
    matches!(status(task).as_str(), "done" | "failed" | "cancelled")
}

#[component]
pub fn ProjectTasks(project: String) -> Element {
    let shown = project.clone();
    let mut tasks = use_resource(move || {
        let project = shown.clone();
        async move { api::op("task.list", json!({ "project": project })).await }
    });
    let agents = use_op("runner.list");
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
            picks: vec![agent_pick(&agents)],
            more: task_more(),
            labels: task_labels(),
            submit: "Make the task",
            clear: true,
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
    let agents = use_op("runner.list");
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
            let shown = if detail["task"].is_null() { detail } else { &detail["task"] };
            let ticket = &shown["ticket"];
            let agent = match text(ticket, "agent") {
                agent if !agent.is_empty() => format!("for {agent}"),
                _ if status(shown) == "backlog" => "in the backlog until it is given to an agent".into(),
                _ => "for no agent".into(),
            };
            let question = text(shown, "question");
            let over = finished(shown);
            rsx! {
                h1 { "{text(ticket, \"title\")}" }
                p { class: "muted",
                    span { class: "state {status(shown)}", "{status(shown)}" }
                    " {id}, {agent}"
                }
                if !question.is_empty() {
                    section { class: "panel notice",
                        p { strong { "It asks: " } "{question}" }
                        OpForm {
                            op: "task.answer",
                            fixed: on_task.clone(),
                            labels: vec![label("text", "your answer")],
                            submit: "Answer",
                            on_done: move |_| task.restart(),
                        }
                    }
                }
                pre { class: "text", "{text(ticket, \"text\")}" }
                if !over {
                    div { class: "columns",
                        section { class: "panel",
                            h2 { "Give it to an agent" }
                            OpForm {
                                op: "task.assign",
                                fixed: on_task.clone(),
                                picks: vec![agent_pick(&agents)],
                                values: vec![value("agent", text(ticket, "agent"))],
                                submit: "Assign",
                                on_done: move |_| task.restart(),
                            }
                        }
                        section { class: "panel",
                            h2 { "Cancel it" }
                            p { class: "muted", "Before it starts; or its runner stops it." }
                            OpForm {
                                op: "task.cancel",
                                fixed: on_task.clone(),
                                submit: "Cancel the task",
                                danger: true,
                                confirm: "Cancel {id}? An agent working on it stops.",
                                on_done: move |_| task.restart(),
                            }
                        }
                    }
                }
                details {
                    summary { "Everything the hub keeps of it" }
                    Json { value: detail.clone() }
                }
            }
        })}
    }
}

#[component]
pub fn Runners() -> Element {
    let runners = use_op("runner.list");
    rsx! {
        h1 { "Runners" }
        {loaded(&runners.read(), |out| {
            let list = out["runners"].as_array().cloned().unwrap_or_default();
            rsx! {
                if list.is_empty() {
                    p { class: "muted",
                        "No runner that works for you is connected. On a computer that should work: "
                        code { "ensemble-runner join <hub> <code>" }
                        ", with a code from "
                        Link { to: Route::Account {}, "your account" }
                        "."
                    }
                }
                for runner in list {
                    section { class: "card", key: "{text(&runner, \"runner\")}",
                        h2 { "{text(&runner, \"runner\")}" }
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
        div { class: "columns",
            if me.can("recall") {
                section { class: "panel",
                    h2 { "Recall" }
                    OpForm {
                        op: "knowledge.find",
                        labels: vec![label("query", "topic")],
                        submit: "Recall",
                        quiet: true,
                        on_done: move |out: Value| found.set(Some(out)),
                    }
                    if let Some(out) = found.read().as_ref() {
                        if text(out, "text").trim().is_empty() {
                            p { class: "muted", "No notes about it." }
                        } else {
                            pre { class: "text", "{text(out, \"text\")}" }
                        }
                    }
                }
            }
            if me.can("share") {
                section { class: "panel",
                    h2 { "Share" }
                    OpForm {
                        op: "knowledge.share",
                        picks: vec![Pick::named("visibility", &[("private", "Only you"), ("hub", "Everyone on the hub")])],
                        values: vec![value("visibility", "private")],
                        submit: "Share",
                        clear: true,
                    }
                }
            }
        }
    }
}

#[component]
pub fn Account() -> Element {
    let me = me();
    let mut tokens = use_op("me.tokens");
    let mine = lookup::strings(&me.0["permissions"]);
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
                    thead { tr { th { "Name" } th { "May" } th { "Made" } th { "Expires" } th {} } }
                    tbody {
                        for token in list {
                            TokenRow { key: "{text(&token, \"id\")}", token: token.clone(), on_revoked: move |_| tokens.restart() }
                        }
                    }
                }
            }
        })}
        div { class: "columns",
            section { class: "panel",
                h3 { "New token" }
                OpForm {
                    op: "me.token",
                    picks: vec![Pick::only("scopes", mine.clone())],
                    labels: vec![
                        label("scopes", "may (none: all your role may)"),
                        label("expires_days", "expires in days"),
                    ],
                    submit: "Make a token",
                    clear: true,
                    on_done: move |_| tokens.restart(),
                }
            }
            section { class: "panel",
                h3 { "Join code" }
                p { class: "muted", "Signs one client in, such as a runner, within 24 hours." }
                OpForm { op: "me.join-code", submit: "Make a join code" }
            }
        }
        div { class: "columns",
            section { class: "panel",
                h3 { "Password" }
                OpForm {
                    op: "me.password",
                    repeat: vec!["new".to_string()],
                    labels: vec![label("current", "current password"), label("new", "new password")],
                    submit: "Change the password",
                    clear: true,
                }
            }
            section { class: "panel",
                h3 { "Tracker name" }
                OpForm {
                    op: "me.tracker",
                    values: vec![value("tracker_user", text(&me.0, "tracker_user"))],
                    labels: vec![label("tracker_user", "you in the tracker")],
                    submit: "Save",
                    saved: true,
                }
            }
        }
    }
}

#[component]
fn TokenRow(token: Value, on_revoked: EventHandler<()>) -> Element {
    let id = text(&token, "id");
    let name = text(&token, "name");
    let mut failure = use_signal(|| None::<Failure>);
    let mut asking = use_signal(|| false);
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
        .unwrap_or_else(|| "all its role may".into());
    let expires = match text(&token, "expires") {
        e if e.is_empty() => "never".to_string(),
        e => when(&e),
    };
    rsx! {
        tr {
            td { "{name}" }
            td { "{scopes}" }
            td { "{when(&text(&token, \"made\"))}" }
            td { "{expires}" }
            td { class: "end",
                if asking() {
                    span { class: "muted", "Revoke {name}? " }
                    button { class: "link danger", onclick: revoke, "Yes" }
                    " "
                    button { class: "link", onclick: move |_| asking.set(false), "No" }
                } else {
                    button { class: "link danger", onclick: move |_| asking.set(true), "Revoke" }
                }
                if let Some(f) = failure.read().as_ref() {
                    span { class: "error", " {f}" }
                }
            }
        }
    }
}

#[component]
pub fn Users() -> Element {
    let mut users = use_op("user.list");
    let roles = use_op("role.list");
    let known = lookup::roles(&roles);
    rsx! {
        h1 { "Users" }
        {loaded(&users.read(), |out| {
            let list = out["users"].as_array().cloned().unwrap_or_default();
            let waiting: Vec<Value> = list.iter().filter(|u| u["status"] == "pending").cloned().collect();
            rsx! {
                if !waiting.is_empty() {
                    section { class: "panel notice",
                        h2 { "Sign-ups waiting" }
                        for user in waiting {
                            Waiting {
                                key: "{text(&user, \"name\")}",
                                name: text(&user, "name"),
                                roles: known.clone(),
                                on_done: move |_| users.restart(),
                            }
                        }
                    }
                }
                table {
                    thead { tr { th { "Name" } th { "Role" } th { "Status" } th { "Tokens" } th { "Since" } } }
                    tbody {
                        for user in list {
                            tr { key: "{text(&user, \"name\")}",
                                td { Link { to: Route::UserPage { name: text(&user, "name") }, "{text(&user, \"name\")}" } }
                                td { "{text(&user, \"role\")}" }
                                td { span { class: "state {text(&user, \"status\")}", "{text(&user, \"status\")}" } }
                                td { "{text(&user, \"tokens\")}" }
                                td { "{when(&text(&user, \"registered\"))}" }
                            }
                        }
                    }
                }
            }
        })}
        section { class: "panel",
            h2 { "Add a user" }
            OpForm {
                op: "user.add",
                picks: vec![lookup::role_pick("role", &known)],
                repeat: vec!["password".to_string()],
                submit: "Add",
                clear: true,
                on_done: move |_| users.restart(),
            }
            p { class: "muted",
                "What each role may do is in "
                Link { to: Route::SettingsPage { section: "roles".into() }, "Settings, Roles" }
                "."
            }
        }
    }
}

/// A sign-up waiting for an admin: let in with a role, or turned down.
#[component]
fn Waiting(name: String, roles: Vec<(String, Vec<String>)>, on_done: EventHandler<()>) -> Element {
    let named = vec![("name".to_string(), json!(name))];
    rsx! {
        div { class: "columns",
            div {
                h3 { "{name}" }
                OpForm {
                    op: "user.approve",
                    fixed: named.clone(),
                    picks: vec![lookup::role_pick("role", &roles)],
                    submit: "Let in",
                    on_done: move |_| on_done.call(()),
                }
            }
            div {
                h3 { "\u{a0}" }
                OpForm {
                    op: "user.remove",
                    fixed: named.clone(),
                    submit: "Turn down",
                    danger: true,
                    confirm: "Turn down {name}'s sign-up?",
                    on_done: move |_| on_done.call(()),
                }
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
    let roles = use_op("role.list");
    let known = lookup::roles(&roles);
    let catalog = use_context::<crate::form::Catalog>();
    let nav = navigator();
    let me = me();
    let named = vec![("name".to_string(), json!(name))];
    let yourself = me.name() == name;
    rsx! {
        p { class: "crumbs", Link { to: Route::Users {}, "Users" } " / {name}" }
        h1 { "{name}" }
        {loaded(&user.read(), |out| {
            let pending = out["status"] == "pending";
            let tracker = match text(out, "tracker_user") {
                t if t.is_empty() => format!("{} (its own name)", text(out, "tracker_name")),
                t => t,
            };
            rsx! {
                dl { class: "facts",
                    dt { "Role" } dd { "{text(out, \"role\")}" }
                    dt { "Status" } dd { span { class: "state {text(out, \"status\")}", "{text(out, \"status\")}" } }
                    dt { "Since" } dd { "{when(&text(out, \"registered\"))}" }
                    dt { "Password" } dd { if out["has_password"] == true { "set" } else { "none: signs in with a token" } }
                    dt { "Tokens" } dd { "{text(out, \"tokens\")}" }
                    dt { "In the tracker" } dd { "{tracker}" }
                }
                div { class: "grid",
                    if pending {
                        section { class: "panel",
                            h3 { "Let in" }
                            OpForm {
                                op: "user.approve",
                                fixed: named.clone(),
                                picks: vec![lookup::role_pick("role", &known)],
                                submit: "Let in",
                                on_done: move |_| user.restart(),
                            }
                        }
                    } else {
                        section { class: "panel",
                            h3 { "Role" }
                            OpForm {
                                op: "user.role",
                                fixed: named.clone(),
                                picks: vec![lookup::role_pick("role", &known)],
                                values: vec![value("role", text(out, "role"))],
                                submit: "Change the role",
                                confirm: if yourself { Some("Change your own role? You may lose what lets you change it back.".to_string()) } else { None },
                                saved: true,
                                on_done: move |_| user.restart(),
                            }
                        }
                        section { class: "panel",
                            h3 { "Password" }
                            OpForm {
                                op: "user.password",
                                fixed: named.clone(),
                                repeat: vec!["password".to_string()],
                                labels: vec![label("password", "new password")],
                                submit: "Set the password",
                                clear: true,
                                saved: true,
                                on_done: move |_| user.restart(),
                            }
                        }
                        section { class: "panel",
                            h3 { "Name in the tracker" }
                            OpForm {
                                op: "user.tracker",
                                fixed: named.clone(),
                                values: vec![value("tracker_user", text(out, "tracker_user"))],
                                labels: vec![label("tracker_user", "name in the tracker")],
                                submit: "Save",
                                saved: true,
                                on_done: move |_| user.restart(),
                            }
                        }
                        section { class: "panel",
                            h3 { "Sign a client in" }
                            p { class: "muted", "A token, or a join code for a runner. Each is shown once." }
                            OpForm { op: "user.token", fixed: named.clone(), submit: "Make a token", on_done: move |_| user.restart() }
                            OpForm { op: "user.join-code", fixed: named.clone(), submit: "Make a join code" }
                        }
                    }
                }
                section { class: "panel danger-zone",
                    h3 { "Danger zone" }
                    div { class: "columns",
                        if !pending {
                            OpForm {
                                op: "user.revoke",
                                fixed: named.clone(),
                                submit: "Revoke every token",
                                danger: true,
                                confirm: "Revoke all {name}'s tokens? Their agents and runners are signed out.",
                                on_done: move |_| user.restart(),
                            }
                        }
                        if may_run(&catalog, "user.remove") {
                            OpForm {
                                op: "user.remove",
                                fixed: named.clone(),
                                submit: if pending { "Turn down" } else { "Remove the account" },
                                danger: true,
                                confirm: if yourself {
                                    "Remove your own account? You are signed out for good.".to_string()
                                } else {
                                    format!("Remove {name}? This cannot be undone.")
                                },
                                quiet: true,
                                on_done: move |_| { nav.push(Route::Users {}); },
                            }
                        }
                    }
                }
            }
        })}
    }
}
