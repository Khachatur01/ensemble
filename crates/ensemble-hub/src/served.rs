//! The operations only the running hub answers, for an account, for
//! itself: its tasks, its knowledge and the runners that work for it. Each
//! is the link's request of the same kind, asked of the running hub
//! ([`HubContext::ask`]), as MCP's tools are, so the link, MCP, the REST API
//! and the web UI cannot differ.
//!
//! | Operation | The link's request | Permission |
//! |---|---|---|
//! | `task.projects` | `task_projects` | `tasks` |
//! | `task.list`, `task.show`, `task.create` | `tasks`, `task`, `task_create` | `tasks` |
//! | `task.answer`, `task.gate`, `task.cancel` | `task_answer`, `task_gate`, `task_cancel` | `tasks` |
//! | `task.reassign`, `task.confirm-gate` | `task_reassign`, `task_confirm_gate` | `tasks` |
//! | `task.choose` | `task_choose` | `tasks` |
//! | `runner.list` | `runners` | `tasks` |
//! | `knowledge.share` | `share` | `share` |
//! | `knowledge.find` | `recall` | `recall` |

use crate::accounts::HubContext;
use ensemble_link::protocol::{
    RECALL, RUNNERS, SHARE, TASK, TASK_ANSWER, TASK_CANCEL, TASK_CHOOSE, TASK_CONFIRM_GATE,
    TASK_CREATE, TASK_GATE, TASK_PROJECTS, TASK_REASSIGN, TASKS,
};
use ensemble_ops::{Access, Caller, Error, Field, Kind, Operation, Spec, operation};
use serde::Deserialize;
use serde_json::{Value, json};

/// Who may ask: an account, for itself; the running hub checks the permission
/// each needs, as it does on the link.
const SIGNED: Access = Access::Signed;

const PROJECT: Field = Field::text(
    "project",
    "The project: the name of its repository's folder, such as shop.",
)
.required();
const ID: Field = Field::text("id", "The task's id, such as T7.").required();
const ON_PROJECT: &[Field] = &[PROJECT];
const ON_TASK: &[Field] = &[PROJECT, ID];
const ANSWER: &[Field] = &[
    PROJECT,
    ID,
    Field::text("text", "The answer to the question the task asked.").required(),
];
/// A task's gate, as three fields a form shows: no command for none.
const GATE_COMMAND: Field = Field::text(
    "gate",
    "A shell command its work must pass, run in its working directory after the run is done; exit status 0 passes.",
);
const GATE_TIMEOUT: Field = Field::new(
    "gate_timeout_seconds",
    Kind::Number,
    "Seconds the gate may run before it counts as failed, at least 1.",
);
const GATE_ATTEMPTS: Field = Field::new(
    "gate_attempts",
    Kind::Number,
    "How many runs it gets to pass the gate, at least 1.",
);
const SET_GATE: &[Field] = &[PROJECT, ID, GATE_COMMAND, GATE_TIMEOUT, GATE_ATTEMPTS];
const REASSIGN: &[Field] = &[
    PROJECT,
    ID,
    Field::text(
        "to",
        "The account to give it to: its runners take it, once it confirms the task's gate.",
    )
    .required(),
];
const CHOOSE: &[Field] = &[
    PROJECT,
    ID,
    Field::text("runner", "The runner of yours to run it, connected now.").required(),
    Field::text("harness", "The harness of that runner to run it on, such as claude-code.").required(),
    Field::text(
        "model",
        "The model that harness drives, as the runner names it, such as sonnet or local:qwen3.5:9b.",
    )
    .required(),
];
const CREATE: &[Field] = &[
    PROJECT,
    Field::text("title", "What it is, in one line.").required(),
    Field::text(
        "task",
        "What to do, with everything the run needs: it sees nothing else.",
    )
    .required(),
    Field::text(
        "definition_of_done",
        "What must be true when it is done: the run checks its work against it.",
    )
    .required(),
    Field::text(
        "repository",
        "The URL or path its repository is cloned from.",
    ),
    Field::text("context", "What led to it."),
    Field::text("goal", "Why it matters."),
    Field::new("constraints", Kind::List, "What the run must keep to."),
    Field::new("files", Kind::List, "The files it is about."),
    Field::new("after", Kind::List, "The ids of the tasks it waits for."),
    GATE_COMMAND,
    GATE_TIMEOUT,
    GATE_ATTEMPTS,
];
const SHARE_FIELDS: &[Field] = &[
    Field::text("title", "What it is about, in a few words.").required(),
    Field::text(
        "text",
        "What is worth keeping, written to stand on its own.",
    )
    .required(),
    Field::new(
        "visibility",
        Kind::Choice(&["private", "hub"]),
        "Who may recall it: you alone (private, the default), or everyone on the hub.",
    ),
];
const FIND: &[Field] = &[Field::text("query", "The topic to recall notes about.").required()];

#[derive(Deserialize)]
struct Nothing {}

#[derive(Deserialize)]
struct OnProject {
    project: String,
}

#[derive(Deserialize)]
struct OnTask {
    project: String,
    id: String,
}

#[derive(Deserialize)]
struct Choose {
    project: String,
    id: String,
    runner: String,
    harness: String,
    model: String,
}

#[derive(Deserialize)]
struct Answer {
    project: String,
    id: String,
    text: String,
}

#[derive(Deserialize)]
struct Create {
    project: String,
    title: String,
    task: String,
    definition_of_done: String,
    repository: Option<String>,
    context: Option<String>,
    goal: Option<String>,
    #[serde(default)]
    constraints: Vec<String>,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    after: Vec<String>,
    gate: Option<String>,
    gate_timeout_seconds: Option<u64>,
    gate_attempts: Option<u64>,
}

#[derive(Deserialize)]
struct Reassign {
    project: String,
    id: String,
    to: String,
}

#[derive(Deserialize)]
struct SetGate {
    project: String,
    id: String,
    gate: Option<String>,
    gate_timeout_seconds: Option<u64>,
    gate_attempts: Option<u64>,
}

/// The gate its three fields say: null without a command, which a form
/// leaves empty for none, else the command with both numbers.
fn gate_of(
    command: Option<&str>,
    timeout_seconds: Option<u64>,
    attempts: Option<u64>,
) -> Result<Value, Error> {
    match (
        command.map(str::trim).filter(|c| !c.is_empty()),
        timeout_seconds,
        attempts,
    ) {
        (None, ..) => Ok(Value::Null),
        (Some(command), Some(timeout_seconds), Some(attempts)) => Ok(json!({
            "command": command,
            "timeout_seconds": timeout_seconds,
            "attempts": attempts,
        })),
        (_, None, _) => {
            Err(Error::invalid("a gate needs the seconds it may run").on("gate_timeout_seconds"))
        }
        (.., None) => Err(Error::invalid("a gate needs how many runs it gets").on("gate_attempts")),
    }
}

#[derive(Deserialize)]
struct Share {
    title: String,
    text: String,
    visibility: Option<String>,
}

#[derive(Deserialize)]
struct Find {
    query: String,
}

/// The operations the running hub answers.
pub(crate) fn operations() -> Vec<Operation<HubContext>> {
    vec![
        operation(
            Spec {
                name: "task.projects",
                summary: "The projects you have tasks in, each with how many.",
                access: SIGNED,
                input: &[],
            },
            |c: &HubContext, caller: &Caller, _: Nothing| c.ask(caller, TASK_PROJECTS, json!({})),
        ),
        operation(
            Spec {
                name: "task.list",
                summary: "Every task of one of your projects.",
                access: SIGNED,
                input: ON_PROJECT,
            },
            |c: &HubContext, caller: &Caller, input: OnProject| {
                c.ask(caller, TASKS, json!({ "project": input.project }))
            },
        ),
        operation(
            Spec {
                name: "task.show",
                summary: "One of your tasks, with its result, questions and hand-offs.",
                access: SIGNED,
                input: ON_TASK,
            },
            |c: &HubContext, caller: &Caller, input: OnTask| {
                let shown = c.ask(
                    caller,
                    TASK,
                    json!({ "project": input.project, "id": input.id }),
                )?;
                if shown["task"].is_null() {
                    return Err(Error::not_found(format!(
                        "no task '{}' in {}",
                        input.id, input.project
                    )));
                }
                Ok(shown)
            },
        ),
        operation(
            Spec {
                name: "task.create",
                summary: "Make a task for your runners.",
                access: SIGNED,
                input: CREATE,
            },
            |c: &HubContext, caller: &Caller, input: Create| {
                let gate = gate_of(
                    input.gate.as_deref(),
                    input.gate_timeout_seconds,
                    input.gate_attempts,
                )?;
                let args = json!({
                    "project": input.project,
                    "title": input.title,
                    "task": input.task,
                    "definition_of_done": input.definition_of_done,
                    "repository": input.repository,
                    "context": input.context,
                    "goal": input.goal,
                    "constraints": input.constraints,
                    "files": input.files,
                    "after": input.after,
                    "gate": gate,
                });
                let ticket = crate::mcp::ticket(&args, "task.create").map_err(Error::invalid)?;
                c.ask(
                    caller,
                    TASK_CREATE,
                    json!({ "project": input.project, "ticket": ticket }),
                )
            },
        ),
        operation(
            Spec {
                name: "task.answer",
                summary: "Answer the question one of your tasks asked, so it goes on.",
                access: SIGNED,
                input: ANSWER,
            },
            |c: &HubContext, caller: &Caller, input: Answer| {
                c.ask(
                    caller,
                    TASK_ANSWER,
                    json!({ "project": input.project, "id": input.id, "text": input.text }),
                )
            },
        ),
        operation(
            Spec {
                name: "task.gate",
                summary: "Give one of your tasks that is not running the gate its work must pass, or, with none, take it away.",
                access: SIGNED,
                input: SET_GATE,
            },
            |c: &HubContext, caller: &Caller, input: SetGate| {
                let gate = gate_of(
                    input.gate.as_deref(),
                    input.gate_timeout_seconds,
                    input.gate_attempts,
                )?;
                let gate = crate::mcp::gate(&json!({ "gate": gate }), "task.gate")
                    .map_err(Error::invalid)?;
                c.ask(
                    caller,
                    TASK_GATE,
                    json!({ "project": input.project, "id": input.id, "gate": gate }),
                )
            },
        ),
        operation(
            Spec {
                name: "task.confirm-gate",
                summary: "Confirm the gate of a task someone gave you, so your runners take it.",
                access: SIGNED,
                input: ON_TASK,
            },
            |c: &HubContext, caller: &Caller, input: OnTask| {
                c.ask(
                    caller,
                    TASK_CONFIRM_GATE,
                    json!({ "project": input.project, "id": input.id }),
                )
            },
        ),
        operation(
            Spec {
                name: "task.reassign",
                summary: "Give one of your tasks that is not running to another person, whose runners then take it.",
                access: SIGNED,
                input: REASSIGN,
            },
            |c: &HubContext, caller: &Caller, input: Reassign| {
                c.ask(
                    caller,
                    TASK_REASSIGN,
                    json!({ "project": input.project, "id": input.id, "to": input.to }),
                )
            },
        ),
        operation(
            Spec {
                name: "task.choose",
                summary: "Choose the worker of one of your tasks the decision models were not sure of: a runner of yours, and the harness and the model it runs it on.",
                access: SIGNED,
                input: CHOOSE,
            },
            |c: &HubContext, caller: &Caller, input: Choose| {
                c.ask(
                    caller,
                    TASK_CHOOSE,
                    json!({
                        "project": input.project,
                        "id": input.id,
                        "worker": { "runner": input.runner, "harness": input.harness, "model": input.model },
                    }),
                )
            },
        ),
        operation(
            Spec {
                name: "task.cancel",
                summary: "Cancel one of your tasks: before it starts, or its runner stops it.",
                access: SIGNED,
                input: ON_TASK,
            },
            |c: &HubContext, caller: &Caller, input: OnTask| {
                c.ask(
                    caller,
                    TASK_CANCEL,
                    json!({ "project": input.project, "id": input.id }),
                )
            },
        ),
        operation(
            Spec {
                name: "runner.list",
                summary: "The runners connected now that work for you, with the harnesses and models each runs tasks on.",
                access: SIGNED,
                input: &[],
            },
            |c: &HubContext, caller: &Caller, _: Nothing| c.ask(caller, RUNNERS, json!({})),
        ),
        operation(
            Spec {
                name: "knowledge.share",
                summary: "Keep something in the knowledge vault, to be turned into notes.",
                access: SIGNED,
                input: SHARE_FIELDS,
            },
            |c: &HubContext, caller: &Caller, input: Share| {
                c.ask(
                    caller,
                    SHARE,
                    json!({
                        "title": input.title,
                        "text": input.text,
                        "visibility": input.visibility,
                        "client": "api",
                    }),
                )
            },
        ),
        operation(
            Spec {
                name: "knowledge.find",
                summary: "The notes about a topic that you may recall.",
                access: SIGNED,
                input: FIND,
            },
            |c: &HubContext, caller: &Caller, input: Find| {
                c.ask(caller, RECALL, json!({ "query": input.query }))
            },
        ),
    ]
}
