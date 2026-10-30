//! The tools an agent calls to use the ensemble: delegate, list agents, check
//! health. The schemas and the execution live here; `mcp.rs` wraps them in
//! its JSON-RPC envelope (`inputSchema`).
//!
//! No tool takes a backend or a model. The caller names an *agent* and Ensemble
//! decides what runs it — that is the whole point of the indirection, and a
//! `model` parameter would quietly undo it at the first opportunity.

use crate::router::{Ensemble, RouteError};
use serde_json::{Value, json};

pub const DELEGATE: &str = "delegate";
pub const AGENTS: &str = "agents";
pub const HEALTH: &str = "health";

/// One tool: its name, what it is for, and the schema of its arguments.
pub struct Tool {
    pub name: &'static str,
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: Value,
}

/// The catalogue, for a given set of delegable agents.
///
/// `agents` is what the caller may delegate to.
///
/// With no delegable agents there is no delegation tool. Offering one whose
/// enum is empty invites a call that cannot succeed.
pub fn tools(agents: &[&str]) -> Vec<Tool> {
    let mut out = Vec::new();

    if !agents.is_empty() {
        // Listing the real agent names in the description and the enum is what
        // makes the first delegation land: without it the model guesses
        // "architect" and spends a round trip finding out.
        let agent_list = agents.join(", ");
        out.push(Tool {
            name: DELEGATE,
            description: format!(
                "Delegate a self-contained task to an agent of the ensemble and return its \
                 work. Available agents: {agent_list}. Ensemble selects what runs the agent \
                 — you do not choose a model. Give the agent everything it needs: each \
                 call is stateless and the agent cannot see this conversation."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "enum": agents,
                        "description": "Which agent of the ensemble does the work."
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
                "required": ["agent", "task"],
                "additionalProperties": false
            }),
        });
    }

    out.push(Tool {
        name: AGENTS,
        description: "List the agents this ensemble can delegate to, and what each is for. \
                      Call this first if you are unsure who should do a piece of work."
            .into(),
        parameters: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    });

    out.push(Tool {
        name: HEALTH,
        description: "Check that every agent CLI the ensemble runs starts, and that every LLM \
                      backend answers and serves its agents' models. Spends no model tokens. \
                      Use it when a delegation fails, to tell a configuration problem from a \
                      bad answer."
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
}

impl ToolResult {
    pub fn ok(text: impl Into<String>) -> Self {
        ToolResult {
            text: text.into(),
            is_error: false,
        }
    }
    pub fn err(text: impl Into<String>) -> Self {
        ToolResult {
            text: text.into(),
            is_error: true,
        }
    }
}

/// Run one of the ensemble's own tools, passing along what a delegated agent
/// is doing as it does it.
/// Setting `cancel` stops a delegation.
pub fn call(
    ensemble: &Ensemble,
    name: &str,
    args: &Value,
    progress: &dyn Fn(&str),
    cancel: &std::sync::atomic::AtomicBool,
) -> ToolResult {
    match name {
        DELEGATE => {
            let Some(role) = args.get("agent").and_then(Value::as_str) else {
                return ToolResult::err(format!(
                    "{DELEGATE}: `agent` is required and must be a string"
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

            match ensemble.delegate_reporting(role, task, context, progress, cancel) {
                Ok(out) => ToolResult {
                    text: format!("{}\n\n{}", out.text, agent_footer(role, &out.stats)),
                    is_error: false,
                },
                Err(e) => {
                    // Telling the model whether another attempt could work is
                    // the difference between a useful retry and a loop against
                    // a typo'd agent name.
                    let timed_out =
                        matches!(&e, RouteError::Agent(crate::port::Error::Timeout { .. }));
                    let hint = if timed_out {
                        " This may be transient; the same call is worth one retry."
                    } else if e.is_retryable() {
                        " Call `health`, and retry once what it reports is fixed."
                    } else {
                        " Retrying this unchanged will fail the same way."
                    };
                    let detail = match &e {
                        RouteError::UnknownAgent { .. } => String::new(),
                        RouteError::Agent(_) => format!(" (agent '{role}')"),
                    };
                    ToolResult::err(format!("delegation failed{detail}: {e}{hint}"))
                }
            }
        }

        AGENTS => {
            let mut lines = Vec::new();
            for role in ensemble.agents() {
                // Where it runs and the model are deliberately absent: the caller must
                // not start choosing them.
                let purpose = ensemble
                    .config()
                    .agent(role)
                    .and_then(|a| a.prompt.as_deref())
                    .map(first_line)
                    .unwrap_or_else(|| "(no description configured)".to_string());
                let kind = match ensemble.config().agent(role) {
                    Some(a) if a.can_change_files() => " [can change files]",
                    Some(a) if a.has_tools() => " [read-only]",
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
                        lines.push(format!("{backend}: FAILED — {e}"));
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
             {DELEGATE}, {AGENTS}, {HEALTH}."
        )),
    }
}

/// Most shell commands listed in a footer; the rest are counted.
const MAX_LISTED_COMMANDS: usize = 10;

/// What an agent did, under its report: enough for the caller to review the
/// work without asking the agent to describe it.
fn agent_footer(role: &str, a: &crate::router::AgentStats) -> String {
    let mut out = format!(
        "[{role}: {} step(s), {} tool call(s)]",
        a.steps, a.tool_calls
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
    if a.overlapped && !a.changes.is_empty() {
        out.push_str(
            "\n[another delegation ran in the project at the same time; some of these files may be its changes]",
        );
    }
    out
}

/// An agent prompt is a paragraph; a tool listing wants a line.
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
            delegate.parameters["properties"]["agent"]["enum"][0],
            "planner"
        );
        assert_eq!(
            delegate.parameters["properties"]["agent"]["enum"][1],
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
        assert_eq!(names, vec![AGENTS, HEALTH]);
    }

    #[test]
    fn a_long_role_prompt_is_clipped_to_one_marked_line() {
        let long = "x".repeat(400);
        let out = first_line(&format!("{long}\nsecond line"));
        assert!(out.ends_with('…'), "{out}");
        assert!(out.chars().count() <= 161, "{}", out.chars().count());
    }
}
