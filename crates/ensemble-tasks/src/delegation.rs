//! The tools an agent calls to use the ensemble: delegate, submit and follow
//! tasks, list agents, check health. The schemas and the execution live here;
//! `ensemble-mcp` wraps them in its JSON-RPC envelope (`inputSchema`).
//!
//! No tool takes a backend or a model. The caller names an *agent* and Ensemble
//! decides what runs it — that is the whole point of the indirection, and a
//! `model` parameter would quietly undo it at the first opportunity.

use super::queue::{Ask, Queue};
use crate::tasks::question_in;
use ensemble_agents::router::{Ensemble, RouteError};
use ensemble_agents::runtime::blocking;
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

pub const DELEGATE: &str = "delegate";
pub const SUBMIT: &str = "submit";
pub const STATUS: &str = "status";
pub const WAIT: &str = "wait";
pub const CANCEL: &str = "cancel";
pub const ANSWER: &str = "answer";
pub const ASSIGN: &str = "assign";
pub const REVIEW: &str = "review";

/// Seconds `wait` waits when the caller names no time, and the most it may.
const DEFAULT_WAIT_SECONDS: u64 = 30;
const MAX_WAIT_SECONDS: u64 = 300;
pub const AGENTS: &str = "agents";
pub const HEALTH: &str = "health";
pub const TEAMS: &str = "teams";
pub const TEAM_CREATE: &str = "team_create";

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
                        "description": "Which agent does the work. Required unless you `continue` a question."
                    },
                    "task": {
                        "type": "string",
                        "description": "The task, stated so it can be acted on without further questions. With `continue`, your answer to the agent's question."
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional background: prior findings, file contents, constraints."
                    },
                    "repo": {
                        "type": "string",
                        "description": REPO_DESCRIPTION
                    },
                    "continue": {
                        "type": "string",
                        "description": "The session of a question an agent stopped to ask (a reply that starts with needs_input: true). Put your answer in `task`; `agent` is then not needed, and the same agent goes on with what it had."
                    },
                    "background": {
                        "type": "boolean",
                        "description": "Return at once with an id instead of waiting for the agent. \
                                        See what it is doing with `status`, and get its reply with `wait`. \
                                        Nothing is written to the project's tasks."
                    }
                },
                "required": ["task"],
                "additionalProperties": false
            }),
        });

        out.push(Tool {
            name: SUBMIT,
            description: format!(
                "Make a task for an agent, kept on the hub for this project, and return its id at \
                 once. Your nodes work through tasks while they run, as many at a time as each \
                 agent allows, and go on after this session ends. Available agents: \
                 {agent_list}. Follow it with `status`, collect its result with `wait`."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "enum": agents,
                        "description": "Who does the work. Leave it out to put the task in the backlog for someone to assign."
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
                    "repo": {
                        "type": "string",
                        "description": REPO_DESCRIPTION
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
                "required": ["title", "task"],
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

        out.push(Tool {
            name: ASSIGN,
            description:
                "Assign a task to an agent: a task in the backlog (no assignee yet), or one \
                          that should go to someone else. An agent with no task assigned stays \
                          idle. Not for a task that is running or has ended."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "The task id." },
                    "agent": { "type": "string", "description": "Who gets it." }
                },
                "required": ["task", "agent"],
                "additionalProperties": false
            }),
        });

        out.push(Tool {
            name: ANSWER,
            description: "Answer the question a task stopped to ask (`status` and `wait` show it \
                          as `needs input`). The task then runs again with the questions and \
                          answers so far."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "The task that asked." },
                    "answer": { "type": "string", "description": "The decision or information it asked for." }
                },
                "required": ["task", "answer"],
                "additionalProperties": false
            }),
        });
    }

    if !agents.is_empty() {
        out.push(Tool {
            name: REVIEW,
            description:
                "Look at the work of a finished task of an agent that works in a git worktree: \
                      its branch, what changed on it, and its result. It changes nothing, and \
                      there is no tool that merges: the owner merges, with `ensemble merge`."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "The task id." }
                },
                "required": ["task"],
                "additionalProperties": false
            }),
        });
    }

    out.push(Tool {
        name: AGENTS,
        description: "List the agents of the helper team, with what each is for. Call this \
                      first if you are unsure who should do a piece of work."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    });

    out.push(Tool {
        name: TEAMS,
        description: "List the helper teams of the node, where each comes from, and their \
                      agents, named `<team>/<agent>`. A team written with `team_create` is \
                      listed as waiting for approval until the person approves it. With \
                      `sample`, return instead a sample config.yaml with every setting, the \
                      form `team_create` takes."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "sample": {
                    "type": "boolean",
                    "description": "Return the sample config.yaml instead of the list."
                }
            },
            "additionalProperties": false
        }),
    });

    out.push(Tool {
        name: TEAM_CREATE,
        description: "Write a new helper team for the node, when the person asks for one: its \
                      name and its whole config.yaml (version 1, every setting written; \
                      `teams` with `sample` shows the form). It is checked, then kept \
                      waiting: it runs nothing until the person reads and approves it with \
                      `ensemble team approve <name>` in a terminal; the node then uses it at once. \
                      Say so to the person. It cannot change a team that exists."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The team's name: letters, digits, '-', '_' or '.', such as web or pcb-design."
                },
                "config": {
                    "type": "string",
                    "description": "The team's config.yaml, whole."
                }
            },
            "required": ["name", "config"],
            "additionalProperties": false
        }),
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
/// A background delegation is registered in `queue`, which runs it in
/// [`Queue::work`], so that this call can return at once. A blocking one waits
/// for a slot of its agent first.
pub async fn call(
    ensemble: &Ensemble,
    name: &str,
    args: &Value,
    progress: &(dyn Fn(&str) + Sync),
    cancel: &AtomicBool,
    queue: &Queue<'_>,
) -> ToolResult {
    match name {
        DELEGATE => {
            let resumed = match args.get("continue") {
                None | Some(Value::Null) => None,
                Some(Value::String(token)) => match queue.take_ask(token.trim()) {
                    Some(ask) => Some(ask),
                    None => {
                        return ToolResult::err(format!(
                            "{DELEGATE}: no question is waiting under '{token}'. A session is for one answer, and ends with the MCP session; \
                             delegate again with everything the agent needs."
                        ));
                    }
                },
                Some(_) => {
                    return ToolResult::err(format!(
                        "{DELEGATE}: `continue` must be the session of a question"
                    ));
                }
            };
            let Some(given) = args.get("task").and_then(Value::as_str) else {
                return ToolResult::err(format!(
                    "{DELEGATE}: `task` is required and must be a string"
                ));
            };
            if given.trim().is_empty() {
                return ToolResult::err(format!(
                    "{DELEGATE}: `task` is empty — {}",
                    if resumed.is_some() {
                        "with `continue` it is your answer"
                    } else {
                        "there is nothing to do"
                    }
                ));
            }
            let background = resumed.is_none()
                && args
                    .get("background")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);

            // What is delegated: the agent, the task as asked, its context and
            // the repository to work in. An answer resumes what stopped.
            let (role, asked, context, repo, prompt) = match resumed {
                Some(ask) => {
                    let prompt = ask.resume_prompt(given);
                    (ask.agent, ask.task, ask.context, ask.repo, prompt)
                }
                None => {
                    let Some(role) = args.get("agent").and_then(Value::as_str) else {
                        return ToolResult::err(format!(
                            "{DELEGATE}: `agent` is required and must be a string (unless you `continue` a question)"
                        ));
                    };
                    let repo = match repo_of(ensemble, args) {
                        Ok(repo) => repo,
                        Err(e) => return ToolResult::err(format!("{DELEGATE}: {e}")),
                    };
                    // What the knowledge vault has about it comes with the
                    // context, and stays with it when the agent asks.
                    let context = queue
                        .with_notes(
                            role,
                            given,
                            args.get("context").and_then(Value::as_str),
                            progress,
                        )
                        .await;
                    let prompt = if background {
                        given.to_string()
                    } else {
                        format!("{given}\n\n{ASK_INSTRUCTION}")
                    };
                    (role.to_string(), given.to_string(), context, repo, prompt)
                }
            };
            let (role, context) = (role.as_str(), context.as_deref());

            // An unknown agent fails at once, so it takes the blocking path
            // and says so now rather than inside a background run.
            if ensemble.config().agent(role).is_none() {
                return run_delegation(
                    ensemble,
                    role,
                    &prompt,
                    context,
                    repo.as_deref(),
                    progress,
                    cancel,
                )
                .await;
            }
            if let Err(refused) = queue.spend(role) {
                return ToolResult::err(format!("delegation refused: {refused}"));
            }
            if background {
                let id = queue.start_adhoc(role, &prompt, context, repo);
                return ToolResult::ok(format!(
                    "{id} started. `status` shows what it is doing; `wait` with task {id} \
                     returns its reply."
                ));
            }
            let waiting = || progress(&format!("{role}: waiting for a free slot"));
            let Some(_slot) = queue.slot(role, cancel, &waiting).await else {
                return ToolResult::err(format!(
                    "delegation cancelled while it waited for a free slot of '{role}'."
                ));
            };
            let result = run_delegation(
                ensemble,
                role,
                &prompt,
                context,
                repo.as_deref(),
                progress,
                cancel,
            )
            .await;
            // An agent that stopped to ask is a question for the caller, with
            // somewhere to send the answer, not an answer that fell short.
            match (result.is_error, question_in(&result.text)) {
                (false, Some(question)) => {
                    let session = queue.remember(Ask {
                        agent: role.to_string(),
                        task: asked,
                        context: context.map(str::to_string),
                        repo,
                        question: question.clone(),
                        reply: result.text.clone(),
                    });
                    ToolResult::ok(format!(
                        "needs_input: true\nsession: {session}\nquestion: {question}\n\n\
                         The agent stopped to ask. Answer it with `{DELEGATE}`, `continue` set to \
                         {session} and your answer as `task`.\n\n{}",
                        result.text
                    ))
                }
                _ => result,
            }
        }

        SUBMIT => {
            let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::trim);
            let (Some(title), Some(task)) = (text("title"), text("task")) else {
                return ToolResult::err(format!(
                    "{SUBMIT}: `title` and `task` are required and must be strings"
                ));
            };
            if title.is_empty() || task.is_empty() {
                return ToolResult::err(format!("{SUBMIT}: `title` and `task` cannot be empty"));
            }
            if title.contains('\n') {
                return ToolResult::err(format!("{SUBMIT}: `title` is one line"));
            }
            // With an agent the task is assigned; without one it goes to the
            // backlog, for the calling agent to assign.
            let role = match text("agent").filter(|a| !a.is_empty()) {
                Some(agent) => match ensemble.config().agent(agent) {
                    Some(a) => a.name.clone(),
                    None => {
                        return ToolResult::err(format!(
                            "{SUBMIT}: no agent '{agent}'. Agents: {}.",
                            ensemble.config().agent_names().join(", ")
                        ));
                    }
                },
                None => String::new(),
            };
            let role = &role;
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
                    Ok(g) if ensemble.config().gate_for(role, &g.name).is_none() => {
                        let config = ensemble.config();
                        let known: Vec<&str> = config.gates.keys().map(String::as_str).collect();
                        return ToolResult::err(format!(
                            "{SUBMIT}: no gate '{}' is configured under `gates`. Gates: {}.",
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
            if let Err(e) = repo_of(ensemble, args) {
                return ToolResult::err(format!("{SUBMIT}: {e}"));
            }
            queue
                .submit(&crate::tasks::Ticket {
                    title: title.to_string(),
                    agent: role.to_string(),
                    context: optional("context"),
                    goal: optional("goal"),
                    repo: optional("repo"),
                    constraints,
                    files,
                    done_when,
                    after,
                    gate,
                    text: task.to_string(),
                    ..Default::default()
                })
                .await
        }

        STATUS => match args.get("task") {
            None | Some(Value::Null) => queue.status(None).await,
            Some(task) => match task.as_str() {
                Some(id) => queue.status(Some(id)).await,
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
            queue.wait(id, Duration::from_secs(seconds)).await
        }

        CANCEL => match args.get("task").and_then(Value::as_str) {
            Some(id) => queue.cancel(id).await,
            None => ToolResult::err(format!(
                "{CANCEL}: `task` is required and must be a task id"
            )),
        },

        REVIEW => {
            let Some(id) = args.get("task").and_then(Value::as_str) else {
                return ToolResult::err(format!(
                    "{REVIEW}: `task` is required and must be a task id"
                ));
            };
            let workdir = match ensemble.workdir() {
                Ok(dir) => dir,
                Err(e) => return ToolResult::err(format!("{REVIEW}: {e}")),
            };
            let detail = match queue.detail(id).await {
                Ok(detail) => detail,
                Err(e) => return ToolResult::err(format!("{REVIEW}: {e}")),
            };
            match blocking(|| {
                crate::review::find(detail, &workdir, id)
                    .and_then(|task| crate::review::review(&task))
            }) {
                Ok(text) => ToolResult::ok(text),
                Err(e) => ToolResult::err(format!("{REVIEW}: {e}")),
            }
        }

        ASSIGN => {
            let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::trim);
            match (text("task"), text("agent")) {
                (Some(id), Some(agent)) if !agent.is_empty() => queue.assign(id, agent).await,
                _ => ToolResult::err(format!("{ASSIGN}: `task` and `agent` are required")),
            }
        }

        ANSWER => {
            let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::trim);
            match (text("task"), text("answer")) {
                (Some(id), Some(answer)) if !answer.is_empty() => queue.answer(id, answer).await,
                _ => ToolResult::err(format!(
                    "{ANSWER}: `task` and a non-empty `answer` are required"
                )),
            }
        }

        AGENTS => {
            let config = ensemble.config();
            let line = |role: &str| {
                // Where it runs and the model are deliberately absent: the caller must
                // not start choosing them.
                let purpose = config
                    .agent(role)
                    .and_then(|a| a.prompt.as_deref())
                    .map(first_line)
                    .unwrap_or_else(|| "(no description configured)".to_string());
                let kind = match config.agent(role) {
                    Some(a) if a.can_change_files() => " [can change files]",
                    Some(a) if a.has_tools() => " [read-only]",
                    _ => "",
                };
                // Whether it can run now names no model, and helps the caller plan.
                let note = ensemble
                    .fallback_note(role)
                    .map_or(String::new(), |n| format!(" ({n})"));
                format!("{role}{kind}{note}: {purpose}")
            };
            let lines: Vec<String> = config.agent_names().into_iter().map(line).collect();
            ToolResult::ok(lines.join("\n"))
        }

        TEAMS => {
            if args.get("sample").and_then(Value::as_bool) == Some(true) {
                return ToolResult::ok(ensemble_config::SAMPLE);
            }
            let config = ensemble.config();
            let entries = ensemble_config::teams::list(&config.source);
            if entries.is_empty() {
                return ToolResult::ok(
                    "The node has no helper teams. Make one with `team_create` when the person asks, \
                     or the person makes one with `ensemble team new` or pulls one with \
                     `ensemble team pull`.",
                );
            }
            let mut lines = Vec::new();
            for team in &entries {
                lines.push(format!("{} ({})", team.name, team.describe()));
                let prefix = format!("{}/", team.name);
                let agents: Vec<&str> = config
                    .agent_names()
                    .into_iter()
                    .filter(|a| a.starts_with(&prefix))
                    .collect();
                if agents.is_empty() && !team.pending {
                    lines.push(
                        "  (not running: changed by hand since the node last read its teams, or it has no agents; `ensemble node reload`)".into(),
                    );
                }
                for agent in agents {
                    let purpose = config
                        .agent(agent)
                        .and_then(|a| a.prompt.as_deref())
                        .map(first_line)
                        .unwrap_or_else(|| "(no description configured)".to_string());
                    lines.push(format!("  {agent}: {purpose}"));
                }
            }
            ToolResult::ok(lines.join("\n"))
        }

        TEAM_CREATE => {
            let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or("");
            let (name, yaml) = (text("name").trim(), text("config"));
            if name.is_empty() || yaml.trim().is_empty() {
                return ToolResult::err(format!(
                    "{TEAM_CREATE}: needs the team's `name` and its whole `config`"
                ));
            }
            let dir = &ensemble.config().source;
            if !dir.is_dir() {
                return ToolResult::err(format!(
                    "{TEAM_CREATE}: this helper team is not a node's teams, so no team can be added to it"
                ));
            }
            match blocking(|| {
                ensemble_config::teams::create_pending(
                    dir,
                    name,
                    yaml,
                    "an agent, at the person's request",
                )
            }) {
                Ok(path) => ToolResult::ok(format!(
                    "Wrote the team '{name}' in {}. It runs nothing until the person approves it: \
                     tell them to read it and run `ensemble team approve {name}`; the node uses it at once.",
                    path.display()
                )),
                Err(e) => ToolResult::err(format!("{TEAM_CREATE}: {e}")),
            }
        }

        HEALTH => {
            let mut lines = Vec::new();
            let mut any_bad = false;
            for (backend, result) in blocking(|| ensemble.health()) {
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
             {DELEGATE}, {SUBMIT}, {STATUS}, {WAIT}, {CANCEL}, {ANSWER}, {ASSIGN}, {REVIEW}, {AGENTS}, {TEAMS}, {TEAM_CREATE}, {HEALTH}."
        )),
    }
}

/// Told to an agent a blocking delegation goes to, so that it can stop and ask.
pub(crate) const ASK_INSTRUCTION: &str = "If you cannot finish without a decision only the person who asked can make, stop and end your reply with one line: NEEDS INPUT: <your question>";

const REPO_DESCRIPTION: &str = "Optional: a folder of the working directory to work in, such as one repository of several. A path relative to the working directory; one that leaves it is refused.";

/// The `repo` argument as a directory inside the working directory, or why it
/// is not one. `None` when there is none.
fn repo_of(ensemble: &Ensemble, args: &Value) -> Result<Option<std::path::PathBuf>, String> {
    let repo = match args.get("repo") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(repo)) if repo.trim().is_empty() => return Ok(None),
        Some(Value::String(repo)) => repo.trim(),
        Some(_) => return Err("`repo` must be a string".into()),
    };
    let workdir = ensemble
        .workdir()
        .map_err(|e| format!("no working directory: {e}"))?;
    ensemble_core::workspace::resolve_repo(&workdir, repo).map(Some)
}

/// Delegate and turn the outcome into a tool result: the agent's work and a
/// footer of what it did, or why it failed and whether another attempt could
/// work. The agent works in `workdir`, or in the project without one.
pub(super) async fn run_delegation(
    ensemble: &Ensemble,
    role: &str,
    task: &str,
    context: Option<&str>,
    workdir: Option<&std::path::Path>,
    progress: &(dyn Fn(&str) + Sync),
    cancel: &AtomicBool,
) -> ToolResult {
    let outcome = match workdir {
        Some(dir) => {
            ensemble
                .delegate_in(role, task, context, dir, progress, cancel)
                .await
        }
        None => {
            ensemble
                .delegate_reporting(role, task, context, progress, cancel)
                .await
        }
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
            let timed_out = matches!(
                &e,
                RouteError::Agent(ensemble_core::port::Error::Timeout { .. })
            );
            let hint = if timed_out {
                " This may be transient; the same call is worth one retry."
            } else if e.is_retryable() {
                " Call `health`, and retry once what it reports is fixed."
            } else {
                " Retrying this unchanged will fail the same way."
            };
            let detail = match &e {
                RouteError::UnknownAgent { .. } | RouteError::NoBackend { .. } => String::new(),
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
fn agent_footer(role: &str, a: &ensemble_agents::router::AgentStats) -> String {
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
        assert_eq!(names, vec![AGENTS, TEAMS, TEAM_CREATE, HEALTH]);
    }

    #[test]
    fn the_task_tools_come_with_delegate() {
        let names: Vec<&str> = tools(&["coder"]).iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                DELEGATE,
                SUBMIT,
                STATUS,
                WAIT,
                CANCEL,
                ASSIGN,
                ANSWER,
                REVIEW,
                AGENTS,
                TEAMS,
                TEAM_CREATE,
                HEALTH
            ]
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
