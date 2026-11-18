//! The `codex` adapter: the Codex CLI as a member of the helper team.
//!
//! Like the other CLI adapters it drives the official program as a subprocess,
//! started locally by the person who owns the login: `codex exec --json`, the
//! headless mode, whose JSON lines are read as they arrive. Ensemble does not
//! touch Codex's credentials and never speaks to OpenAI's API itself.
//!
//! Codex has no tool list and no turn limit to set. What limits it is its own
//! sandbox (`read-only`, `workspace-write` or `danger-full-access`), which the
//! adapter passes on and which the config must name. It has no flag for a
//! system prompt either, so an agent's `prompt` goes in front of the task.
//!
//! The event shapes read here (`item.completed` with `agent_message`,
//! `command_execution` and `file_change` items, `turn.completed` with `usage`,
//! `turn.failed`, `error`) follow Codex's documented `exec --json` output. The
//! suite checks them against stand-ins and a saved stream; whether a given
//! Codex release still prints them is what `tests/real_cli.rs` is for.

use super::AgentRun;
use super::process;
use ensemble_config::{AgentConfig, CODEX_AGENT, CodexAdapter, codex_sandbox_writes};
use ensemble_core::port::{Error, Usage};
use ensemble_core::workspace::Changes;
use ensemble_core::workspace::{self, Workspace};
use serde_json::Value;
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// Run a `codex` agent on one task, in `workdir`.
pub async fn run(
    agent: &AgentConfig,
    cx: &CodexAdapter,
    task: &str,
    context: Option<&str>,
    workdir: &Path,
    progress: &(dyn Fn(&str) + Sync),
    cancel: &AtomicBool,
) -> Result<AgentRun, Error> {
    run_program(
        std::slice::from_ref(&cx.command),
        agent,
        cx,
        task,
        context,
        workdir,
        progress,
        cancel,
    )
    .await
}

/// The command line, less the program. Codex runs in the directory it is
/// started in, and does not insist that it is a git repository.
pub fn arguments(agent: &AgentConfig, cx: &CodexAdapter, prompt: &str) -> Vec<String> {
    let prompt = match agent.prompt.as_deref().filter(|p| !p.trim().is_empty()) {
        Some(system) => format!("{}\n\n{prompt}", system.trim()),
        None => prompt.to_string(),
    };
    vec![
        "exec".into(),
        "--json".into(),
        "--skip-git-repo-check".into(),
        "--sandbox".into(),
        cx.sandbox.clone(),
        "--model".into(),
        cx.model.clone(),
        "--".into(),
        prompt,
    ]
}

#[allow(clippy::too_many_arguments)]
async fn run_program(
    program: &[String],
    agent: &AgentConfig,
    cx: &CodexAdapter,
    task: &str,
    context: Option<&str>,
    workdir: &Path,
    progress: &(dyn Fn(&str) + Sync),
    cancel: &AtomicBool,
) -> Result<AgentRun, Error> {
    let backend = CODEX_AGENT.to_string();
    let workspace = Workspace::new(workdir).map_err(|detail| Error::Unsupported {
        backend: backend.clone(),
        detail: format!("agent '{}' cannot start: {detail}", agent.name),
    })?;
    let say = |what: &str| progress(&format!("{} · {what}", agent.name));

    let prompt = match context.filter(|c| !c.trim().is_empty()) {
        Some(ctx) => format!("Context:\n{ctx}\n\n{task}"),
        None => task.to_string(),
    };
    // What Codex changed is found by comparing the project before and after.
    let baseline = codex_sandbox_writes(&cx.sandbox)
        .then(|| workspace::snapshot(&workspace))
        .flatten();

    say("starting Codex");
    let mut stream = Stream::new(&workspace, &say);
    let ended = process::Run {
        backend: CODEX_AGENT,
        program,
        args: arguments(agent, cx, &prompt),
        env: Vec::new(),
        workdir: workspace.root(),
        max_seconds: agent.max_seconds,
        cancel,
    }
    .stream(&mut |line| stream.read(line))
    .await?;

    let mut out = AgentRun {
        text: String::new(),
        model: cx.model.clone(),
        usage: stream.usage,
        steps: stream.turns.max(u32::from(!stream.last_text.is_empty())),
        tool_calls: stream.tool_calls,
        changes: Changes::new(),
        commands: std::mem::take(&mut stream.commands),
    };
    if let Some(before) = &baseline
        && let Some(after) = workspace::snapshot(&workspace)
    {
        workspace::diff(before, &after, &mut out.changes);
    }

    if let Some(why) = ended.stopped {
        say(&why);
        out.text = format!("[{why}; the work is unfinished]");
        if !stream.last_text.trim().is_empty() {
            out.text.push('\n');
            out.text.push_str(&stream.last_text);
        }
        return Ok(out);
    }
    if let Some(error) = &stream.error {
        return Err(Error::ProcessFailed {
            backend,
            code: ended.status,
            stderr: error.clone(),
        });
    }
    if ended.status != Some(0) {
        return Err(Error::ProcessFailed {
            backend,
            code: ended.status,
            stderr: if ended.stderr.trim().is_empty() {
                "it ended without an answer".into()
            } else {
                ended.stderr
            },
        });
    }
    if stream.last_text.trim().is_empty() {
        return Err(Error::Protocol {
            backend,
            detail: "it ended without an agent message to answer with".into(),
        });
    }
    out.text = std::mem::take(&mut stream.last_text);
    say("done");
    Ok(out)
}

/// What the `exec --json` events have said so far.
struct Stream<'a> {
    workspace: &'a Workspace,
    say: &'a (dyn Fn(&str) + Sync),
    turns: u32,
    tool_calls: u32,
    commands: Vec<String>,
    last_text: String,
    usage: Usage,
    error: Option<String>,
}

impl<'a> Stream<'a> {
    fn new(workspace: &'a Workspace, say: &'a (dyn Fn(&str) + Sync)) -> Self {
        Stream {
            workspace,
            say,
            turns: 0,
            tool_calls: 0,
            commands: Vec::new(),
            last_text: String::new(),
            usage: Usage::default(),
            error: None,
        }
    }

    /// One line of output. Anything that is not a JSON event is skipped.
    fn read(&mut self, line: &str) {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("turn.completed") => {
                self.turns += 1;
                let used = |k: &str| {
                    event
                        .pointer(&format!("/usage/{k}"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u32
                };
                self.usage.prompt_tokens += used("input_tokens");
                self.usage.completion_tokens += used("output_tokens");
            }
            Some("turn.failed") => {
                self.error = event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| Some("the turn failed".into()));
            }
            Some("error") => {
                if self.error.is_none() {
                    self.error = event
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }
            Some("item.completed") => {
                if let Some(item) = event.get("item") {
                    self.item(item);
                }
            }
            _ => {}
        }
    }

    fn item(&mut self, item: &Value) {
        match item.get("type").and_then(Value::as_str) {
            Some("agent_message") => {
                if let Some(text) = item.get("text").and_then(Value::as_str)
                    && !text.trim().is_empty()
                {
                    self.last_text = text.to_string();
                }
            }
            Some("command_execution") => {
                self.tool_calls += 1;
                let command = item.get("command").and_then(Value::as_str).unwrap_or("");
                (self.say)(&format!(
                    "step {} · command({})",
                    self.tool_calls,
                    super::brief(command)
                ));
                // One that failed did not run to its end.
                if item.get("exit_code").and_then(Value::as_i64) == Some(0) {
                    self.commands.push(super::brief(command));
                }
            }
            Some("file_change") => {
                self.tool_calls += 1;
                let paths: Vec<String> = item
                    .get("changes")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|c| c.get("path").and_then(Value::as_str))
                            .map(|p| self.workspace.show(Path::new(p)))
                            .collect()
                    })
                    .unwrap_or_default();
                (self.say)(&format!(
                    "step {} · edit({})",
                    self.tool_calls,
                    super::brief(&paths.join(", "))
                ));
            }
            Some("mcp_tool_call") | Some("web_search") => self.tool_calls += 1,
            _ => {}
        }
    }
}

/// Check that the CLI is there and starts. `--version` spends nothing.
pub fn health(command: &str) -> Result<(), Error> {
    process::health(CODEX_AGENT, command)
}

#[cfg(test)]
mod tests {
    use super::process::testing::{emitting, fake};
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn run_program(
        program: &[String],
        agent: &AgentConfig,
        cx: &CodexAdapter,
        task: &str,
        context: Option<&str>,
        workdir: &Path,
        progress: &(dyn Fn(&str) + Sync),
        cancel: &AtomicBool,
    ) -> Result<AgentRun, Error> {
        crate::runtime::block_on(super::run_program(
            program, agent, cx, task, context, workdir, progress, cancel,
        ))
    }
    use ensemble_config::Adapter;
    use ensemble_core::workspace::testing::TempDir;
    use serde_json::json;

    fn agent(sandbox: &str, max_seconds: u32) -> (AgentConfig, CodexAdapter) {
        let cx = CodexAdapter {
            command: "codex".into(),
            model: "gpt-5-codex".into(),
            sandbox: sandbox.into(),
        };
        let a = AgentConfig {
            name: "fixer".into(),
            prompt: Some("You fix things.".into()),
            max_seconds,
            max_concurrent: 1,
            isolation: ensemble_config::Isolation::Shared,
            budget: ensemble_config::Budget::Unlimited,
            limit: None,
            adapter: Adapter::Codex(cx.clone()),
        };
        (a, cx)
    }

    fn go(
        program: &[String],
        a: &AgentConfig,
        cx: &CodexAdapter,
        dir: &Path,
    ) -> (Result<AgentRun, Error>, Vec<String>) {
        let seen = std::sync::Mutex::new(Vec::new());
        let r = run_program(
            program,
            a,
            cx,
            "fix it",
            Some("it is broken"),
            dir,
            &|l| seen.lock().unwrap().push(l.to_string()),
            &AtomicBool::new(false),
        );
        (r, seen.into_inner().unwrap())
    }

    fn message(text: &str) -> serde_json::Value {
        json!({"type": "item.completed", "item": {"id": "i", "type": "agent_message", "text": text}})
    }

    fn command(cmd: &str, exit: i64) -> serde_json::Value {
        json!({"type": "item.completed", "item": {"id": "c", "type": "command_execution",
               "command": cmd, "aggregated_output": "", "exit_code": exit, "status": "completed"}})
    }

    fn done(input: u64, output: u64) -> serde_json::Value {
        json!({"type": "turn.completed", "usage": {"input_tokens": input, "cached_input_tokens": 0, "output_tokens": output}})
    }

    #[test]
    fn the_command_line_carries_the_sandbox_model_and_the_prompt_in_front_of_the_task() {
        let (a, cx) = agent("workspace-write", 60);
        let argv = arguments(&a, &cx, "do it");
        assert_eq!(&argv[..2], ["exec", "--json"]);
        let at = |flag: &str| argv.iter().position(|x| x == flag).unwrap();
        assert_eq!(argv[at("--sandbox") + 1], "workspace-write");
        assert_eq!(argv[at("--model") + 1], "gpt-5-codex");
        assert!(argv.contains(&"--skip-git-repo-check".to_string()));
        assert_eq!(argv[argv.len() - 2], "--");
        assert_eq!(argv.last().unwrap(), "You fix things.\n\ndo it");
        let (mut bare, cx) = agent("read-only", 60);
        bare.prompt = None;
        assert_eq!(arguments(&bare, &cx, "do it").last().unwrap(), "do it");
    }

    #[test]
    fn a_run_reports_progress_commands_usage_and_the_answer() {
        let tmp = TempDir::new();
        let script = emitting(&[
            json!({"type": "thread.started", "thread_id": "t"}),
            json!({"type": "turn.started"}),
            command("cargo test", 0),
            command("rm -rf /", 1),
            message("fixed it"),
            done(100, 7),
        ]) + "echo noise >&2\n";
        let (a, cx) = agent("read-only", 60);
        let (r, seen) = go(&fake(&tmp.0, &script), &a, &cx, &tmp.0);
        let r = r.unwrap();
        assert_eq!(r.text, "fixed it");
        assert_eq!(r.model, "gpt-5-codex");
        assert_eq!((r.steps, r.tool_calls), (1, 2));
        assert_eq!((r.usage.prompt_tokens, r.usage.completion_tokens), (100, 7));
        assert_eq!(
            r.commands,
            vec!["cargo test"],
            "one that failed did not run"
        );
        assert_eq!(
            seen,
            vec![
                "fixer · starting Codex",
                "fixer · step 1 · command(cargo test)",
                "fixer · step 2 · command(rm -rf /)",
                "fixer · done",
            ]
        );
    }

    #[test]
    fn the_last_message_is_the_answer_and_usage_adds_up_over_turns() {
        let tmp = TempDir::new();
        let script = emitting(&[
            message("first"),
            done(10, 1),
            message("second"),
            done(20, 2),
        ]);
        let (a, cx) = agent("read-only", 60);
        let r = go(&fake(&tmp.0, &script), &a, &cx, &tmp.0).0.unwrap();
        assert_eq!(r.text, "second");
        assert_eq!(
            (r.steps, r.usage.prompt_tokens, r.usage.completion_tokens),
            (2, 30, 3)
        );
    }

    #[test]
    fn files_codex_changed_are_listed_only_when_its_sandbox_can_write() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("a.txt"), "old").unwrap();
        let script = format!(
            "echo new > a.txt\n{}",
            emitting(&[message("changed it"), done(1, 1)])
        );
        let (a, cx) = agent("workspace-write", 60);
        let (r, _) = go(&fake(&tmp.0, &script), &a, &cx, &tmp.0);
        assert!(r.unwrap().changes.iter().any(|(p, _)| p == "a.txt"));
        let (a, cx) = agent("read-only", 60);
        let (r, _) = go(&fake(&tmp.0, &script), &a, &cx, &tmp.0);
        assert!(
            r.unwrap().changes.is_empty(),
            "a read-only agent's changes are not looked for"
        );
    }

    #[test]
    fn a_failed_turn_a_failing_cli_and_a_missing_answer_are_errors_with_their_reason() {
        let tmp = TempDir::new();
        let (a, cx) = agent("read-only", 60);
        let failed =
            emitting(&[json!({"type": "turn.failed", "error": {"message": "rate limited"}})]);
        let err = go(&fake(&tmp.0, &failed), &a, &cx, &tmp.0).0.unwrap_err();
        assert!(err.to_string().contains("rate limited"), "{err}");

        let crashed = "echo 'not logged in' >&2\nexit 1\n";
        let err = go(&fake(&tmp.0, crashed), &a, &cx, &tmp.0).0.unwrap_err();
        assert!(err.to_string().contains("not logged in"), "{err}");

        let silent = emitting(&[json!({"type": "turn.started"}), done(1, 1)]);
        let err = go(&fake(&tmp.0, &silent), &a, &cx, &tmp.0).0.unwrap_err();
        assert!(err.to_string().contains("agent message"), "{err}");
    }

    #[test]
    fn a_missing_cli_says_what_is_missing() {
        let tmp = TempDir::new();
        let (a, cx) = agent("read-only", 60);
        let err = go(&["/nonexistent/codex".into()], &a, &cx, &tmp.0)
            .0
            .unwrap_err();
        assert!(matches!(err, Error::Spawn { .. }), "{err}");
        assert!(health("/nonexistent/codex").is_err());
    }

    #[test]
    fn the_time_limit_keeps_what_was_said() {
        let tmp = TempDir::new();
        let script = format!("{}sleep 30\n", emitting(&[message("so far")]));
        let (a, cx) = agent("read-only", 1);
        let started = std::time::Instant::now();
        let r = go(&fake(&tmp.0, &script), &a, &cx, &tmp.0).0.unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert!(
            r.text.contains("the work is unfinished") && r.text.contains("so far"),
            "{}",
            r.text
        );
    }

    #[test]
    fn every_saved_event_stream_still_parses() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let tmp = TempDir::new();
            let script = format!("cat '{}'\n", path.display());
            let (a, cx) = agent("read-only", 60);
            let r = go(&fake(&tmp.0, &script), &a, &cx, &tmp.0).0;
            let r = r.unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert!(!r.text.is_empty(), "{}: no answer", path.display());
            checked += 1;
        }
        assert!(checked > 0, "no fixtures in {}", dir.display());
    }
}
