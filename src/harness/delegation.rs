//! The tools an agent calls to use the ensemble: delegate, list roles, check
//! health. The schemas and the execution live here; `mcp.rs` wraps them in
//! its JSON-RPC envelope (`inputSchema`).
//!
//! No tool takes a backend or a model. The caller names a *role* and Ensemble
//! decides what runs it — that is the whole point of the indirection, and a
//! `model` parameter would quietly undo it at the first opportunity.

use crate::port::Usage;
use crate::router::{Ensemble, RouteError};
use serde_json::{Value, json};

pub const DELEGATE: &str = "delegate";
pub const ROLES: &str = "roles";
pub const HEALTH: &str = "health";

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
/// `roles` is what the caller may delegate to.
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
    /// only read configuration. A delegation is a real model call (or
    /// several, for an agent) the caller paid for.
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

/// Turn a call that names a role directly into the arguments
/// [`DELEGATE`] takes.
///
/// Models that have just been told about a role often call it as if it were a
/// tool: `scout({"task": "find the README"})`. The task is usually under
/// `task`, but `prompt`, `description`, `input`, `query` and `message` all
/// occur. A bare string is a task. When none of those is present, the whole
/// argument object becomes the task rather than being discarded — the model
/// wrote it for a reason, and a downstream role can still use it.
///
/// An unusable call is an error the caller reads as a tool result and can
/// correct.
pub fn role_arguments(role: &str, arguments: &str) -> Result<Value, String> {
    let parsed: Value = serde_json::from_str(arguments).map_err(|e| {
        format!(
            "the arguments were not valid JSON ({e}). Call `{role}` with \
             {{\"task\": \"...\"}}, or call `{DELEGATE}` with `role` and `task`."
        )
    })?;

    let (task, context) = match parsed {
        Value::String(task) => (task, None),
        Value::Object(mut object) => {
            let context = object
                .remove("context")
                .and_then(|v| v.as_str().map(str::to_string));
            let task = ["task", "prompt", "description", "input", "query", "message"]
                .iter()
                .find_map(|key| object.get(*key).and_then(Value::as_str).map(str::to_string))
                .or_else(|| {
                    (!object.is_empty()).then(|| Value::Object(object.clone()).to_string())
                });
            match task {
                Some(task) => (task, context),
                None => return Err(no_task(role)),
            }
        }
        other => {
            return Err(format!(
                "the arguments must be a JSON object or a string; got {other}. {}",
                no_task(role)
            ));
        }
    };

    if task.trim().is_empty() {
        return Err(format!(
            "`{role}` was given an empty task — there is nothing to do. Call it with \
             {{\"task\": \"what to do\"}}."
        ));
    }

    let mut out = json!({ "role": role, "task": task });
    if let Some(context) = context {
        out["context"] = Value::String(context);
    }
    Ok(out)
}

fn no_task(role: &str) -> String {
    format!(
        "no task: call `{role}` with {{\"task\": \"what to do\"}}, or call `{DELEGATE}` \
         with `role` and `task`."
    )
}

/// Run one of the ensemble's own tools.
pub fn call(ensemble: &Ensemble, name: &str, args: &Value) -> ToolResult {
    call_reporting(ensemble, name, args, &|_| {})
}

/// [`call`], passing along what a delegated agent is doing as it does it.
pub fn call_reporting(
    ensemble: &Ensemble,
    name: &str,
    args: &Value,
    progress: &dyn Fn(&str),
) -> ToolResult {
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

            match ensemble.delegate_reporting(role, task, context, progress) {
                Ok(out) => ToolResult {
                    text: match &out.agent {
                        Some(a) => format!("{}\n\n{}", out.text, agent_footer(role, a)),
                        None => out.text,
                    },
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
                    let timed_out =
                        matches!(&e, RouteError::Backend(crate::port::Error::Timeout { .. }));
                    let hint = if timed_out {
                        " This may be transient; the same call is worth one retry."
                    } else if e.is_retryable() {
                        " Ensemble already retried it; call `health`, and retry once the backend is back."
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
                let kind = match ensemble.config().assignment(role) {
                    Some(a) if a.tools.iter().any(|t| t.writes()) => " [agent, can change files]",
                    Some(a) if !a.tools.is_empty() => " [agent, read-only]",
                    _ => "",
                };
                lines.push(format!("{role}{kind}: {purpose}"));
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

/// Most `bash` commands listed in a footer; the rest are counted.
const MAX_LISTED_COMMANDS: usize = 10;

/// What an agent did, under its report: enough for the caller to review the
/// work without asking the agent to describe it.
fn agent_footer(role: &str, a: &crate::router::AgentStats) -> String {
    let mut out = format!(
        "[{role} ran as an agent: {} step(s), {} tool call(s){}]",
        a.steps,
        a.tool_calls,
        if a.trimmed > 0 {
            format!(", {} old tool output(s) dropped", a.trimmed)
        } else {
            String::new()
        }
    );
    if !a.changes.is_empty() {
        let files: Vec<String> = a
            .changes
            .iter()
            .map(|(path, change)| format!("{path} ({})", change.as_str()))
            .collect();
        out.push_str(&format!("\n[files changed: {}]", files.join(", ")));
    }
    if !a.commands.is_empty() {
        let mut shown: Vec<&str> = a
            .commands
            .iter()
            .take(MAX_LISTED_COMMANDS)
            .map(String::as_str)
            .collect();
        let more = a.commands.len().saturating_sub(MAX_LISTED_COMMANDS);
        let more = format!("and {more} more");
        if a.commands.len() > MAX_LISTED_COMMANDS {
            shown.push(&more);
        }
        out.push_str(&format!("\n[commands run: {}]", shown.join("; ")));
    }
    out
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

    #[test]
    fn a_role_named_directly_becomes_a_delegation_with_task_and_context() {
        let args = role_arguments(
            "scout",
            r#"{"task":"find the README","context":"the project root"}"#,
        )
        .unwrap();
        assert_eq!(args["role"], "scout");
        assert_eq!(args["task"], "find the README");
        assert_eq!(args["context"], "the project root");
    }

    #[test]
    fn the_task_aliases_models_use_are_accepted() {
        for key in ["task", "prompt", "description", "input", "query", "message"] {
            let call = format!(r#"{{"{key}":"do the work"}}"#);
            let args = role_arguments("scout", &call).unwrap();
            assert_eq!(args["task"], "do the work", "alias {key}");
        }
    }

    #[test]
    fn a_bare_string_is_the_task() {
        let args = role_arguments("scout", r#""find the README""#).unwrap();
        assert_eq!(args["role"], "scout");
        assert_eq!(args["task"], "find the README");
    }

    #[test]
    fn an_unrecognised_argument_object_is_kept_as_the_task() {
        let args = role_arguments("scout", r#"{"filePath":"src/main.rs"}"#).unwrap();
        assert_eq!(args["role"], "scout");
        assert_eq!(args["task"], r#"{"filePath":"src/main.rs"}"#);
    }

    #[test]
    fn a_direct_role_call_with_no_task_says_how_to_call_it() {
        let err = role_arguments("scout", "{}").unwrap_err();
        assert!(err.contains("no task"), "{err}");
        assert!(err.contains(DELEGATE), "{err}");
    }
}
