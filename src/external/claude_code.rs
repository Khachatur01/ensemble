//! The `claude-code` adapter: the Claude Code CLI as a member of the team.
//!
//! WHAT THIS IS, AND WHAT IT DELIBERATELY IS NOT
//! ----------------------------------------------
//! It drives the official `claude` CLI in its supported headless mode
//! (`claude -p`), as a subprocess, started locally by the person who owns the
//! subscription. It does NOT read, copy, replay or forge Claude Code's
//! credentials and it never speaks to Anthropic's API itself. There is no token
//! extraction here and none should be added: a shim that replays subscription
//! credentials to serve a general-purpose API is what the terms prohibit, and it
//! is the reason this adapter is a subprocess rather than an HTTP client.
//!
//! It never passes `--bare`, which bypasses the saved login and bills per token.
//!
//! Claude Code is a whole agent: it runs its own loop with its own tools in the
//! project directory. Ensemble starts it with the task, the tools it may use
//! (`--allowedTools`; with `--permission-mode dontAsk` anything else is denied),
//! and reads its `stream-json` output as it goes, which is where progress, the
//! commands it ran and its final answer come from.

use super::AgentRun;
use super::process;
use crate::config::{AgentConfig, CLAUDE_CODE_AGENT, ClaudeCodeAdapter, claude_tool_writes};
use crate::port::{Error, Usage};
use crate::workspace::Changes;
use crate::workspace::{self, Workspace};
use serde_json::Value;
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// Run a `claude-code` agent on one task, in `workdir`.
pub fn run(
    agent: &AgentConfig,
    cc: &ClaudeCodeAdapter,
    task: &str,
    context: Option<&str>,
    workdir: &Path,
    progress: &dyn Fn(&str),
    cancel: &AtomicBool,
) -> Result<AgentRun, Error> {
    run_program(
        std::slice::from_ref(&cc.command),
        agent,
        cc,
        task,
        context,
        workdir,
        progress,
        cancel,
    )
}

/// The command line, less the program. One argument per tool: the CLI takes
/// them space- or comma-separated, and a rule such as `Bash(git log:*)` has a
/// space in it.
pub fn arguments(agent: &AgentConfig, cc: &ClaudeCodeAdapter, prompt: &str) -> Vec<String> {
    let mut v: Vec<String> = vec![
        "-p".into(),
        prompt.into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--model".into(),
        cc.model.clone(),
        "--permission-mode".into(),
        cc.permission_mode.clone(),
        "--max-turns".into(),
        cc.max_turns.to_string(),
    ];
    if !cc.tools.is_empty() {
        v.push("--allowedTools".into());
        v.push(cc.tools.join(","));
    }
    if let Some(system) = agent.prompt.as_deref().filter(|p| !p.trim().is_empty()) {
        v.push("--append-system-prompt".into());
        v.push(system.into());
    }
    v
}

/// [`run`], with the program given as an argv prefix, so tests can stand in a
/// script for `claude`.
#[allow(clippy::too_many_arguments)]
fn run_program(
    program: &[String],
    agent: &AgentConfig,
    cc: &ClaudeCodeAdapter,
    task: &str,
    context: Option<&str>,
    workdir: &Path,
    progress: &dyn Fn(&str),
    cancel: &AtomicBool,
) -> Result<AgentRun, Error> {
    let backend = CLAUDE_CODE_AGENT.to_string();
    let workspace = Workspace::new(workdir).map_err(|detail| Error::Unsupported {
        backend: backend.clone(),
        detail: format!("agent '{}' cannot start: {detail}", agent.name),
    })?;
    let say = |what: &str| progress(&format!("{} · {what}", agent.name));

    let prompt = match context.filter(|c| !c.trim().is_empty()) {
        Some(ctx) => format!("Context:\n{ctx}\n\n{task}"),
        None => task.to_string(),
    };
    // Claude Code's own edits are not visible from here one by one, so what
    // it changed is found by comparing the project before and after.
    let baseline = cc
        .tools
        .iter()
        .any(|t| claude_tool_writes(t))
        .then(|| workspace::snapshot(&workspace))
        .flatten();

    say("starting Claude Code");
    let mut stream = Stream::new(&workspace, &say);
    let ended = process::Run {
        backend: CLAUDE_CODE_AGENT,
        program,
        args: arguments(agent, cc, &prompt),
        env: Vec::new(),
        workdir: workspace.root(),
        max_seconds: agent.max_seconds,
        cancel,
    }
    .stream(&mut |line| stream.read(line))?;
    let (status, stderr, stopped) = (ended.status, ended.stderr, ended.stopped);

    let mut out = AgentRun {
        text: String::new(),
        model: stream.model.clone().unwrap_or_else(|| cc.model.clone()),
        usage: Usage::default(),
        steps: stream.turns,
        tool_calls: stream.tool_calls,
        changes: Changes::new(),
        commands: std::mem::take(&mut stream.commands),
    };
    if let Some(before) = &baseline
        && let Some(after) = workspace::snapshot(&workspace)
    {
        workspace::diff(before, &after, &mut out.changes);
    }

    if let Some(why) = stopped {
        say(&why);
        out.text = format!(
            "[{why} after {} turn(s); the work is unfinished]",
            out.steps
        );
        if !stream.last_text.trim().is_empty() {
            out.text.push('\n');
            out.text.push_str(&stream.last_text);
        }
        return Ok(out);
    }

    let Some(result) = stream.result.take() else {
        return Err(Error::ProcessFailed {
            backend,
            code: status,
            stderr: if stderr.trim().is_empty() {
                "it ended without a result".into()
            } else {
                stderr
            },
        });
    };
    finish(&mut out, &result, cc, &stream.last_text, status, &stderr)?;
    say("done");
    Ok(out)
}

/// Fill `out` from the stream's closing `result` event.
fn finish(
    out: &mut AgentRun,
    result: &Value,
    cc: &ClaudeCodeAdapter,
    last_text: &str,
    status: Option<i32>,
    stderr: &str,
) -> Result<(), Error> {
    let backend = CLAUDE_CODE_AGENT.to_string();
    let used: Vec<&str> = result
        .get("modelUsage")
        .and_then(Value::as_object)
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default();
    // A stray ANTHROPIC_BASE_URL can quietly send the work elsewhere; if
    // something that is not Claude answered, that is not what was asked for.
    if !used.is_empty() && !used.iter().any(|m| m.contains("claude")) {
        return Err(Error::Protocol {
            backend,
            detail: format!(
                "a non-Claude model ('{}') answered — refusing. Check ANTHROPIC_BASE_URL.",
                used.join(", ")
            ),
        });
    }
    if let Some(model) = used.first() {
        out.model = model.to_string();
    }
    let usage = result.get("usage");
    let num = |k: &str| {
        usage
            .and_then(|u| u.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32
    };
    out.usage = Usage {
        prompt_tokens: num("input_tokens")
            + num("cache_creation_input_tokens")
            + num("cache_read_input_tokens"),
        completion_tokens: num("output_tokens"),
    };
    if let Some(n) = result.get("num_turns").and_then(Value::as_u64) {
        out.steps = n as u32;
    }

    let subtype = result.get("subtype").and_then(Value::as_str).unwrap_or("");
    let text = result.get("result").and_then(Value::as_str);
    let is_error = result
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    out.text = match (subtype, text) {
        ("error_max_turns", _) => {
            format!("[stopped at the {}-turn limit]\n{last_text}", cc.max_turns)
        }
        (_, Some(t)) if !is_error && status == Some(0) => t.to_string(),
        (_, t) => {
            return Err(Error::ProcessFailed {
                backend,
                code: status,
                stderr: t
                    .map(str::to_string)
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or_else(|| format!("{subtype} {stderr}")),
            });
        }
    };

    let mut denied: Vec<&str> = result
        .get("permission_denials")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|d| d.get("tool_name").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    denied.sort_unstable();
    denied.dedup();
    if !denied.is_empty() {
        out.text.push_str(&format!(
            "\n\n[Claude Code was denied: {}. Add them to the adapter's tools to allow them.]",
            denied.join(", ")
        ));
    }
    Ok(())
}

/// What the `stream-json` events have said so far.
struct Stream<'a> {
    workspace: &'a Workspace,
    say: &'a dyn Fn(&str),
    model: Option<String>,
    turns: u32,
    tool_calls: u32,
    commands: Vec<String>,
    /// `Bash` calls waiting for their result, by tool-use id. A command
    /// counts as run only once its result comes back without an error: a
    /// denied one never ran.
    pending: std::collections::HashMap<String, String>,
    last_text: String,
    result: Option<Value>,
}

impl<'a> Stream<'a> {
    fn new(workspace: &'a Workspace, say: &'a dyn Fn(&str)) -> Self {
        Stream {
            workspace,
            say,
            model: None,
            turns: 0,
            tool_calls: 0,
            commands: Vec::new(),
            pending: std::collections::HashMap::new(),
            last_text: String::new(),
            result: None,
        }
    }

    /// One line of output. Anything that is not a JSON event (a warning the
    /// CLI printed) is skipped.
    fn read(&mut self, line: &str) {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("system") if event.get("subtype").and_then(Value::as_str) == Some("init") => {
                self.model = event
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            Some("assistant") => self.assistant(&event),
            Some("user") => self.tool_results(&event),
            Some("result") => self.result = Some(event),
            _ => {}
        }
    }

    fn assistant(&mut self, event: &Value) {
        let Some(content) = event.pointer("/message/content").and_then(Value::as_array) else {
            return;
        };
        let calls: Vec<&Value> = content
            .iter()
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("tool_use"))
            .collect();
        for c in content {
            if c.get("type").and_then(Value::as_str) == Some("text")
                && let Some(t) = c.get("text").and_then(Value::as_str)
                && !t.trim().is_empty()
            {
                self.last_text = t.to_string();
            }
        }
        if calls.is_empty() {
            return;
        }
        self.turns += 1;
        for call in calls {
            self.tool_calls += 1;
            let name = call.get("name").and_then(Value::as_str).unwrap_or("tool");
            let input = call.get("input").cloned().unwrap_or(Value::Null);
            let shown = self.describe(&input);
            if name == "Bash"
                && let Some(id) = call.get("id").and_then(Value::as_str)
            {
                self.pending.insert(id.to_string(), shown.clone());
            }
            (self.say)(&format!("turn {} · {name}({shown})", self.turns));
        }
    }

    fn tool_results(&mut self, event: &Value) {
        let Some(content) = event.pointer("/message/content").and_then(Value::as_array) else {
            return;
        };
        for c in content {
            if c.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let failed = c.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            if let Some(id) = c.get("tool_use_id").and_then(Value::as_str)
                && let Some(command) = self.pending.remove(id)
                && !failed
            {
                self.commands.push(command);
            }
        }
    }

    /// A tool's input on one short line: the path for file tools, the command
    /// for `Bash`, the arguments otherwise.
    fn describe(&self, input: &Value) -> String {
        for key in ["file_path", "notebook_path", "path"] {
            if let Some(p) = input.get(key).and_then(Value::as_str) {
                let shown = self.workspace.show(Path::new(p));
                return if shown.is_empty() { ".".into() } else { shown };
            }
        }
        for key in ["command", "pattern", "url", "query"] {
            if let Some(s) = input.get(key).and_then(Value::as_str) {
                return super::brief(s);
            }
        }
        super::brief(&input.to_string())
    }
}

/// Check that the CLI is there and starts. `--version` does not touch the
/// subscription, which is the whole requirement for a health check.
pub fn health(command: &str) -> Result<(), Error> {
    process::health(CLAUDE_CODE_AGENT, command)
}

#[cfg(test)]
mod tests {
    use super::process::testing::{emitting, fake};
    use super::*;
    use crate::config::Adapter;
    use crate::workspace::testing::TempDir;
    use std::time::{Duration, Instant};

    fn agent(tools: &[&str], max_seconds: u32) -> (AgentConfig, ClaudeCodeAdapter) {
        let cc = ClaudeCodeAdapter {
            command: "claude".into(),
            model: "sonnet".into(),
            tools: tools.iter().map(|t| t.to_string()).collect(),
            permission_mode: "dontAsk".into(),
            max_turns: 7,
        };
        let a = AgentConfig {
            name: "reviewer".into(),
            prompt: Some("You review code.".into()),
            max_seconds,
            max_concurrent: 1,
            isolation: crate::config::Isolation::Shared,
            budget: crate::config::Budget::Unlimited,
            adapter: Adapter::ClaudeCode(cc.clone()),
        };
        (a, cc)
    }

    fn tool_use(id: &str, name: &str, input: Value) -> Value {
        serde_json::json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": id, "name": name, "input": input}
        ]}})
    }

    fn tool_result(id: &str, is_error: bool) -> Value {
        serde_json::json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": id, "is_error": is_error, "content": "..."}
        ]}})
    }

    fn result(text: &str) -> Value {
        serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false, "result": text,
            "num_turns": 3,
            "usage": {"input_tokens": 5, "cache_read_input_tokens": 100, "output_tokens": 7},
            "modelUsage": {"claude-sonnet-5-5": {}},
            "permission_denials": []
        })
    }

    fn go(
        program: &[String],
        a: &AgentConfig,
        cc: &ClaudeCodeAdapter,
        dir: &Path,
    ) -> (Result<AgentRun, Error>, Vec<String>) {
        let seen = std::cell::RefCell::new(Vec::new());
        let r = run_program(
            program,
            a,
            cc,
            "review it",
            Some("it is new"),
            dir,
            &|l| seen.borrow_mut().push(l.to_string()),
            &AtomicBool::new(false),
        );
        (r, seen.into_inner())
    }

    #[test]
    fn the_command_line_carries_the_task_tools_model_and_prompt_and_never_bare() {
        let (a, cc) = agent(&["Read", "Bash(git log:*)"], 60);
        let args = arguments(&a, &cc, "do it");
        let at = |flag: &str| {
            let i = args.iter().position(|x| x == flag).unwrap();
            args[i + 1].clone()
        };
        assert_eq!(at("-p"), "do it");
        assert_eq!(at("--output-format"), "stream-json");
        assert_eq!(at("--model"), "sonnet");
        assert_eq!(at("--permission-mode"), "dontAsk");
        assert_eq!(at("--max-turns"), "7");
        assert_eq!(at("--allowedTools"), "Read,Bash(git log:*)");
        assert_eq!(at("--append-system-prompt"), "You review code.");
        assert!(
            args.contains(&"--verbose".to_string()),
            "stream-json needs it"
        );
        // --bare bypasses the subscription login and bills per token.
        assert!(!args.contains(&"--bare".to_string()));
    }

    #[test]
    fn a_run_reports_progress_commands_usage_and_the_answer() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("a.rs"), "x").unwrap();
        let file = tmp.0.canonicalize().unwrap().join("a.rs");
        let script = emitting(&[
            serde_json::json!({"type": "system", "subtype": "init", "model": "claude-sonnet-5-5"}),
            tool_use(
                "t1",
                "Read",
                serde_json::json!({"file_path": file.display().to_string()}),
            ),
            tool_result("t1", false),
            tool_use("t2", "Bash", serde_json::json!({"command": "cargo test"})),
            tool_result("t2", false),
            tool_use("t3", "Bash", serde_json::json!({"command": "rm -rf /"})),
            tool_result("t3", true),
            serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "looks fine"}]}}),
            result("looks fine"),
        ]) + "echo noise >&2\n";
        let (a, cc) = agent(&["Read", "Bash"], 60);
        let (r, seen) = go(&fake(&tmp.0, &script), &a, &cc, &tmp.0);
        let r = r.unwrap();
        assert_eq!(r.text, "looks fine");
        assert_eq!(r.model, "claude-sonnet-5-5");
        assert_eq!((r.steps, r.tool_calls), (3, 3));
        assert_eq!((r.usage.prompt_tokens, r.usage.completion_tokens), (105, 7));
        assert_eq!(r.commands, vec!["cargo test"], "a denied command never ran");
        assert_eq!(
            seen,
            vec![
                "reviewer · starting Claude Code",
                "reviewer · turn 1 · Read(a.rs)",
                "reviewer · turn 2 · Bash(cargo test)",
                "reviewer · turn 3 · Bash(rm -rf /)",
                "reviewer · done",
            ]
        );
    }

    #[test]
    fn files_claude_code_changed_are_listed() {
        let tmp = TempDir::new();
        let script = format!(
            "echo new > made.txt\n{}",
            emitting(&[result("made a file")])
        );
        let (a, cc) = agent(&["Read", "Write"], 60);
        let (r, _) = go(&fake(&tmp.0, &script), &a, &cc, &tmp.0);
        let r = r.unwrap();
        assert_eq!(
            r.changes.get("made.txt"),
            Some(&crate::workspace::Change::Added),
            "{:?}",
            r.changes
        );
    }

    #[test]
    fn denied_tools_and_the_turn_limit_are_reported() {
        let tmp = TempDir::new();
        let mut denied = result("partial");
        denied["permission_denials"] =
            serde_json::json!([{"tool_name": "Edit"}, {"tool_name": "Edit"}]);
        let (a, cc) = agent(&["Read"], 60);
        let (r, _) = go(&fake(&tmp.0, &emitting(&[denied])), &a, &cc, &tmp.0);
        let text = r.unwrap().text;
        assert!(text.contains("[Claude Code was denied: Edit."), "{text}");

        let limit = serde_json::json!({"type": "result", "subtype": "error_max_turns", "is_error": true, "num_turns": 7});
        let said = serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "half done"}]}});
        let (r, _) = go(&fake(&tmp.0, &emitting(&[said, limit])), &a, &cc, &tmp.0);
        let text = r.unwrap().text;
        assert!(
            text.starts_with("[stopped at the 7-turn limit]\nhalf done"),
            "{text}"
        );
    }

    #[test]
    fn a_failing_cli_is_an_error_with_its_reason() {
        let tmp = TempDir::new();
        let (a, cc) = agent(&["Read"], 60);
        let (r, _) = go(
            &fake(&tmp.0, "echo 'credit balance too low' >&2\nexit 1\n"),
            &a,
            &cc,
            &tmp.0,
        );
        let err = r.unwrap_err().to_string();
        assert!(
            err.contains("exited 1") && err.contains("credit balance too low"),
            "{err}"
        );

        let mut failed = result("API Error: 529 overloaded");
        failed["is_error"] = Value::Bool(true);
        let (r, _) = go(
            &fake(&tmp.0, &(emitting(&[failed]) + "exit 1\n")),
            &a,
            &cc,
            &tmp.0,
        );
        let err = r.unwrap_err().to_string();
        assert!(err.contains("529 overloaded"), "{err}");
    }

    #[test]
    fn a_missing_cli_says_what_is_missing() {
        let tmp = TempDir::new();
        let (a, cc) = agent(&["Read"], 60);
        let (r, _) = go(&["/nonexistent/claude".to_string()], &a, &cc, &tmp.0);
        let err = r.unwrap_err().to_string();
        assert!(
            err.contains("could not start") && err.contains("PATH"),
            "{err}"
        );
    }

    #[test]
    fn a_non_claude_model_answering_is_refused() {
        let tmp = TempDir::new();
        let mut other = result("hi");
        other["modelUsage"] = serde_json::json!({"gpt-9": {}});
        let (a, cc) = agent(&["Read"], 60);
        let (r, _) = go(&fake(&tmp.0, &emitting(&[other])), &a, &cc, &tmp.0);
        assert!(r.unwrap_err().to_string().contains("ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn the_time_limit_kills_the_whole_tree_and_keeps_what_was_said() {
        let tmp = TempDir::new();
        let marker = tmp.0.join("orphan-alive");
        let said = serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "working on it"}]}});
        let script = format!(
            "{}( sleep 3; touch '{}' ) &\nsleep 30\n",
            emitting(&[said]),
            marker.display()
        );
        let (a, cc) = agent(&["Read"], 1);
        let started = Instant::now();
        let (r, _) = go(&fake(&tmp.0, &script), &a, &cc, &tmp.0);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "returned promptly"
        );
        let text = r.unwrap().text;
        assert!(text.starts_with("[stopped at the 1s time limit"), "{text}");
        assert!(text.ends_with("working on it"), "{text}");
        std::thread::sleep(Duration::from_secs(4));
        assert!(!marker.exists(), "a grandchild outlived the stop");
    }

    #[test]
    fn a_cancel_stops_the_cli() {
        let tmp = TempDir::new();
        let (a, cc) = agent(&["Read"], 60);
        let cancel = AtomicBool::new(true);
        let r = run_program(
            &fake(&tmp.0, "sleep 30\n"),
            &a,
            &cc,
            "t",
            None,
            &tmp.0,
            &|_| {},
            &cancel,
        );
        assert!(r.unwrap().text.starts_with("[cancelled"));
    }
}
