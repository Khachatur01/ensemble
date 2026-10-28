//! The ensemble's own tools, shared by every harness surface.
//!
//! Both surfaces offer the same three tools and must keep offering the same
//! three tools. A harness talking MCP and a manager driven over the chat API
//! are asking for the same thing — give this role this task — and if the two
//! descriptions, schemas or failure messages drifted apart, a config that
//! worked through one would mysteriously not work through the other.
//!
//! So the schemas and the execution live here, and `mcp.rs` and `serve/`
//! compose them rather than each writing their own. The only thing that
//! differs is the envelope: MCP wants `inputSchema`, the chat API wants
//! `parameters`.
//!
//! No tool takes a backend or a model. The caller names a *role* and Ensemble
//! decides what runs it — that is the whole point of the indirection, and a
//! `model` parameter would quietly undo it at the first opportunity.

use crate::port::Usage;
use crate::router::{Ensemble, RouteError};
use serde_json::{Value, json};

pub const DELEGATE: &str = "ensemble_delegate";
pub const ROLES: &str = "ensemble_roles";
pub const HEALTH: &str = "ensemble_health";

/// Every name this module owns. A harness tool that collides with one of these
/// is a configuration problem, not something to silently shadow.
pub const NAMES: [&str; 3] = [DELEGATE, ROLES, HEALTH];

pub fn is_ensemble_tool(name: &str) -> bool {
    NAMES.contains(&name)
}

/// One tool, in the vocabulary neither protocol owns.
pub struct Tool {
    pub name: &'static str,
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: Value,
}

/// The catalogue, for a given set of delegable roles.
///
/// `roles` is what the *caller* may delegate to, which is not the same list on
/// every surface: over MCP the caller is outside the ensemble and the manager
/// is a legitimate target, while over the chat API the caller *is* the manager
/// and would only be recursing into itself.
///
/// With no delegable roles there is no delegation tool. Offering one whose
/// enum is empty invites a call that cannot succeed.
pub fn tools(roles: &[&str]) -> Vec<Tool> {
    let mut out = Vec::new();

    if !roles.is_empty() {
        // Listing the real role names in the description and the enum is what
        // makes the first delegation land: without it the model guesses
        // "architect" and spends a round trip finding out.
        let role_list = roles.join(", ");
        out.push(Tool {
            name: DELEGATE,
            description: format!(
                "Delegate a self-contained task to a member of the ensemble and return its \
                 work. Available roles: {role_list}. Ensemble selects the model and backend \
                 for the role — you do not choose one. Give the role everything it needs: \
                 each call is stateless and the role cannot see this conversation."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "role": {
                        "type": "string",
                        "enum": roles,
                        "description": "Which member of the ensemble does the work."
                    },
                    "task": {
                        "type": "string",
                        "description": "The task, stated so it can be acted on without further questions."
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional background: prior findings, file contents, constraints."
                    }
                },
                "required": ["role", "task"],
                "additionalProperties": false
            }),
        });
    }

    out.push(Tool {
        name: ROLES,
        description: "List the roles this ensemble can delegate to, and what each is for. \
                      Call this first if you are unsure who should do a piece of work."
            .into(),
        parameters: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    });

    out.push(Tool {
        name: HEALTH,
        description: "Check that every backend the ensemble routes to is reachable. Spends no \
                      model tokens. Use it when a delegation fails, to tell a configuration \
                      problem from a bad answer."
            .into(),
        parameters: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    });

    out
}

/// The result of a tool call: text, plus whether it is an error.
///
/// A refused or failed call comes back as a *result* with `is_error`, not as a
/// protocol-level error. The model has to read why it failed in order to do the
/// right thing next; a transport-level error is handled by the harness and the
/// model never sees the reason.
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
    /// What the call spent, when it spent anything. Zero for the two tools that
    /// only read configuration. The chat API sums this into the usage it
    /// reports, because a delegation is a real model call the caller paid for
    /// even though it never saw it happen.
    pub usage: Usage,
}

impl ToolResult {
    pub fn ok(text: impl Into<String>) -> Self {
        ToolResult {
            text: text.into(),
            is_error: false,
            usage: Usage::default(),
        }
    }
    pub fn err(text: impl Into<String>) -> Self {
        ToolResult {
            text: text.into(),
            is_error: true,
            usage: Usage::default(),
        }
    }
}

/// Run one of the ensemble's own tools.
pub fn call(ensemble: &Ensemble, name: &str, args: &Value) -> ToolResult {
    match name {
        DELEGATE => {
            let Some(role) = args.get("role").and_then(Value::as_str) else {
                return ToolResult::err(format!(
                    "{DELEGATE}: `role` is required and must be a string"
                ));
            };
            let Some(task) = args.get("task").and_then(Value::as_str) else {
                return ToolResult::err(format!(
                    "{DELEGATE}: `task` is required and must be a string"
                ));
            };
            if task.trim().is_empty() {
                return ToolResult::err(format!(
                    "{DELEGATE}: `task` is empty — there is nothing to do"
                ));
            }
            let context = args.get("context").and_then(Value::as_str);

            match ensemble.delegate(role, task, context) {
                Ok(out) => ToolResult {
                    text: out.text,
                    is_error: false,
                    usage: Usage {
                        prompt_tokens: out.prompt_tokens,
                        completion_tokens: out.completion_tokens,
                    },
                },
                Err(e) => {
                    // Telling the model whether another attempt could work is
                    // the difference between a useful retry and a loop against
                    // a typo'd role name.
                    let hint = if e.is_retryable() {
                        " This may be transient; the same call is worth one retry."
                    } else {
                        " Retrying this unchanged will fail the same way."
                    };
                    let detail = match &e {
                        RouteError::UnknownRole { .. } => String::new(),
                        RouteError::Backend(_) => format!(" (role '{role}')"),
                    };
                    ToolResult::err(format!("delegation failed{detail}: {e}{hint}"))
                }
            }
        }

        ROLES => {
            let mut lines = Vec::new();
            for role in ensemble.roles() {
                // Backend and model are deliberately absent: the caller must
                // not start choosing them.
                let purpose = ensemble
                    .config()
                    .assignment(role)
                    .and_then(|a| a.prompt.as_deref())
                    .map(first_line)
                    .unwrap_or_else(|| "(no description configured)".to_string());
                lines.push(format!("{role}: {purpose}"));
            }
            ToolResult::ok(lines.join("\n"))
        }

        HEALTH => {
            let mut lines = Vec::new();
            let mut any_bad = false;
            for (backend, result) in ensemble.health() {
                match result {
                    Ok(()) => lines.push(format!("{backend}: ok")),
                    Err(e) => {
                        any_bad = true;
                        lines.push(format!("{backend}: UNREACHABLE — {e}"));
                    }
                }
            }
            let text = lines.join("\n");
            if any_bad {
                ToolResult::err(text)
            } else {
                ToolResult::ok(text)
            }
        }

        other => ToolResult::err(format!(
            "no such tool: '{other}'. This server provides: \
             {DELEGATE}, {ROLES}, {HEALTH}."
        )),
    }
}

/// A role prompt is a paragraph; a tool listing wants a line.
pub fn first_line(prompt: &str) -> String {
    let line = prompt
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() <= 160 {
        line.to_string()
    } else {
        format!("{}…", line.chars().take(160).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_tool_accepts_a_model_or_a_backend_parameter() {
        // The indirection is the product. A `model` argument would undo it.
        let rendered = tools(&["manager", "coder"])
            .iter()
            .map(|t| t.parameters.to_string())
            .collect::<String>();
        assert!(!rendered.contains("model"), "{rendered}");
        assert!(!rendered.contains("backend"), "{rendered}");
    }

    #[test]
    fn the_delegate_enum_is_exactly_the_roles_it_was_given() {
        let t = tools(&["planner", "coder"]);
        let delegate = t.iter().find(|t| t.name == DELEGATE).unwrap();
        assert_eq!(
            delegate.parameters["properties"]["role"]["enum"][0],
            "planner"
        );
        assert_eq!(
            delegate.parameters["properties"]["role"]["enum"][1],
            "coder"
        );
        assert!(
            delegate.description.contains("planner, coder"),
            "{}",
            delegate.description
        );
    }

    #[test]
    fn with_no_delegable_roles_there_is_no_delegation_tool() {
        // An enum with nothing in it is an invitation to a call that cannot
        // succeed.
        let names: Vec<&str> = tools(&[]).iter().map(|t| t.name).collect();
        assert_eq!(names, vec![ROLES, HEALTH]);
    }

    #[test]
    fn a_long_role_prompt_is_clipped_to_one_marked_line() {
        let long = "x".repeat(400);
        let out = first_line(&format!("{long}\nsecond line"));
        assert!(out.ends_with('…'), "{out}");
        assert!(out.chars().count() <= 161, "{}", out.chars().count());
    }
}
