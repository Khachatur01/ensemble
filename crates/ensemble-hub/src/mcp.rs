//! The hub's MCP over HTTP, at `/mcp` on the hub's port, for client agents:
//! Claude Code, Codex, OpenCode, claude.ai. A client adds the hub with its URL
//! and an account's token as the API key (`Authorization: Bearer <token>`),
//! and installs nothing of Ensemble.
//!
//! It is MCP's Streamable HTTP transport at its simplest: each JSON-RPC
//! message is one `POST`, answered with one JSON response, or with `202` for a
//! notification. The hub sends nothing unasked, so there is no event stream
//! (`GET` is refused) and no session id.
//!
//! The tools are what the token may do: `share` and `recall`, and with
//! `tasks` on a hub that keeps them, the tasks of the account's projects.
//! Each runs as the link's request of the same name would
//! ([`Server::answer`]), so the link and MCP cannot differ.

use crate::http::{self, Head};
use crate::server::{Server, Session};
use ensemble_config::Permission;
use ensemble_core::runtime::blocking;
use ensemble_knowledge::Visibility;
use ensemble_link::protocol::{
    RECALL, RUNNERS, Request, SHARE, TASK, TASK_ANSWER, TASK_CANCEL, TASK_CREATE, TASK_GATE, TASKS,
};
use serde_json::{Map, Value, json};
use tokio::net::TcpStream;

/// The path MCP is served at.
pub(crate) const PATH: &str = "/mcp";

/// The protocol version the hub speaks when the client names none.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// The most a message may be: a share may be long.
const MAX_MESSAGE: usize = 1024 * 1024;

/// The tools of tasks, and the link's method each runs.
const RUNNERS_TOOL: &str = "runners";
const SUBMIT: &str = "submit";
const LIST: &str = "tasks";
const SHOW: &str = "task";
const ANSWER: &str = "answer";
const GATE: &str = "gate";
const CANCEL: &str = "cancel";

impl Server<'_> {
    /// Answer one MCP request on `stream`, then close it.
    pub(crate) async fn mcp(
        &self,
        mut stream: TcpStream,
        head: &Head,
        already: &[u8],
        say: &(dyn Fn(&str) + Sync),
    ) -> Result<(), String> {
        if head.method != "POST" {
            http::respond_with(
                &mut stream,
                405,
                &[("allow", "POST")],
                Some(&json!({"error": "MCP here is a POST of one JSON-RPC message; the hub sends nothing unasked"})),
            )
            .await
            .map_err(|e| e.to_string())?;
            return Ok(());
        }
        let session = match head.bearer() {
            Some(token) => {
                let token = token.to_string();
                blocking(|| self.session(token))
            }
            None => None,
        };
        let Some(session) = session else {
            say(&format!("POST {PATH} - 401"));
            http::respond_with(
                &mut stream,
                401,
                &[("www-authenticate", "Bearer")],
                Some(&json!({"error": "a valid token is needed: Authorization: Bearer <token>"})),
            )
            .await
            .map_err(|e| e.to_string())?;
            http::drain(&mut stream).await;
            return Ok(());
        };
        let message = match http::read_body_up_to(&mut stream, head, already, MAX_MESSAGE).await {
            Ok(body) => serde_json::from_slice::<Value>(&body)
                .map_err(|e| failure(Value::Null, -32700, format!("parse error: {e}"))),
            Err(e) => {
                http::respond(&mut stream, 413, &json!({"error": e}))
                    .await
                    .map_err(|e| e.to_string())?;
                http::drain(&mut stream).await;
                return Ok(());
            }
        };
        let answer = match message {
            Ok(message) => blocking(|| self.message(&session, &message, say)),
            Err(failed) => Some(failed),
        };
        match answer {
            Some(answer) => http::respond(&mut stream, 200, &answer).await,
            None => http::respond_with(&mut stream, 202, &[], None).await,
        }
        .map_err(|e| e.to_string())
    }

    /// The answer to one JSON-RPC message, or `None` for a notification or a
    /// client's response, which get none.
    fn message(
        &self,
        session: &Session,
        message: &Value,
        say: &(dyn Fn(&str) + Sync),
    ) -> Option<Value> {
        let id = message.get("id").cloned().filter(|id| !id.is_null());
        let method = message.get("method").and_then(Value::as_str);
        let (Some(id), Some(method)) = (id, method) else {
            return None;
        };
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        let permissions = match self.permissions(session) {
            Ok(p) => p,
            Err(e) => return Some(failure(id, -32001, e)),
        };
        let result = match method {
            "initialize" => {
                // Echo the client's version: insisting on ours would refuse
                // clients that are otherwise compatible.
                let version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(PROTOCOL_VERSION);
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "ensemble", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": self.instructions(&permissions),
                })
            }
            "ping" => json!({}),
            "tools/list" => json!({ "tools": self.tools(&permissions) }),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                say(&format!("{}: mcp {name}", session.user));
                let (text, failed) = match self.call(session, &permissions, name, &args) {
                    Ok(text) => (text, false),
                    Err(text) => (text, true),
                };
                json!({ "content": [{ "type": "text", "text": text }], "isError": failed })
            }
            other => return Some(failure(id, -32601, format!("method not found: {other}"))),
        };
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    /// The tools `permissions` allow.
    fn tools(&self, permissions: &[Permission]) -> Vec<Value> {
        let mut tools: Vec<Value> = self
            .memory
            .tools()
            .into_iter()
            .filter(|t| permissions.iter().any(|p| p.as_str() == t.name))
            .map(|t| json!({ "name": t.name, "description": t.description, "inputSchema": t.parameters }))
            .collect();
        if self.keeps_tasks() && permissions.contains(&Permission::Tasks) {
            tools.extend(task_tools());
        }
        tools
    }

    fn keeps_tasks(&self) -> bool {
        self.tasks.is_some()
    }

    /// What the hub tells a client agent about itself when it connects.
    fn instructions(&self, permissions: &[Permission]) -> String {
        let knowledge: Vec<&str> = permissions
            .iter()
            .map(|p| p.as_str())
            .filter(|p| [SHARE, RECALL].contains(p))
            .collect();
        let mut out = format!("This is the Ensemble hub of {}.", self.memory.owner());
        if !knowledge.is_empty() {
            out.push_str(&ensemble_knowledge::instructions_for(&knowledge));
        }
        if self.keeps_tasks() && permissions.contains(&Permission::Tasks) {
            out.push_str(
                "\n\nThe user's runners work through tasks kept here, each on a harness and a \
                 model the runner picks. Make one with `submit` when the user wants work done \
                 that goes on without this conversation, naming the project (the repository's \
                 name), and its `repository` (the URL or path to clone it from) when you know it, \
                 and giving the run everything it needs: it cannot see this conversation. \
                 `runners` lists the runners connected now and what each runs on. Follow a task \
                 with `tasks` and `task`; when it asks a question, ask the user and `answer` it. \
                 A task is `blocked` when every runner gave it back, saying why in `given_back`: \
                 tell the user, and `answer` or make it again once that is fixed.",
            );
        }
        out
    }

    /// Run the tool `name`: its text, or why it failed.
    fn call(
        &self,
        session: &Session,
        permissions: &[Permission],
        name: &str,
        args: &Value,
    ) -> Result<String, String> {
        let allowed = |p: Permission| permissions.contains(&p);
        let ask = |method: &str, params: Value| -> Result<Value, String> {
            let reply = self.answer(
                session,
                0,
                Request {
                    id: 0,
                    method: method.into(),
                    params,
                },
            );
            match (reply.result, reply.error) {
                (_, Some(e)) => Err(e),
                (Some(result), None) => Ok(result),
                (None, None) => Err(format!("{method}: no answer")),
            }
        };
        let tasks = || -> Result<(), String> {
            if self.keeps_tasks() && allowed(Permission::Tasks) {
                Ok(())
            } else {
                Err(format!("no tool `{name}` for you here"))
            }
        };
        let field = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
        };
        // What the task tools pass on as they are.
        let pass = |keys: &[&str]| -> Value {
            Value::Object(
                keys.iter()
                    .map(|k| (k.to_string(), json!(field(k))))
                    .collect::<Map<_, _>>(),
            )
        };
        match name {
            SHARE if allowed(Permission::Share) => {
                let mut params = pass(&["title", "text"]);
                params["visibility"] = args.get("visibility").cloned().unwrap_or(Value::Null);
                params["client"] = json!("mcp");
                let shared = ask(SHARE, params)?;
                let visibility = shared["visibility"]
                    .as_str()
                    .and_then(Visibility::parse)
                    .unwrap_or_default();
                Ok(format!(
                    "Shared as {} ({}). The hub turns it into notes in the background.",
                    shared["id"].as_str().unwrap_or("?"),
                    ensemble_knowledge::shared_with(visibility)
                ))
            }
            RECALL if allowed(Permission::Recall) => {
                let recalled = ask(RECALL, pass(&["query"]))?;
                Ok(recalled["text"].as_str().unwrap_or("").to_string())
            }
            RUNNERS_TOOL => {
                tasks()?;
                let listed = ask(RUNNERS, json!({}))?;
                Ok(runners_text(&listed))
            }
            SUBMIT => {
                tasks()?;
                let ticket = ticket(args, SUBMIT)?;
                let made = ask(
                    TASK_CREATE,
                    json!({ "project": field("project"), "ticket": ticket }),
                )?;
                Ok(format!(
                    "Made task {} in {}.",
                    made["id"].as_str().unwrap_or("?"),
                    field("project"),
                ))
            }
            LIST => {
                tasks()?;
                pretty(&ask(TASKS, pass(&["project"]))?["tasks"])
            }
            SHOW => {
                tasks()?;
                let shown = ask(TASK, pass(&["project", "id"]))?;
                if shown["task"].is_null() {
                    return Err(format!("no task '{}' in {}", field("id"), field("project")));
                }
                pretty(&shown["task"])
            }
            ANSWER => {
                tasks()?;
                ask(TASK_ANSWER, pass(&["project", "id", "text"]))?;
                Ok(format!("Answered task {}; it goes on.", field("id")))
            }
            GATE => {
                tasks()?;
                let gate = gate(args, GATE)?;
                let what = match gate["command"].as_str() {
                    Some(command) => format!("Task {} must now pass `{command}`.", field("id")),
                    None => format!("Task {} has no gate now.", field("id")),
                };
                ask(
                    TASK_GATE,
                    json!({ "project": field("project"), "id": field("id"), "gate": gate }),
                )?;
                Ok(what)
            }
            CANCEL => {
                tasks()?;
                let cancelled = ask(TASK_CANCEL, pass(&["project", "id"]))?;
                Ok(match cancelled["cancel"].as_str() {
                    Some("before") => format!("Cancelled task {} before it started.", field("id")),
                    Some("running") => {
                        format!("Cancelled task {}; its runner stops it.", field("id"))
                    }
                    _ => format!("Task {} had ended already.", field("id")),
                })
            }
            _ => Err(format!("no tool `{name}` for you here")),
        }
    }
}

/// A JSON-RPC error.
fn failure(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn pretty(value: &Value) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|e| e.to_string())
}

/// The runners `listed`, the user's own, each with what it runs tasks on.
fn runners_text(listed: &Value) -> String {
    let mut lines = Vec::new();
    for runner in listed["runners"].as_array().into_iter().flatten() {
        let name = runner["runner"].as_str().unwrap_or("?");
        let harnesses: Vec<String> = runner["harnesses"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|h| {
                let models: Vec<String> = h["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|m| {
                        format!(
                            "{} ({})",
                            m["model"].as_str().unwrap_or("?"),
                            m["tier"].as_str().unwrap_or("?")
                        )
                    })
                    .collect();
                format!(
                    "{} on {}",
                    h["harness"].as_str().unwrap_or("?"),
                    models.join(", ")
                )
            })
            .collect();
        lines.push(if harnesses.is_empty() {
            format!("- {name}: runs no task (no harness is on with a model it can drive)")
        } else {
            format!("- {name}: {}", harnesses.join("; "))
        });
    }
    if lines.is_empty() {
        "No runner that works for you is connected now. A task still waits for one.".into()
    } else {
        lines.join("\n")
    }
}

/// The ticket of a new task from `args`, as `what` (a tool or an operation)
/// takes them: refused, named after `what`, when they cannot be one.
pub(crate) fn ticket(args: &Value, what: &str) -> Result<Value, String> {
    let text = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
    };
    let (title, task) = (text("title"), text("task"));
    if title.is_empty() || task.is_empty() || text("project").is_empty() {
        return Err(format!(
            "{what}: `project`, `title` and `task` are required"
        ));
    }
    if title.contains('\n') {
        return Err(format!("{what}: `title` is one line"));
    }
    let optional = |key: &str| Some(text(key)).filter(|t| !t.is_empty());
    let mut lists = Map::new();
    for key in ["constraints", "files", "done_when", "after"] {
        let list = match args.get(key) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|i| i.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| format!("{what}: `{key}` must be a list of strings"))?,
            Some(_) => return Err(format!("{what}: `{key}` must be a list of strings")),
        };
        lists.insert(key.into(), json!(list));
    }
    let gate = gate(args, what)?;
    let repository = optional("repository");
    if let Some(repository) = repository {
        ensemble_tasks::tasks::check_repository(repository).map_err(|e| format!("{what}: {e}"))?;
    }
    let mut ticket = json!({
        "id": "",
        "title": title,
        "context": optional("context"),
        "goal": optional("goal"),
        "repository": repository,
        "repo": optional("repo"),
        "gate": gate,
        "text": task,
        "notes": null,
    });
    ticket.as_object_mut().expect("an object").extend(lists);
    Ok(ticket)
}

/// The gate in `args`, as `what` takes it: null when there is none, refused,
/// named after `what`, when it cannot be one. The command is the account's
/// own, run only on its runners.
pub(crate) fn gate(args: &Value, what: &str) -> Result<Value, String> {
    let gate = match args.get("gate") {
        None | Some(Value::Null) => return Ok(Value::Null),
        Some(gate) => gate,
    };
    let command = gate["command"].as_str().map(str::trim).unwrap_or("");
    let number = |key: &str| {
        gate[key]
            .as_u64()
            .filter(|n| (1..=u64::from(u32::MAX)).contains(n))
    };
    let known = gate.as_object().is_some_and(|g| {
        g.keys()
            .all(|k| ["command", "timeout_seconds", "attempts"].contains(&k.as_str()))
    });
    match (number("timeout_seconds"), number("attempts")) {
        (Some(timeout_seconds), Some(attempts)) if !command.is_empty() && known => Ok(json!({
            "command": command,
            "timeout_seconds": timeout_seconds,
            "attempts": attempts,
        })),
        _ => Err(format!(
            "{what}: `gate` is {{command, timeout_seconds, attempts}}: the shell command the work must pass, the seconds it may run, and how many runs it gets to pass it, each at least 1"
        )),
    }
}

/// The tools of tasks, in MCP's form.
fn task_tools() -> Vec<Value> {
    let project = json!({
        "type": "string",
        "description": "The project: the repository's name, such as `shop`."
    });
    let id = json!({ "type": "string", "description": "The task's id." });
    let list = |about: &str| json!({ "type": "array", "items": { "type": "string" }, "description": about });
    let gate = json!({
        "type": "object",
        "description": "A check the work must pass: a shell command, run in the task's working directory after the run is done (exit status 0 passes), how long it may run, and how many runs the task gets to pass it.",
        "properties": {
            "command": { "type": "string", "description": "The command, run with `sh -c`, such as `cargo test`." },
            "timeout_seconds": { "type": "integer", "minimum": 1, "description": "Seconds it may run before it is stopped and counts as failed." },
            "attempts": { "type": "integer", "minimum": 1, "description": "Runs in all, the first included." },
        },
        "required": ["command", "timeout_seconds", "attempts"],
    });
    let tool = |name: &str, description: &str, properties: Value, required: &[&str]| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": { "type": "object", "properties": properties, "required": required },
        })
    };
    vec![
        tool(
            RUNNERS_TOOL,
            "List the user's runners connected now, each with the harnesses and models it runs tasks on.",
            json!({}),
            &[],
        ),
        tool(
            SUBMIT,
            "Make a task, kept on the hub, and return its id at once. One of the user's runners \
             takes it, picks a harness and a model for it, finds the project in its workspace \
             or clones it from `repository`, and works on it on a branch it pushes, after this \
             conversation ends; without `repository`, a runner with no copy asks for it on the \
             task. The hub adds the notes it knows related to the task.",
            json!({
                "project": project,
                "title": { "type": "string", "description": "The task in a few words, one line." },
                "task": { "type": "string", "description": "What to do, self-contained: the run cannot see this conversation." },
                "goal": { "type": "string", "description": "The outcome wanted, in a sentence." },
                "context": { "type": "string", "description": "Background the run needs." },
                "constraints": list("What the run must not do or change."),
                "files": list("Where the work is."),
                "done_when": list("How to tell it is done, such as `cargo test passes`."),
                "after": list("Ids of tasks that must be done first."),
                "repository": { "type": "string", "description": "Where the project lives: the git URL or path a runner clones it from when it has no copy, such as `git@github.com:acme/shop.git`." },
                "repo": { "type": "string", "description": "A folder inside the project, when it holds several repositories." },
                "gate": gate.clone(),
            }),
            &["project", "title", "task"],
        ),
        tool(
            LIST,
            "List the tasks of a project: each one's id, title and state.",
            json!({ "project": project }),
            &["project"],
        ),
        tool(
            SHOW,
            "Show one task: its ticket, state, questions and result.",
            json!({ "project": project, "id": id }),
            &["project", "id"],
        ),
        tool(
            ANSWER,
            "Answer the question a task asked, so that it goes on.",
            json!({ "project": project, "id": id, "text": { "type": "string", "description": "The answer." } }),
            &["project", "id", "text"],
        ),
        tool(
            GATE,
            "Give a task that is not running the check its work must pass, or, without `gate`, \
             take its check away.",
            json!({ "project": project, "id": id, "gate": gate }),
            &["project", "id"],
        ),
        tool(
            CANCEL,
            "Cancel a task; a runner working on it stops.",
            json!({ "project": project, "id": id }),
            &["project", "id"],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use crate::board::TaskService;
    use crate::server::testing::{Running, start, start_with_tasks};
    use serde_json::{Value, json};
    use std::sync::Mutex;

    /// One MCP message: the status, the answer and the headers asked for.
    fn post(hub: &Running, token: Option<&str>, message: Value) -> (u16, Value) {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let url = format!("{}mcp", hub.url.replace("ws://", "http://"));
        let mut req = agent
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(t) = token {
            req = req.header("authorization", &format!("Bearer {t}"));
        }
        let mut resp = req.send(message.to_string()).unwrap();
        let status = resp.status().as_u16();
        if status == 401 {
            assert_eq!(
                resp.headers()
                    .get("www-authenticate")
                    .map(|v| v.to_str().unwrap()),
                Some("Bearer")
            );
        }
        let text = resp.body_mut().read_to_string().unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    fn rpc(hub: &Running, token: &str, method: &str, params: Value) -> Value {
        let (status, answer) = post(
            hub,
            Some(token),
            json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}),
        );
        assert_eq!(status, 200, "{answer}");
        assert_eq!(answer["id"], 7);
        answer
    }

    fn tool_names(hub: &Running, token: &str) -> Vec<String> {
        rpc(hub, token, "tools/list", json!({}))["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// A tool's text, and whether it failed.
    fn tool(hub: &Running, token: &str, name: &str, arguments: Value) -> (String, bool) {
        let result = &rpc(
            hub,
            token,
            "tools/call",
            json!({"name": name, "arguments": arguments}),
        )["result"];
        (
            result["content"][0]["text"].as_str().unwrap().to_string(),
            result["isError"].as_bool().unwrap(),
        )
    }

    #[test]
    fn a_client_agent_with_a_token_shares_and_recalls_as_its_account() {
        let hub = start();
        let (status, _) = post(
            &hub,
            None,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        );
        assert_eq!(status, 401, "no token");
        assert_eq!(
            post(
                &hub,
                Some("not-a-token"),
                json!({"id": 1, "method": "ping"})
            )
            .0,
            401
        );

        let init = rpc(
            &hub,
            &hub.ana,
            "initialize",
            json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "claude-code"}}),
        );
        assert_eq!(
            init["result"]["protocolVersion"], "2025-03-26",
            "the client's own"
        );
        let instructions = init["result"]["instructions"].as_str().unwrap();
        assert!(instructions.contains("hub of Acme"), "{instructions}");
        assert!(instructions.contains("`recall`"), "{instructions}");
        let (status, answer) = post(
            &hub,
            Some(&hub.ana),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        );
        assert_eq!(
            (status, answer),
            (202, Value::Null),
            "a notification gets no answer"
        );

        // A hub that keeps no tasks offers no task tools.
        assert_eq!(tool_names(&hub, &hub.ana), ["share", "recall"]);
        assert_eq!(
            tool_names(&hub, &hub.rex),
            ["recall"],
            "a reader recalls only"
        );
        let (text, failed) = tool(&hub, &hub.rex, "share", json!({"title": "t", "text": "x"}));
        assert!(failed && text.contains("no tool `share`"), "{text}");

        let (text, failed) = tool(&hub, &hub.ana, "recall", json!({"query": "deploy day"}));
        assert!(!failed && text.contains("Tuesdays"), "{text}");
        let (text, failed) = tool(
            &hub,
            &hub.ana,
            "share",
            json!({"title": "Staging", "text": "Staging runs on the old cluster until March."}),
        );
        assert!(!failed && text.starts_with("Shared as "), "{text}");
        assert!(
            text.contains("only you"),
            "private unless it says hub: {text}"
        );
        let shares = hub.tmp.0.join("knowledge/vault").join("shares");
        let kept: Vec<String> = walk(&shares)
            .into_iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect();
        assert!(
            kept.iter()
                .any(|s| s.contains("ana") && s.contains("old cluster")),
            "kept as ana's: {kept:?}"
        );

        let unknown = rpc(&hub, &hub.ana, "resources/list", json!({}));
        assert_eq!(unknown["error"]["code"], -32601);
    }

    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
        out
    }

    #[test]
    fn mcp_is_a_post_and_nothing_else() {
        let hub = start();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let url = format!("{}mcp", hub.url.replace("ws://", "http://"));
        let resp = agent
            .get(&url)
            .header("authorization", &format!("Bearer {}", hub.ana))
            .call()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 405);
        assert_eq!(
            resp.headers().get("allow").map(|v| v.to_str().unwrap()),
            Some("POST")
        );
    }

    /// Tasks as a hub keeps them, as far as these tests look.
    #[derive(Default)]
    struct Tasks {
        asked: Mutex<Vec<(String, String, Value)>>,
    }

    impl TaskService for Tasks {
        fn handle(
            &self,
            asker: &crate::board::Asker<'_>,
            method: &str,
            params: &Value,
        ) -> Result<Value, String> {
            let user = asker.user;
            self.asked
                .lock()
                .unwrap()
                .push((user.into(), method.into(), params.clone()));
            Ok(match method {
                "task_create" => json!({"id": "t-1"}),
                "tasks" => json!({"tasks": [{"id": "t-1", "title": "Fix the cart"}]}),
                "task" => json!({"task": null}),
                "task_cancel" => json!({"cancel": "before"}),
                _ => json!({}),
            })
        }
    }

    #[test]
    fn a_client_agent_makes_and_follows_tasks_of_its_account() {
        let tasks: &'static Tasks = Box::leak(Box::default());
        let hub = start_with_tasks(tasks);
        assert_eq!(
            tool_names(&hub, &hub.ana),
            [
                "share", "recall", "runners", "submit", "tasks", "task", "answer", "gate", "cancel"
            ]
        );
        assert_eq!(
            tool_names(&hub, &hub.rex),
            ["recall"],
            "no `tasks` permission"
        );

        let (text, failed) = tool(
            &hub,
            &hub.ana,
            "submit",
            json!({"project": "shop", "title": "Fix"}),
        );
        assert!(failed && text.contains("`task` are required"), "{text}");
        let (text, failed) = tool(
            &hub,
            &hub.ana,
            "submit",
            json!({
                "project": "shop", "title": "Fix the cart", "task": "The cart loses items.",
                "done_when": ["cargo test passes"],
            }),
        );
        assert!(!failed, "{text}");
        assert_eq!(text, "Made task t-1 in shop.");
        let (user, method, params) = tasks.asked.lock().unwrap()[0].clone();
        assert_eq!((user.as_str(), method.as_str()), ("ana", "task_create"));
        assert_eq!(params["project"], "shop");
        let ticket = &params["ticket"];
        assert_eq!(ticket["text"], "The cart loses items.");
        assert_eq!(ticket["done_when"], json!(["cargo test passes"]));
        assert_eq!(ticket["constraints"], json!([]));
        assert_eq!(ticket["goal"], Value::Null);
        assert_eq!(ticket["gate"], Value::Null);
        assert_eq!(ticket["repository"], Value::Null);

        // Where the project lives, and the gate its work must pass.
        let (text, failed) = tool(
            &hub,
            &hub.ana,
            "submit",
            json!({
                "project": "shop", "title": "Fix the cart", "task": "The cart loses items.",
                "repository": "git@example.com:acme/shop.git",
                "gate": {"command": "cargo test", "timeout_seconds": 600, "attempts": 3},
                "notes": "mine",
            }),
        );
        assert!(!failed, "{text}");
        let ticket = tasks.asked.lock().unwrap()[1].2["ticket"].clone();
        assert_eq!(ticket["repository"], "git@example.com:acme/shop.git");
        assert_eq!(
            ticket["gate"],
            json!({"command": "cargo test", "timeout_seconds": 600, "attempts": 3})
        );
        assert_eq!(ticket["notes"], Value::Null, "the hub's to set");
        let said = "`gate` is {command, timeout_seconds, attempts}";
        for gate in [
            json!({"command": "cargo test", "attempts": 2}),
            json!({"command": " ", "timeout_seconds": 60, "attempts": 2}),
            json!({"command": "cargo test", "timeout_seconds": 60, "attempts": 0}),
            json!("cargo test"),
            json!({"command": "make", "timeout_seconds": 60, "attempts": 1, "name": "test"}),
        ] {
            let (text, failed) = tool(
                &hub,
                &hub.ana,
                "submit",
                json!({"project": "shop", "title": "Fix", "task": "t", "gate": gate}),
            );
            assert!(failed && text.contains(said), "{gate}: {text}");
        }
        let (text, failed) = tool(
            &hub,
            &hub.ana,
            "submit",
            json!({"project": "shop", "title": "Fix", "task": "t", "repository": "--upload-pack=x"}),
        );
        assert!(failed && text.contains("not a git URL"), "{text}");
        assert_eq!(
            tasks.asked.lock().unwrap().len(),
            2,
            "none of these was made"
        );

        let (text, _) = tool(&hub, &hub.ana, "tasks", json!({"project": "shop"}));
        assert!(text.contains("Fix the cart"), "{text}");
        let (text, failed) = tool(
            &hub,
            &hub.ana,
            "task",
            json!({"project": "shop", "id": "t-9"}),
        );
        assert!(failed && text.contains("no task 't-9'"), "{text}");
        let (text, _) = tool(
            &hub,
            &hub.ana,
            "cancel",
            json!({"project": "shop", "id": "t-1"}),
        );
        assert_eq!(text, "Cancelled task t-1 before it started.");
        let gate = json!({"command": "make test", "timeout_seconds": 60, "attempts": 2});
        let (text, failed) = tool(
            &hub,
            &hub.ana,
            "gate",
            json!({"project": "shop", "id": "t-1", "gate": gate}),
        );
        assert_eq!(
            (text.as_str(), failed),
            ("Task t-1 must now pass `make test`.", false)
        );
        let (text, _) = tool(
            &hub,
            &hub.ana,
            "gate",
            json!({"project": "shop", "id": "t-1"}),
        );
        assert_eq!(text, "Task t-1 has no gate now.");
        let asked = tasks.asked.lock().unwrap().clone();
        let gated: Vec<&Value> = asked
            .iter()
            .filter(|(_, method, _)| method == "task_gate")
            .map(|(_, _, params)| &params["gate"])
            .collect();
        assert_eq!(gated, [&gate, &Value::Null]);
        let (text, _) = tool(&hub, &hub.ana, "runners", json!({}));
        assert!(text.starts_with("No runner that works for you"), "{text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn runners_lists_the_accounts_runners_with_what_they_run_on() {
        use ensemble_link::client::Connection;
        use ensemble_link::protocol::RUNNER;
        let tasks: &'static Tasks = Box::leak(Box::default());
        let hub = start_with_tasks(tasks);
        let mut desk = Connection::open(&hub.url, &hub.ana).await.unwrap();
        desk.call(
            RUNNER,
            json!({"name": "desk", "harnesses": [
                {"harness": "claude-code", "models": [{"model": "haiku", "tier": "medium"}, {"model": "sonnet", "tier": "strong"}]},
                {"harness": "builtin", "models": [{"model": "local:qwen3.5:9b", "tier": "medium"}]},
            ]}),
        )
        .await
        .unwrap();
        let mut laptop = Connection::open(&hub.url, &hub.ana).await.unwrap();
        laptop
            .call(RUNNER, json!({"name": "laptop", "harnesses": []}))
            .await
            .unwrap();
        // root's works for root alone.
        let mut gpu = Connection::open(&hub.url, &hub.root).await.unwrap();
        gpu.call(
            RUNNER,
            json!({"name": "gpu", "harnesses": [{"harness": "codex", "models": [{"model": "gpt-5", "tier": "strong"}]}]}),
        )
        .await
        .unwrap();
        let (text, failed) =
            ensemble_core::runtime::blocking(|| tool(&hub, &hub.ana, "runners", json!({})));
        assert!(!failed, "{text}");
        assert_eq!(
            text,
            "- ana/desk: claude-code on haiku (medium), sonnet (strong); builtin on local:qwen3.5:9b (medium)\n\
             - ana/laptop: runs no task (no harness is on with a model it can drive)"
        );
    }
}
