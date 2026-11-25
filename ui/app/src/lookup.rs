//! What the forms offer to pick from, read from the hub: its roles, the
//! projects with tasks, and the
//! models of the ANN pool that are on. Each is asked for once a page needs
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
    ("tasks", "have tasks worked on by their runners"),
    ("admin", "manage users and the hub's settings"),
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

/// The models of the ANN pool of `kind`, `llm` or `embedding`, as
/// `<provider>:<model>`, each with what it is (`model.list`).
pub fn pool_models(asked: &Asked, kind: &str) -> Vec<(String, String)> {
    match &*asked.read() {
        Some(Ok(out)) => out["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["type"] == kind)
            .filter_map(|m| {
                let name = m["name"].as_str()?.to_string();
                let about = match (m["tier"].as_str(), kind) {
                    (Some(tier), _) => format!("{tier} LLM"),
                    (None, "decision") => "decision model".to_string(),
                    (None, _) => "embedding model".to_string(),
                };
                Some((name, about))
            })
            .collect(),
        _ => Vec::new(),
    }
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
