//! The REST API's OpenAPI document, at `GET /api/v1/openapi.json`: made from
//! the routes of [`api`](crate::api) and the hub's operations as they are, so
//! a client, a UI or a tool that reads OpenAPI knows every path and the body
//! each takes. It is the same for everyone who asks, signed in or not, and
//! says of each path who may call it; operations done only on the hub's own
//! computer are left out, as no request runs them.

use crate::accounts::HubContext;
use ensemble_ops::{Access, Registry, Spec};
use serde_json::{Map, Value, json};

/// One route of the accounts' own, as the document shows it.
pub(crate) struct Route {
    pub method: &'static str,
    /// Under `/api/v1`, with `{…}` for what the path names.
    pub path: &'static str,
    /// The operations it runs; their input, less what the path names, is its
    /// body.
    pub ops: &'static [&'static str],
    /// Its status when it does what it is asked.
    pub status: u16,
    /// Fields the route requires though its operation does not.
    pub require: &'static [&'static str],
}

const fn route(
    method: &'static str,
    path: &'static str,
    ops: &'static [&'static str],
    status: u16,
) -> Route {
    Route {
        method,
        path,
        ops,
        status,
        require: &[],
    }
}

/// Every route [`api`](crate::api) answers but `/ops`, in its order.
pub(crate) const ROUTES: &[Route] = &[
    route("POST", "/register", &["account.register"], 201),
    route("POST", "/login", &["account.login"], 200),
    route("POST", "/join", &["token.join"], 201),
    route("POST", "/logout", &["account.logout"], 200),
    route("GET", "/me", &["me.show"], 200),
    Route {
        require: &["tracker_user"],
        ..route("PUT", "/me", &["me.tracker"], 200)
    },
    route("PUT", "/me/password", &["me.password"], 200),
    route("GET", "/me/tokens", &["me.tokens"], 200),
    route("POST", "/me/tokens", &["me.token"], 201),
    route("DELETE", "/me/tokens/{id}", &["me.revoke"], 200),
    route("POST", "/me/join-codes", &["me.join-code"], 201),
    route("GET", "/users", &["user.list"], 200),
    Route {
        require: &["password"],
        ..route("POST", "/users", &["user.add"], 201)
    },
    route("GET", "/roles", &["role.list"], 200),
    route("POST", "/users/{name}/approve", &["user.approve"], 200),
    route("PUT", "/users/{name}", &["user.role", "user.tracker"], 200),
    route("PUT", "/users/{name}/password", &["user.password"], 200),
    route("POST", "/users/{name}/join-codes", &["user.join-code"], 201),
    route("DELETE", "/users/{name}/tokens", &["user.revoke"], 200),
    route("DELETE", "/users/{name}", &["user.remove"], 200),
];

/// The document, for a hub reached at `base`.
pub(crate) fn document(operations: &Registry<HubContext>, base: &str, owner: &str) -> Value {
    let mut paths = Map::new();
    let mut add = |path: String, method: &str, operation: Value| {
        let entry = paths.entry(path).or_insert_with(|| json!({}));
        entry[method.to_lowercase()] = operation;
    };
    for route in ROUTES {
        let specs: Vec<&Spec> = route
            .ops
            .iter()
            .filter_map(|name| operations.get(name).map(|op| &op.spec))
            .collect();
        let Some(first) = specs.first() else {
            continue;
        };
        let named = path_names(route.path);
        let mut operation = json!({
            "operationId": route_id(route),
            "summary": specs.iter().map(|s| s.summary).collect::<Vec<_>>().join(" "),
            "description": format!("Runs {}. {}", ops_named(route.ops), who(first.access)),
            "tags": [first.name.split('.').next().unwrap_or(first.name)],
            "responses": responses(route.status),
        });
        if !named.is_empty() {
            operation["parameters"] = named
                .iter()
                .map(|n| json!({ "name": n, "in": "path", "required": true, "schema": { "type": "string" } }))
                .collect();
        }
        if matches!(route.method, "POST" | "PUT") {
            let body = route_body(&specs, &named, route.require);
            if !body["properties"].as_object().is_some_and(Map::is_empty) {
                operation["requestBody"] = json!({
                    "required": true,
                    "content": { "application/json": { "schema": body } },
                });
            }
        }
        secure(&mut operation, first.access);
        add(format!("/api/v1{}", route.path), route.method, operation);
    }
    add(
        "/api/v1/ops".into(),
        "GET",
        json!({
            "operationId": "ops",
            "summary": "The operations the token may run, each with the JSON Schema of its input.",
            "tags": ["ops"],
            "security": [{ "bearer": [] }],
            "responses": responses(200),
        }),
    );
    for op in operations.operations() {
        let spec = &op.spec;
        if spec.access == Access::Local {
            continue;
        }
        let mut operation = json!({
            "operationId": spec.name,
            "summary": spec.summary,
            "description": who(spec.access),
            "tags": [spec.name.split('.').next().unwrap_or(spec.name)],
            "requestBody": {
                "required": true,
                "content": { "application/json": { "schema": spec.input_schema() } },
            },
            "responses": responses(200),
        });
        secure(&mut operation, spec.access);
        add(format!("/api/v1/ops/{}", spec.name), "POST", operation);
    }
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Ensemble hub",
            "version": env!("CARGO_PKG_VERSION"),
            "description": format!(
                "The REST API of {owner}'s Ensemble hub. Every operation of the hub is \
                 reached by its name under /api/v1/ops/; the other paths are the accounts' own. \
                 Sign in with a session from /api/v1/login or an access token, as a bearer token."
            ),
        },
        "servers": [{ "url": base }],
        "paths": paths,
        "components": {
            "securitySchemes": {
                "bearer": { "type": "http", "scheme": "bearer" },
            },
            "schemas": {
                "Error": {
                    "type": "object",
                    "properties": {
                        "error": { "type": "string", "description": "What went wrong." },
                        "code": {
                            "enum": ["invalid", "unauthorized", "forbidden", "not_found",
                                     "conflict", "too_many", "unavailable", "failed"],
                        },
                        "field": { "type": "string", "description": "The input field it is about." },
                        "hint": { "type": "string", "description": "What to do about it." },
                    },
                    "required": ["error", "code"],
                },
            },
        },
    })
}

/// What a path names, such as `name` in `/users/{name}`.
fn path_names(path: &str) -> Vec<&str> {
    path.split('/')
        .filter_map(|p| p.strip_prefix('{')?.strip_suffix('}'))
        .collect()
}

/// A route's id: its method and path, such as `put_users_name_password`.
fn route_id(route: &Route) -> String {
    let path: Vec<String> = route
        .path
        .split('/')
        .filter(|p| !p.is_empty())
        .map(|p| p.trim_matches(['{', '}']).replace('-', "_"))
        .collect();
    format!("{}_{}", route.method.to_lowercase(), path.join("_"))
}

fn ops_named(ops: &[&str]) -> String {
    ops.iter()
        .map(|o| format!("`{o}`"))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// The body of a route: its operations' input, less what the path names. One
/// route that runs more than one takes any of their fields.
fn route_body(specs: &[&Spec], named: &[&str], require: &[&str]) -> Value {
    let mut properties = Map::new();
    let mut required: Vec<String> = require.iter().map(|r| r.to_string()).collect();
    for spec in specs {
        let schema = spec.input_schema();
        for (name, field) in schema["properties"].as_object().into_iter().flatten() {
            if !named.contains(&name.as_str()) {
                properties.insert(name.clone(), field.clone());
            }
        }
        if specs.len() == 1 {
            for name in schema["required"].as_array().into_iter().flatten() {
                let name = name.as_str().unwrap_or_default();
                if !named.contains(&name) && !required.iter().any(|r| r == name) {
                    required.push(name.into());
                }
            }
        }
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// Who may call it, in words.
fn who(access: Access) -> String {
    match access {
        Access::Local => "Only on the hub's own computer.".into(),
        Access::Permission(p) => format!("Needs a token with the `{p}` permission."),
        Access::Session => "Needs a session from logging in, not an access token.".into(),
        Access::Signed => "Needs any signed-in account.".into(),
        Access::Anyone => "Anyone may call it.".into(),
    }
}

/// Its `security`: none for what anyone may call.
fn secure(operation: &mut Value, access: Access) {
    operation["security"] = match access {
        Access::Anyone => json!([]),
        _ => json!([{ "bearer": [] }]),
    };
}

fn responses(status: u16) -> Value {
    json!({
        status.to_string(): {
            "description": "Done.",
            "content": { "application/json": { "schema": { "type": "object" } } },
        },
        "default": {
            "description": "Not done, and why.",
            "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_names_what_is_in_braces() {
        assert_eq!(path_names("/users/{name}/approve"), ["name"]);
        assert!(path_names("/me/tokens").is_empty());
        let r = route("PUT", "/users/{name}/join-codes", &[], 200);
        assert_eq!(route_id(&r), "put_users_name_join_codes");
    }
}
