//! The harness interface: MCP over stdio.
//!
//! WHY MCP, AND NOT A PLUGIN OR A PROVIDER
//! ---------------------------------------
//! Three extension points were probed against OpenCode 2.0.32 (see
//! `docs/HARNESS.md` for the evidence):
//!
//! * A **plugin-registered tool** registers and is hookable, but is never put
//!   in the tool array the model is offered — the model calling it is told
//!   `No tool named "..." is currently available`. Unusable as the surface for
//!   a tool the manager must call.
//! * A **provider module** named by `providers.<id>.package` is imported and
//!   its `model(modelID, settings)` is called, but the returned object is then
//!   validated against an internal, unpublished schema that a plain AI SDK
//!   model does not satisfy. Guessing at it would invent a capability and break
//!   on the next release.
//! * An **MCP server** declared as `mcp.<name> = {type:"local", command:[...]}`
//!   connects, and its tools become reachable by the model. This is the one
//!   that works, and it is also the integration every other MCP-speaking
//!   harness already supports.
//!
//! So Ensemble is a subprocess the harness spawns, speaking JSON-RPC 2.0 as
//! newline-delimited JSON on stdin/stdout. **Nothing but protocol goes to
//! stdout** — one stray `println!` corrupts the stream and the harness sees a
//! parse error instead of a tool. Diagnostics go to stderr, which the harness
//! logs.
//!
//! WHAT THE TOOLS DELIBERATELY DO NOT EXPOSE
//! -----------------------------------------
//! No tool takes a backend or a model. The caller names a *role* and Ensemble
//! decides what runs it — that is the whole point of the indirection, and a
//! `model` parameter would quietly undo it at the first opportunity.

use crate::router::{Ensemble, RouteError};
use serde_json::{Value, json};
use std::io::{BufRead, Write};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const SERVER_NAME: &str = "ensemble";

/// The tool catalogue, as MCP describes it.
pub fn tool_definitions(roles: &[&str]) -> Value {
    // Listing the real role names in the description and the enum is what makes
    // the first delegation land: without it the model guesses "architect" and
    // spends a round trip finding out.
    let role_list = roles.join(", ");
    json!([
        {
            "name": "ensemble_delegate",
            "description": format!(
                "Delegate a self-contained task to a member of the ensemble and return its \
                 work. Available roles: {role_list}. Ensemble selects the model and backend \
                 for the role — you do not choose one. Give the role everything it needs: \
                 each call is stateless and the role cannot see this conversation."
            ),
            "inputSchema": {
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
            }
        },
        {
            "name": "ensemble_roles",
            "description":
                "List the roles this ensemble can delegate to, and what each is for. \
                 Call this first if you are unsure who should do a piece of work.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "ensemble_health",
            "description":
                "Check that every backend the ensemble routes to is reachable. Spends no \
                 model tokens. Use it when a delegation fails, to tell a configuration \
                 problem from a bad answer.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }
    ])
}

/// The result of a tool call: text, plus whether it is an error.
///
/// A refused or failed call comes back as a *result* with `isError: true`, not
/// as a JSON-RPC error. The model has to read why it failed in order to do the
/// right thing next; a transport-level error is handled by the harness and the
/// model never sees the reason.
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
}

impl ToolResult {
    fn ok(text: impl Into<String>) -> Self {
        ToolResult {
            text: text.into(),
            is_error: false,
        }
    }
    fn err(text: impl Into<String>) -> Self {
        ToolResult {
            text: text.into(),
            is_error: true,
        }
    }
}

pub fn call_tool(ensemble: &Ensemble, name: &str, args: &Value) -> ToolResult {
    match name {
        "ensemble_delegate" => {
            let Some(role) = args.get("role").and_then(Value::as_str) else {
                return ToolResult::err(
                    "ensemble_delegate: `role` is required and must be a string",
                );
            };
            let Some(task) = args.get("task").and_then(Value::as_str) else {
                return ToolResult::err(
                    "ensemble_delegate: `task` is required and must be a string",
                );
            };
            if task.trim().is_empty() {
                return ToolResult::err(
                    "ensemble_delegate: `task` is empty — there is nothing to do",
                );
            }
            let context = args.get("context").and_then(Value::as_str);

            match ensemble.delegate(role, task, context) {
                Ok(out) => ToolResult::ok(out.text),
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
                        RouteError::Backend(_) => {
                            format!(" (role '{role}')")
                        }
                    };
                    ToolResult::err(format!("delegation failed{detail}: {e}{hint}"))
                }
            }
        }

        "ensemble_roles" => {
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

        "ensemble_health" => {
            let results = ensemble.health();
            let mut lines = Vec::new();
            let mut any_bad = false;
            for (backend, result) in results {
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
             ensemble_delegate, ensemble_roles, ensemble_health."
        )),
    }
}

/// A role prompt is a paragraph; a tool listing wants a line.
fn first_line(prompt: &str) -> String {
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

/// Handle one JSON-RPC message.
///
/// `None` means "send nothing" — which is required, not merely allowed, for
/// notifications: a notification carries no id, and answering one is a protocol
/// violation that some clients treat as fatal.
pub fn handle(ensemble: &Ensemble, request: &Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");

    let reply = |result: Value| Some(json!({"jsonrpc": "2.0", "id": id, "result": result}));

    match method {
        "initialize" => {
            // Echo the client's protocol version when it sends one. Insisting
            // on ours would refuse clients that are otherwise compatible.
            let version = request
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION);
            reply(json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") }
            }))
        }

        "tools/list" => reply(json!({ "tools": tool_definitions(&ensemble.roles()) })),

        "tools/call" => {
            let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let result = call_tool(ensemble, name, &args);
            reply(json!({
                "content": [{ "type": "text", "text": result.text }],
                "isError": result.is_error
            }))
        }

        "ping" => reply(json!({})),

        // Notifications: no id, no answer.
        m if m.starts_with("notifications/") => None,

        other => {
            if id.is_none() || id == Some(Value::Null) {
                return None;
            }
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {other}") }
            }))
        }
    }
}

/// Run the server until stdin closes.
pub fn serve(
    ensemble: &Ensemble,
    input: impl BufRead,
    mut output: impl Write,
) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(request) => handle(ensemble, &request),
            Err(e) => Some(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32700, "message": format!("parse error: {e}") }
            })),
        };
        if let Some(response) = response {
            writeln!(output, "{response}")?;
            // Flush per message. A buffered reply is a reply the harness is
            // still waiting for, and it looks exactly like a hung server.
            output.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::port::ModelPort;
    use crate::port::testing::FakePort;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    const YAML: &str = r#"
version: 1
backends:
  local: { type: openai-compatible-api, base_url_env: L, base_url_default: "http://127.0.0.1:8000/v1" }
  claude: { type: claude-code-cli }
manager:
  backend: local
  model: qwen3-coder
  prompt: "Coordinates the team and delegates work."
roles:
  coder:
    backend: local
    model: qwen3-coder
    prompt: "Writes code to a stated spec."
  reviewer:
    backend: claude
    model: claude-sonnet
"#;

    fn ensemble_with(local: Arc<FakePort>, claude: Arc<FakePort>) -> Ensemble {
        let cfg = config::parse(
            YAML,
            std::path::Path::new("ensemble.yaml"),
            &BTreeMap::new(),
        )
        .unwrap();
        let mut ports: BTreeMap<String, Arc<dyn ModelPort>> = BTreeMap::new();
        ports.insert("local".into(), local);
        ports.insert("claude".into(), claude);
        Ensemble::with_ports(cfg, ports, None)
    }

    fn working() -> Ensemble {
        ensemble_with(
            Arc::new(FakePort::ok("local", "work done")),
            Arc::new(FakePort::ok("claude", "reviewed")),
        )
    }

    fn rpc(method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
    }

    fn call(e: &Ensemble, name: &str, args: Value) -> Value {
        handle(
            e,
            &rpc("tools/call", json!({"name": name, "arguments": args})),
        )
        .unwrap()
    }

    fn text_of(response: &Value) -> &str {
        response["result"]["content"][0]["text"].as_str().unwrap()
    }

    #[test]
    fn initialize_reports_the_server_and_echoes_the_clients_protocol_version() {
        let r = handle(
            &working(),
            &rpc("initialize", json!({"protocolVersion": "2024-11-05"})),
        )
        .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(r["result"]["serverInfo"]["name"], "ensemble");
        assert!(r["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn initialize_falls_back_to_our_version_when_the_client_sends_none() {
        let r = handle(&working(), &rpc("initialize", json!({}))).unwrap();
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSION);
    }

    #[test]
    fn tools_list_advertises_the_three_tools_with_the_real_role_names() {
        let r = handle(&working(), &rpc("tools/list", json!({}))).unwrap();
        let tools = r["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec!["ensemble_delegate", "ensemble_roles", "ensemble_health"]
        );

        let enum_ = &tools[0]["inputSchema"]["properties"]["role"]["enum"];
        let roles: Vec<&str> = enum_
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(roles, vec!["manager", "coder", "reviewer"]);
    }

    #[test]
    fn no_tool_accepts_a_model_or_a_backend_parameter() {
        // The indirection is the product. A `model` parameter would undo it at
        // the first opportunity, so its absence is asserted rather than assumed.
        let r = handle(&working(), &rpc("tools/list", json!({}))).unwrap();
        let schema = r["result"]["tools"].to_string();
        assert!(!schema.contains("\"model\""), "{schema}");
        assert!(!schema.contains("\"backend\""), "{schema}");
    }

    #[test]
    fn delegate_returns_the_roles_work_and_routes_to_its_backend() {
        let local = Arc::new(FakePort::ok("local", "work done"));
        let claude = Arc::new(FakePort::ok("claude", "reviewed"));
        let e = ensemble_with(local.clone(), claude.clone());

        let r = call(
            &e,
            "ensemble_delegate",
            json!({"role": "coder", "task": "write a parser"}),
        );
        assert_eq!(text_of(&r), "work done");
        assert_eq!(r["result"]["isError"], false);
        assert_eq!(local.call_count(), 1);
        assert_eq!(claude.call_count(), 0);

        let r = call(
            &e,
            "ensemble_delegate",
            json!({"role": "reviewer", "task": "review it"}),
        );
        assert_eq!(text_of(&r), "reviewed");
        assert_eq!(claude.call_count(), 1);
    }

    #[test]
    fn delegate_passes_context_through_to_the_backend() {
        let local = Arc::new(FakePort::ok("local", "ok"));
        let e = ensemble_with(local.clone(), Arc::new(FakePort::ok("claude", "ok")));
        call(
            &e,
            "ensemble_delegate",
            json!({"role": "coder", "task": "fix it", "context": "the test fails on line 9"}),
        );
        let sent = local.last().unwrap();
        assert!(
            sent.messages.iter().any(|m| m.content.contains("line 9")),
            "{:?}",
            sent.messages
        );
    }

    #[test]
    fn a_failed_delegation_is_an_is_error_result_not_a_jsonrpc_error() {
        // The model must be able to read the reason. A JSON-RPC error is
        // handled by the harness and never reaches it.
        let e = ensemble_with(
            Arc::new(FakePort::failing("local", "Connection refused")),
            Arc::new(FakePort::ok("claude", "ok")),
        );
        let r = call(
            &e,
            "ensemble_delegate",
            json!({"role": "coder", "task": "t"}),
        );
        assert!(r.get("error").is_none(), "{r}");
        assert_eq!(r["result"]["isError"], true);
        let text = text_of(&r);
        assert!(text.contains("Connection refused"), "{text}");
        assert!(
            text.contains("worth one retry"),
            "says whether to retry: {text}"
        );
    }

    #[test]
    fn a_permanent_failure_tells_the_model_not_to_loop() {
        let e = working();
        let r = call(
            &e,
            "ensemble_delegate",
            json!({"role": "architect", "task": "t"}),
        );
        assert_eq!(r["result"]["isError"], true);
        let text = text_of(&r);
        assert!(text.contains("architect"), "{text}");
        assert!(text.contains("coder"), "lists the real roles: {text}");
        assert!(text.contains("fail the same way"), "{text}");
    }

    #[test]
    fn missing_or_empty_arguments_are_refused_with_a_usable_message() {
        let e = working();
        for args in [
            json!({"task": "t"}),
            json!({"role": "coder"}),
            json!({"role": "coder", "task": "   "}),
        ] {
            let r = call(&e, "ensemble_delegate", args.clone());
            assert_eq!(r["result"]["isError"], true, "{args}");
        }
    }

    #[test]
    fn roles_lists_the_team_without_naming_backends_or_models() {
        let r = call(&working(), "ensemble_roles", json!({}));
        let text = text_of(&r);
        assert!(text.contains("manager"), "{text}");
        assert!(text.contains("Writes code to a stated spec."), "{text}");
        assert!(
            !text.contains("qwen3-coder"),
            "must not leak the model: {text}"
        );
        assert!(
            !text.contains("claude-sonnet"),
            "must not leak the model: {text}"
        );
    }

    #[test]
    fn health_reports_each_backend_and_flags_a_broken_one() {
        let good = call(&working(), "ensemble_health", json!({}));
        assert_eq!(good["result"]["isError"], false);
        assert!(text_of(&good).contains("local: ok"), "{}", text_of(&good));

        // FakePort::health always succeeds, so a genuinely missing adapter is
        // the available way to produce an unhealthy backend here.
        let cfg = config::parse(
            YAML,
            std::path::Path::new("ensemble.yaml"),
            &BTreeMap::new(),
        )
        .unwrap();
        let mut ports: BTreeMap<String, Arc<dyn ModelPort>> = BTreeMap::new();
        ports.insert("local".into(), Arc::new(FakePort::ok("local", "ok")));
        let e = Ensemble::with_ports(cfg, ports, None);
        let bad = call(&e, "ensemble_health", json!({}));
        assert_eq!(bad["result"]["isError"], true);
        assert!(text_of(&bad).contains("UNREACHABLE"), "{}", text_of(&bad));
    }

    #[test]
    fn an_unknown_tool_names_the_ones_that_exist() {
        let r = call(&working(), "ensemble_do_everything", json!({}));
        assert_eq!(r["result"]["isError"], true);
        assert!(text_of(&r).contains("ensemble_delegate"), "{}", text_of(&r));
    }

    #[test]
    fn notifications_are_never_answered() {
        // Answering one is a protocol violation some clients treat as fatal.
        assert!(
            handle(
                &working(),
                &json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )
            .is_none()
        );
        assert!(
            handle(
                &working(),
                &json!({"jsonrpc":"2.0","method":"notifications/cancelled"})
            )
            .is_none()
        );
    }

    #[test]
    fn an_unknown_method_with_an_id_gets_method_not_found() {
        let r = handle(&working(), &rpc("resources/list", json!({}))).unwrap();
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn ping_is_answered_so_liveness_checks_pass() {
        let r = handle(&working(), &rpc("ping", json!({}))).unwrap();
        assert!(r["result"].is_object());
    }

    #[test]
    fn the_serve_loop_writes_one_json_object_per_line_and_skips_notifications() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            "\n",
        );
        let mut out = Vec::new();
        serve(&working(), std::io::Cursor::new(input), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "one reply each for initialize and tools/list: {text}"
        );
        for line in lines {
            let v: Value = serde_json::from_str(line).expect("each line is one JSON object");
            assert_eq!(v["jsonrpc"], "2.0");
        }
    }

    #[test]
    fn a_malformed_line_is_a_parse_error_and_does_not_end_the_session() {
        let input = "{not json\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n";
        let mut out = Vec::new();
        serve(&working(), std::io::Cursor::new(input), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["error"]["code"], -32700);
        // And the session carried on.
        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["id"], 2);
    }
}
