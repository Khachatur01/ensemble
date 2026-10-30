//! The tools an agent calls to use the ensemble: delegate, submit and follow
//! tasks, list agents, check health. The schemas and the execution live here; `mcp.rs` wraps them in
//! its JSON-RPC envelope (`inputSchema`).
//!
//! No tool takes a backend or a model. The caller names an *agent* and Ensemble
//! decides what runs it — that is the whole point of the indirection, and a
//! `model` parameter would quietly undo it at the first opportunity.

use super::queue::{Queue, Task};
use crate::router::{Ensemble, RouteError};
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

pub const DELEGATE: &str = "delegate";
pub const SUBMIT: &str = "submit";
pub const STATUS: &str = "status";
pub const WAIT: &str = "wait";
pub const CANCEL: &str = "cancel";

/// Seconds `wait` waits when the caller names no time, and the most it may.
const DEFAULT_WAIT_SECONDS: u64 = 30;
const MAX_WAIT_SECONDS: u64 = 300;
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
                 call is stateless and the agent cannot see this conversation. The call \
                 blocks until the agent is done; for work that may take more than a minute, \
                 set `background`, or `submit` it as a task, and use `status` and `wait`."
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
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Return at once with an id instead of waiting for the agent. \
                                        See what it is doing with `status`, and get its reply with `wait`. \
                                        Nothing is written to the project's tasks."
                    }
                },
                "required": ["agent", "task"],
                "additionalProperties": false
            }),
        });

        out.push(Tool {
            name: SUBMIT,
            description: format!(
                "Write a task for an agent to the project's task list (.ensemble/tasks/, \
                 committed like a ticket) and return its id at once. Agents work through \
                 tasks while this session runs, as many at a time as each agent allows; a \
                 task this session does not finish runs in the next one. Available agents: \
                 {agent_list}. Follow it with `status`, collect its result with `wait`."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "enum": agents,
                        "description": "Which agent of the ensemble does the work."
                    },
                    "title": {
                        "type": "string",
                        "description": "One line naming the task, as a ticket title."
                    },
                    "task": {
                        "type": "string",
                        "description": "The task, stated so it can be acted on without further questions."
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional background: prior findings, file contents."
                    },
                    "goal": {
                        "type": "string",
                        "description": "Optional: the outcome wanted, in a sentence."
                    },
                    "constraints": {
                        "type": "array", "items": { "type": "string" },
                        "description": "Optional: what the agent must not do or change."
                    },
                    "files": {
                        "type": "array", "items": { "type": "string" },
                        "description": "Optional: where the work is."
                    },
                    "done_when": {
                        "type": "array", "items": { "type": "string" },
                        "description": "Optional: how the agent can tell it is done."
                    },
                    "after": {
                        "type": "array", "items": { "type": "string" },
                        "description": "Optional: ids of tasks that must be done first. The task \
                                        waits until each has ended `done`."
                    },
                    "gate": {
                        "type": "object",
                        "description": "Optional: a check from the project's config the work must \
                                        pass. When it fails, the agent runs again with its output, up \
                                        to `attempts` runs in all.",
                        "properties": {
                            "name": { "type": "string", "description": "One of the config's gates." },
                            "attempts": { "type": "integer", "minimum": 1 }
                        },
                        "required": ["name", "attempts"],
                        "additionalProperties": false
                    }
                },
                "required": ["agent", "title", "task"],
                "additionalProperties": false
            }),
        });

        out.push(Tool {
            name: STATUS,
            description: "Show the project's tasks and this session's background delegations: \
                          which agent, whether each is queued, running, done or failed, how long \
                          it has run, and what it did last. Without `task`, all of them. Spends \
                          no model tokens."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "One task id; omit for all." }
                },
                "additionalProperties": false
            }),
        });

        out.push(Tool {
            name: WAIT,
            description: format!(
                "Get the result of a task or background delegation, waiting up to \
                 `timeout_seconds` (default {DEFAULT_WAIT_SECONDS}, at most {MAX_WAIT_SECONDS}) \
                 for it to end. One still queued or running when the time is up is reported, \
                 not failed: call `wait` again."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "The id `submit` or `delegate` returned." },
                    "timeout_seconds": { "type": "integer", "minimum": 1, "maximum": MAX_WAIT_SECONDS }
                },
                "required": ["task"],
                "additionalProperties": false
            }),
        });

        out.push(Tool {
            name: CANCEL,
            description: "Stop a task or background delegation: its agent's process is killed. A \
                          task that has not started is marked cancelled and never runs."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "The id `submit` or `delegate` returned." }
                },
                "required": ["task"],
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
/// Setting `cancel` stops a blocking delegation.
///
/// A background delegation is registered in `queue` and handed to `spawn`,
/// which runs it on a thread of the session so that this call can return at
/// once. A blocking one waits for a slot of its agent first.
pub fn call<'s>(
    ensemble: &'s Ensemble,
    name: &str,
    args: &Value,
    progress: &dyn Fn(&str),
    cancel: &AtomicBool,
    queue: &'s Queue<'s>,
    spawn: &dyn Fn(Task<'s>),
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
            let background = args
                .get("background")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            // An unknown agent fails at once, so it takes the blocking path
            // and says so now rather than inside a background run.
            if !ensemble.agents().contains(&role) {
                return run_delegation(ensemble, role, task, context, None, progress, cancel);
            }
            if background {
                let id = queue.start_adhoc(role, task, context, spawn);
                return ToolResult::ok(format!(
                    "{id} started. `status` shows what it is doing; `wait` with task {id} \
                     returns its reply."
                ));
            }
            let waiting = || progress(&format!("{role}: waiting for a free slot"));
            let Some(_slot) = queue.slot(role, cancel, &waiting) else {
                return ToolResult::err(format!(
                    "delegation cancelled while it waited for a free slot of '{role}'."
                ));
            };
            run_delegation(ensemble, role, task, context, None, progress, cancel)
        }

        SUBMIT => {
            let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::trim);
            let (Some(role), Some(title), Some(task)) =
                (text("agent"), text("title"), text("task"))
            else {
                return ToolResult::err(format!(
                    "{SUBMIT}: `agent`, `title` and `task` are required and must be strings"
                ));
            };
            if title.is_empty() || task.is_empty() {
                return ToolResult::err(format!("{SUBMIT}: `title` and `task` cannot be empty"));
            }
            if title.contains('\n') {
                return ToolResult::err(format!("{SUBMIT}: `title` is one line"));
            }
            if !ensemble.agents().contains(&role) {
                return ToolResult::err(format!(
                    "{SUBMIT}: no agent '{role}' is configured. Available agents: {}.",
                    ensemble.agents().join(", ")
                ));
            }
            let mut lists = Vec::new();
            for key in ["constraints", "files", "done_when", "after"] {
                match args.get(key) {
                    None | Some(Value::Null) => lists.push(Vec::new()),
                    Some(Value::Array(items)) if items.iter().all(Value::is_string) => lists.push(
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    ),
                    Some(_) => {
                        return ToolResult::err(format!(
                            "{SUBMIT}: `{key}` must be a list of strings"
                        ));
                    }
                }
            }
            let [constraints, files, done_when, after] =
                <[Vec<String>; 4]>::try_from(lists).expect("four lists");
            let gate = match args.get("gate") {
                None | Some(Value::Null) => None,
                Some(g) => match serde_json::from_value::<crate::tasks::GateRef>(g.clone()) {
                    Ok(g) if g.attempts == 0 => {
                        return ToolResult::err(format!(
                            "{SUBMIT}: `gate.attempts` must be at least 1"
                        ));
                    }
                    Ok(g) if !ensemble.config().gates.contains_key(&g.name) => {
                        let known: Vec<&str> =
                            ensemble.config().gates.keys().map(String::as_str).collect();
                        return ToolResult::err(format!(
                            "{SUBMIT}: no gate '{}' is configured. Gates: {}.",
                            g.name,
                            if known.is_empty() {
                                "(none)".to_string()
                            } else {
                                known.join(", ")
                            }
                        ));
                    }
                    Ok(g) => Some(g),
                    Err(e) => {
                        return ToolResult::err(format!(
                            "{SUBMIT}: `gate` must be {{name, attempts}}: {e}"
                        ));
                    }
                },
            };
            let optional = |key: &str| text(key).filter(|s| !s.is_empty()).map(str::to_string);
            queue.submit(&crate::tasks::Ticket {
                title: title.to_string(),
                agent: role.to_string(),
                context: optional("context"),
                goal: optional("goal"),
                constraints,
                files,
                done_when,
                after,
                gate,
                text: task.to_string(),
                ..Default::default()
            })
        }

        STATUS => match args.get("task") {
            None | Some(Value::Null) => queue.status(None),
            Some(task) => match task.as_str() {
                Some(id) => queue.status(Some(id)),
                None => ToolResult::err(format!("{STATUS}: `task` must be a task id")),
            },
        },

        WAIT => {
            let Some(id) = args.get("task").and_then(Value::as_str) else {
                return ToolResult::err(format!(
                    "{WAIT}: `task` is required and must be a task id"
                ));
            };
            let seconds = args
                .get("timeout_seconds")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_WAIT_SECONDS)
                .clamp(1, MAX_WAIT_SECONDS);
            queue.wait(id, Duration::from_secs(seconds))
        }

        CANCEL => match args.get("task").and_then(Value::as_str) {
            Some(id) => queue.cancel(id),
            None => ToolResult::err(format!(
                "{CANCEL}: `task` is required and must be a task id"
            )),
        },

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
             {DELEGATE}, {SUBMIT}, {STATUS}, {WAIT}, {CANCEL}, {AGENTS}, {HEALTH}."
        )),
    }
}

/// Delegate and turn the outcome into a tool result: the agent's work and a
/// footer of what it did, or why it failed and whether another attempt could
/// work. The agent works in `workdir`, or in the project without one.
pub(super) fn run_delegation(
    ensemble: &Ensemble,
    role: &str,
    task: &str,
    context: Option<&str>,
    workdir: Option<&std::path::Path>,
    progress: &dyn Fn(&str),
    cancel: &AtomicBool,
) -> ToolResult {
    let outcome = match workdir {
        Some(dir) => ensemble.delegate_in(role, task, context, dir, progress, cancel),
        None => ensemble.delegate_reporting(role, task, context, progress, cancel),
    };
    match outcome {
        Ok(out) => ToolResult {
            text: format!("{}\n\n{}", out.text, agent_footer(role, &out.stats)),
            is_error: false,
        },
        Err(e) => {
            // Telling the model whether another attempt could work is
            // the difference between a useful retry and a loop against
            // a typo'd agent name.
            let timed_out = matches!(&e, RouteError::Agent(crate::port::Error::Timeout { .. }));
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
    fn the_task_tools_come_with_delegate() {
        let names: Vec<&str> = tools(&["coder"]).iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![DELEGATE, SUBMIT, STATUS, WAIT, CANCEL, AGENTS, HEALTH]
        );
    }

    #[test]
    fn a_long_role_prompt_is_clipped_to_one_marked_line() {
        let long = "x".repeat(400);
        let out = first_line(&format!("{long}\nsecond line"));
        assert!(out.ends_with('…'), "{out}");
        assert!(out.chars().count() <= 161, "{}", out.chars().count());
    }
}
