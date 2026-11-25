//! The `builtin` adapter: an agent loop in Ensemble itself, so an agent on an
//! LLM backend needs no CLI installed. The loop is harness-rs's `AgentLoop`:
//! the model asks for tools, they run in the working directory, their results
//! go back, until the model answers or `max_steps` is reached.
//!
//! The file tools are confined to the working directory by the OS (they open
//! it as a capability directory). `shell` is not: it runs any program, in the
//! working directory, which is why it is only there when it is listed.

use super::AgentRun;
use crate::runtime::until_set;
use async_trait::async_trait;
use ensemble_config::{
    AgentConfig, BUILTIN_AGENT, BackendSettings, BuiltinAdapter, LlmBackend, builtin_tool_writes,
};
use ensemble_core::port::{Error, Usage};
use ensemble_core::workspace::{self, Changes, Workspace};
use harness::{
    Context, Event, Hook, HookOutcome, Model, ModelError, ModelInfo, ModelOutput, Task, Tool, World,
};
use harness_loop::{AgentLoop, CancellationToken, Outcome};
use harness_models::{AnthropicNative, LlmConfig, OpenAiCompat};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// Run a `builtin` agent on one task, in `workdir`, on `backend`: the one its
/// first model is on, once the router has bound it.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    agent: &AgentConfig,
    ba: &BuiltinAdapter,
    backend: &LlmBackend,
    task: &str,
    context: Option<&str>,
    workdir: &Path,
    progress: &(dyn Fn(&str) + Sync),
    cancel: &AtomicBool,
) -> Result<AgentRun, Error> {
    let workspace = Workspace::new(workdir).map_err(|detail| Error::Unsupported {
        backend: BUILTIN_AGENT.into(),
        detail: format!("agent '{}' cannot start: {detail}", agent.name),
    })?;
    let say = |what: &str| progress(&format!("{} · {what}", agent.name));
    let baseline = ba
        .tools
        .iter()
        .any(|t| builtin_tool_writes(t))
        .then(|| workspace::snapshot(&workspace))
        .flatten();

    let (lines, mut heard) = mpsc::unbounded_channel();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let stop = CancellationToken::new();
    let mut agent_loop = AgentLoop::boxed(model(ba, backend))
        .with_streaming(false)
        .with_cancellation(stop.clone())
        .with_hook(Arc::new(Report {
            lines,
            commands: commands.clone(),
        }));
    for tool in &ba.tools {
        agent_loop = agent_loop.with_tool(tool_named(tool));
    }
    if let Some(prompt) = agent.prompt.as_deref().filter(|p| !p.trim().is_empty()) {
        agent_loop = agent_loop.with_system(prompt);
    }
    let description = match context.filter(|c| !c.trim().is_empty()) {
        Some(ctx) => format!("Context:\n{ctx}\n\n{task}"),
        None => task.to_string(),
    };
    let mut world = harness_context::default_world(workspace.root());

    say(&format!("starting on {} at {}", ba.model(), backend.name));
    let run = agent_loop.run_with_max_iters(
        Task {
            description,
            source: None,
            deadline: None,
        },
        &mut world,
        ba.max_steps,
    );
    let limit = tokio::time::sleep(Duration::from_secs(u64::from(agent.max_seconds)));
    let cancelled = until_set(cancel);
    tokio::pin!(run, limit, cancelled);
    let mut stopped = None;
    let outcome = loop {
        tokio::select! {
            outcome = &mut run => break outcome,
            Some(line) = heard.recv() => say(&line),
            // The loop ends at its next step with what it did so far.
            () = &mut limit, if stopped.is_none() => {
                stopped = Some(format!("stopped at the {}s time limit", agent.max_seconds));
                stop.cancel();
            }
            () = &mut cancelled, if stopped.is_none() => {
                stopped = Some("cancelled".to_string());
                stop.cancel();
            }
        }
    };
    while let Ok(line) = heard.try_recv() {
        say(&line);
    }
    let outcome = outcome.map_err(|e| Error::Protocol {
        backend: BUILTIN_AGENT.into(),
        detail: e.to_string(),
    })?;

    let (text, steps, tool_calls, usage, ended) = match outcome {
        Outcome::Done {
            text,
            iters,
            tools_called,
            usage,
            ..
        } => (text, iters, tools_called, usage, None),
        Outcome::BudgetExhausted {
            iters,
            last_text,
            tools_called,
            usage,
            ..
        } => (
            last_text,
            iters,
            tools_called,
            usage,
            Some(format!("stopped at its {} step limit", ba.max_steps)),
        ),
        Outcome::Stuck {
            reason,
            iters,
            last_text,
            tools_called,
            usage,
            ..
        } => (
            last_text,
            iters,
            tools_called,
            usage,
            Some(format!("stopped as stuck: {reason}")),
        ),
        Outcome::Cancelled {
            iters,
            last_text,
            tools_called,
            usage,
            ..
        } => (
            last_text,
            iters,
            tools_called,
            usage,
            Some(stopped.clone().unwrap_or_else(|| "cancelled".into())),
        ),
    };
    let mut out = AgentRun {
        text: text.unwrap_or_default().trim().to_string(),
        model: ba.model().to_string(),
        usage: Usage {
            prompt_tokens: usage.input_tokens,
            completion_tokens: usage.output_tokens,
        },
        steps,
        tool_calls,
        changes: Changes::new(),
        commands: std::mem::take(&mut *commands.lock().unwrap()),
    };
    if let Some(before) = &baseline
        && let Some(after) = workspace::snapshot(&workspace)
    {
        workspace::diff(before, &after, &mut out.changes);
    }
    if let Some(why) = ended {
        say(&why);
        let said = std::mem::take(&mut out.text);
        out.text = format!("[{why} after {steps} step(s); the work is unfinished]");
        if !said.is_empty() {
            out.text.push('\n');
            out.text.push_str(&said);
        }
    } else {
        say("done");
    }
    Ok(out)
}

/// The model `ba` is bound to, on `backend`, its answers capped at
/// `max_tokens`. A request waits at most the backend's `timeout_seconds`.
fn model(ba: &BuiltinAdapter, backend: &LlmBackend) -> Arc<dyn Model> {
    let config = LlmConfig::new(
        backend.name.clone(),
        backend.settings.base_url(),
        backend.settings.api_key().unwrap_or_default(),
        ba.model(),
    );
    let window = ba
        .fallback
        .first()
        .and_then(|c| c.context_window)
        .unwrap_or(0);
    let timeout = Duration::from_secs(backend.timeout_seconds);
    let inner: Arc<dyn Model> = match backend.settings {
        BackendSettings::OpenAiCompatible { .. } => Arc::new(
            OpenAiCompat::new(config)
                .with_streaming(false)
                .with_timeouts(timeout, timeout)
                .with_context_window(window),
        ),
        BackendSettings::Anthropic { .. } => Arc::new(
            AnthropicNative::new(config)
                .with_streaming(false)
                .with_context_window(window),
        ),
    };
    Arc::new(Capped {
        inner,
        max_tokens: ba.max_tokens,
    })
}

/// A model whose answers are capped at `max_tokens`, which the loop itself
/// has no setting for.
struct Capped {
    inner: Arc<dyn Model>,
    max_tokens: u32,
}

#[async_trait]
impl Model for Capped {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
        let mut ctx = ctx.clone();
        ctx.policy.max_output_tokens = ctx.policy.max_output_tokens.min(self.max_tokens);
        self.inner.complete(&ctx).await
    }

    fn info(&self) -> ModelInfo {
        self.inner.info()
    }
}

/// The harness-rs tool behind one of [`ensemble_config::BUILTIN_TOOLS`].
fn tool_named(name: &str) -> Arc<dyn Tool> {
    use harness_tools::fs::{EditFile, Glob, Grep, ListDir, ReadFile, WriteFile};
    use harness_tools::shell::ShellExec;
    match name {
        "read" => Arc::new(ReadFile),
        "list" => Arc::new(ListDir),
        "grep" => Arc::new(Grep),
        "glob" => Arc::new(Glob),
        "write" => Arc::new(WriteFile),
        "edit" => Arc::new(EditFile),
        "shell" => Arc::new(ShellExec),
        other => unreachable!("the config allows no builtin tool '{other}'"),
    }
}

/// Says what each tool call is, and keeps the commands run. Its lines go
/// through a channel: a hook lives as long as the loop, the caller's
/// `progress` only as long as the run.
struct Report {
    lines: mpsc::UnboundedSender<String>,
    commands: Arc<Mutex<Vec<String>>>,
}

impl Hook for Report {
    fn name(&self) -> &str {
        "ensemble-report"
    }

    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::PreToolUse { .. })
    }

    fn fire(&self, ev: &Event<'_>, _world: &mut World) -> HookOutcome {
        if let Event::PreToolUse { action } = ev {
            let args = &action.args;
            let what = match action.tool.as_str() {
                "shell_exec" => {
                    let mut line = args["program"].as_str().unwrap_or("").to_string();
                    for a in args["args"].as_array().into_iter().flatten() {
                        line.push(' ');
                        line.push_str(a.as_str().unwrap_or(""));
                    }
                    let line = super::brief(&line);
                    self.commands.lock().unwrap().push(line.clone());
                    format!("$ {line}")
                }
                tool => {
                    let on = ["path", "pattern"]
                        .iter()
                        .find_map(|k| args[*k].as_str())
                        .unwrap_or("");
                    super::brief(&format!("{tool} {on}"))
                }
            };
            let _ = self.lines.send(what);
        }
        HookOutcome::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_backends::testing::{ModelServer, chat_server};
    use ensemble_config::{Adapter, Budget, Candidate, Fallback, Isolation, WhenNone};
    use ensemble_core::workspace::testing::TempDir;
    use serde_json::{Value, json};

    fn agent(tools: &[&str], max_steps: u32, max_seconds: u32) -> (AgentConfig, BuiltinAdapter) {
        let ba = BuiltinAdapter {
            fallback: Fallback {
                candidates: vec![Candidate {
                    provider: "local".into(),
                    model: "small".into(),
                    context_window: Some(32768),
                }],
                when_none: WhenNone::Fail,
            },
            tools: tools.iter().map(|t| t.to_string()).collect(),
            max_steps,
            max_tokens: 300,
        };
        let a = AgentConfig {
            name: "fixer".into(),
            prompt: Some("You fix things.".into()),
            max_seconds,
            max_concurrent: 1,
            isolation: Isolation::Shared,
            budget: Budget::Unlimited,
            limit: None,
            adapter: Adapter::Builtin(ba.clone()),
        };
        (a, ba)
    }

    fn backend(server: &ModelServer) -> LlmBackend {
        LlmBackend {
            name: "local".into(),
            timeout_seconds: 10,
            settings: BackendSettings::OpenAiCompatible {
                base_url: server.url.clone(),
                api_key: None,
            },
        }
    }

    fn call(id: &str, tool: &str, args: Value) -> Value {
        json!({"role": "assistant", "content": null, "tool_calls": [{
            "id": id, "type": "function",
            "function": {"name": tool, "arguments": args.to_string()}
        }]})
    }

    fn answer(text: &str) -> Value {
        json!({"role": "assistant", "content": text})
    }

    fn go(
        a: &AgentConfig,
        ba: &BuiltinAdapter,
        server: &ModelServer,
        dir: &Path,
        cancel: &AtomicBool,
    ) -> (Result<AgentRun, Error>, Vec<String>) {
        let seen = Mutex::new(Vec::new());
        let r = crate::runtime::block_on(run(
            a,
            ba,
            &backend(server),
            "Fix the greeting.",
            Some("It says helo."),
            dir,
            &|l| seen.lock().unwrap().push(l.to_string()),
            cancel,
        ));
        (r, seen.into_inner().unwrap())
    }

    #[test]
    fn the_model_reads_and_writes_files_with_its_tools_and_the_changes_are_reported() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("greet.txt"), "helo\n").unwrap();
        let server = chat_server(
            &["small"],
            &[
                call("c1", "read_file", json!({"path": "greet.txt"})),
                call(
                    "c2",
                    "write_file",
                    json!({"path": "greet.txt", "content": "hello\n"}),
                ),
                answer("Fixed the greeting."),
            ],
        );
        let (a, ba) = agent(&["read", "write"], 10, 60);
        let (r, progress) = go(&a, &ba, &server, &tmp.0, &AtomicBool::new(false));
        let r = r.unwrap();
        assert_eq!(r.text, "Fixed the greeting.");
        assert_eq!((r.steps, r.tool_calls), (3, 2));
        assert_eq!(
            std::fs::read_to_string(tmp.0.join("greet.txt")).unwrap(),
            "hello\n"
        );
        assert!(r.changes.contains_key("greet.txt"), "{:?}", r.changes);
        assert!(
            progress.iter().any(|l| l.contains("read_file greet.txt")),
            "{progress:?}"
        );

        let requests = server.requests.lock().unwrap();
        let first: Value = serde_json::from_str(&requests[0]).unwrap();
        assert_eq!(first["model"], "small");
        assert_eq!(first["max_tokens"], 300);
        let tools: Vec<&str> = first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(tools.len(), 2, "{tools:?}");
        assert!(tools.contains(&"read_file") && tools.contains(&"write_file"));
        let said = requests[0].as_str();
        assert!(
            said.contains("You fix things.")
                && said.contains("It says helo.")
                && said.contains("Fix the greeting."),
            "{said}"
        );
    }

    #[test]
    fn a_shell_command_is_kept_and_a_run_at_its_step_limit_is_unfinished() {
        let tmp = TempDir::new();
        let server = chat_server(
            &["small"],
            &[call(
                "c1",
                "shell_exec",
                json!({"program": "echo", "args": ["hi"]}),
            )],
        );
        let (a, ba) = agent(&["shell"], 2, 60);
        let (r, _) = go(&a, &ba, &server, &tmp.0, &AtomicBool::new(false));
        let r = r.unwrap();
        assert!(
            r.commands.contains(&"echo hi".to_string()),
            "{:?}",
            r.commands
        );
        assert!(
            r.text.starts_with("[stopped at its 2 step limit"),
            "{}",
            r.text
        );
    }

    #[test]
    fn a_cancelled_run_stops_and_says_so() {
        let tmp = TempDir::new();
        let server = chat_server(&["small"], &[answer("never seen")]);
        let (a, ba) = agent(&["read"], 10, 60);
        let (r, _) = go(&a, &ba, &server, &tmp.0, &AtomicBool::new(true));
        let r = r.unwrap();
        assert!(r.text.starts_with("[cancelled"), "{}", r.text);
    }
}
