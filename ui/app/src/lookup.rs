//! What the forms offer to pick from, read from the hub: its roles, the
//! agents of the runners connected now, the projects with tasks, the LLM
//! backends and the models they serve. Each is asked for once a page needs
//! it; a page that may not ask gets none, and its fields stay free to type.

use crate::api::{self, Failure};
use crate::form::Pick;
use dioxus::prelude::*;
use serde_json::{Value, json};

pub type Asked = Resource<Result<Value, Failure>>;

/// The answer of the operation `op`, asked once.
pub fn use_op(op: &'static str) -> Asked {
    use_resource(move || api::op(op, json!({})))
}

/// What a permission lets, as a person reads it.
pub const PERMISSIONS: &[(&str, &str)] = &[
    ("share", "keep knowledge in the vault"),
    ("recall", "read the vault's notes"),
    ("tasks", "have tasks worked on by their helper team"),
    ("admin", "manage users and the hub's settings"),
    (
        "compute",
        "have tasks worked on by runners that work for the hub",
    ),
    ("host", "connect a runner that works for the hub"),
];

/// The hub's roles, by name, with what each may do (`role.list`).
pub fn roles(asked: &Asked) -> Vec<(String, Vec<String>)> {
    match &*asked.read() {
        Some(Ok(out)) => out["roles"]
            .as_object()
            .map(|roles| {
                roles
                    .iter()
                    .map(|(name, may)| (name.clone(), strings(may)))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Only one of the hub's roles, each read with what it may do.
pub fn role_pick(field: &str, roles: &[(String, Vec<String>)]) -> Pick {
    Pick {
        field: field.into(),
        options: roles
            .iter()
            .map(|(name, may)| (name.clone(), format!("{name}: {}", may.join(", "))))
            .collect(),
        strict: true,
    }
}

/// The agents of the runners connected now (`runner.list`), each read with
/// what it is and its runner.
pub fn agents(asked: &Asked) -> Vec<(String, String)> {
    let mut found = Vec::new();
    if let Some(Ok(out)) = &*asked.read() {
        for runner in out["runners"].as_array().into_iter().flatten() {
            let on = runner["runner"].as_str().unwrap_or("");
            for agent in runner["agents"].as_array().into_iter().flatten() {
                let name = agent["name"].as_str().unwrap_or("").to_string();
                let about = agent["about"].as_str().unwrap_or("");
                if !found.iter().any(|(n, _)| *n == name) {
                    found.push((name.clone(), format!("{about} (on {on})")));
                }
            }
        }
    }
    found
}

/// The projects with tasks (`task.projects`).
pub fn projects(asked: &Asked) -> Vec<String> {
    match &*asked.read() {
        Some(Ok(out)) => out["projects"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p["project"].as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

/// The LLM backends, by name (`backend.list`).
pub fn backends(asked: &Asked) -> Vec<String> {
    match &*asked.read() {
        Some(Ok(out)) => out["backends"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| b["name"].as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

/// Each model a backend serves, as `<backend>:<model>`: from what the
/// backends answered (`backend.test`) and what runners share
/// (`runner.backends`), and each backend's name, for a model it did not list.
pub fn models(backends: &[String], tested: &Asked) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    if let Some(Ok(out)) = &*tested.read() {
        for b in out["backends"].as_array().into_iter().flatten() {
            let name = b["name"].as_str().unwrap_or("");
            for model in strings(&b["models"]) {
                found.push((format!("{name}:{model}"), format!("served by {name}")));
            }
        }
    }
    for name in backends {
        if !found
            .iter()
            .any(|(m, _)| m.starts_with(&format!("{name}:")))
        {
            found.push((format!("{name}:"), format!("a model of {name}")));
        }
    }
    found
}

/// The texts of a JSON list.
pub fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(String::from)
        .collect()
}
