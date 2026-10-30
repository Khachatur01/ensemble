//! The `opencode` adapter: the OpenCode CLI as a member of the team.
//!
//! OpenCode is a whole agent: it runs its own loop with its own tools in the
//! project directory, on whichever provider and model it is given. Its `run`
//! command has no flags for a system prompt or allowed tools, so Ensemble
//! hands it an agent of its own instead: `OPENCODE_CONFIG_CONTENT` carries an
//! agent definition with the prompt, the step cap and a permission set that
//! denies everything (`"*": "deny"`) but the tools listed. OpenCode hides
//! denied tools from the model, so it cannot even try them.
//!
//! An agent on one of the LLM backends gets that backend as an OpenCode
//! provider in the same inline config, so the endpoint and its key are set
//! once, in Ensemble's config, and never written anywhere else.
//!
//! `opencode run --standalone` starts a private server for the run, which
//! inherits this environment; the shared background service would not.
//! `--format json` prints one event per line, which is where progress, the
//! commands it ran and its answer come from.

use super::AgentRun;
use super::process;
use crate::config::{
    AgentConfig, BackendSettings, LlmBackend, OPENCODE_AGENT, OpenCodeAdapter, opencode_tool_writes,
};
use crate::port::{Error, Usage};
use crate::workspace::{self, Changes, Workspace};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// The AI SDK package OpenCode talks to an OpenAI-compatible endpoint with.
const OPENAI_COMPATIBLE_PACKAGE: &str = "@ai-sdk/openai-compatible";

/// Most tokens a reply on an LLM backend may take; a quarter of the context
/// when that is less.
const MAX_OUTPUT_TOKENS: u32 = 32_768;

/// Shown by `ensemble show` in place of an API key.
pub const REDACTED: &str = "<redacted>";

/// Run an `opencode` agent on one task, in `workdir`. `backend` is the LLM
/// backend the agent names, if it names one.
#[allow(clippy::too_many_arguments)]
pub fn run(
    agent: &AgentConfig,
    oc: &OpenCodeAdapter,
    backend: Option<&LlmBackend>,
    task: &str,
    context: Option<&str>,
    workdir: &Path,
    progress: &dyn Fn(&str),
    cancel: &AtomicBool,
) -> Result<AgentRun, Error> {
    run_program(
        std::slice::from_ref(&oc.command),
        agent,
        oc,
        backend,
        task,
        context,
        workdir,
        progress,
        cancel,
    )
}

/// The name the agent runs under inside OpenCode.
pub fn agent_id(agent: &AgentConfig) -> String {
    format!("ensemble-{}", safe(&agent.name))
}

/// The name an LLM backend is given as an OpenCode provider.
pub fn provider_id(backend: &LlmBackend) -> String {
    format!("ensemble-{}", safe(&backend.name))
}

/// `name` with anything but letters, digits, `-` and `_` turned into `-`.
fn safe(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// OpenCode's permission set for `tools`: everything denied but what is
/// listed. `bash(git *)` allows only commands matching the pattern.
pub fn permissions(tools: &[String]) -> Value {
    let mut p = Map::new();
    p.insert("*".into(), json!("deny"));
    for tool in tools {
        match tool.split_once('(') {
            Some((name, rest)) => {
                let pattern = rest.strip_suffix(')').unwrap_or(rest);
                let entry = p
                    .entry(name.to_string())
                    .or_insert_with(|| json!({ "*": "deny" }));
                // A plain grant of the whole tool already covers any pattern.
                if let Value::Object(rules) = entry {
                    rules.insert(pattern.to_string(), json!("allow"));
                }
            }
            None => {
                p.insert(tool.clone(), json!("allow"));
            }
        }
    }
    Value::Object(p)
}

/// The inline OpenCode config that defines the agent and, when it runs on an
/// LLM backend, the provider for it. It holds the backend's API key: it goes
/// only into the CLI's environment, and [`redact`] hides it for display.
pub fn config(agent: &AgentConfig, oc: &OpenCodeAdapter, backend: Option<&LlmBackend>) -> Value {
    let mut def = json!({
        "description": format!("Ensemble agent '{}'", agent.name),
        "mode": "primary",
        "steps": oc.max_steps,
        "permission": permissions(&oc.tools),
    });
    if let Some(prompt) = agent.prompt.as_deref().filter(|p| !p.trim().is_empty()) {
        def["prompt"] = json!(prompt);
    }
    let mut cfg = json!({ "agent": { agent_id(agent): def } });
    if let Some(backend) = backend {
        cfg["provider"] =
            json!({ provider_id(backend): provider(backend, oc, agent.context_window) });
    }
    cfg
}

/// An LLM backend as an OpenCode provider serving the agent's one model.
fn provider(backend: &LlmBackend, oc: &OpenCodeAdapter, context_window: Option<u32>) -> Value {
    let BackendSettings::OpenAiCompatible {
        base_url, api_key, ..
    } = &backend.settings;
    let mut options = json!({
        "baseURL": base_url,
        "timeout": backend.timeout_seconds.saturating_mul(1000),
    });
    if let Some(key) = api_key {
        options["apiKey"] = json!(key);
    }
    let mut model = json!({ "name": oc.model });
    if let Some(context) = context_window {
        model["limit"] = json!({
            "context": context,
            "output": (context / 4).min(MAX_OUTPUT_TOKENS),
        });
    }
    json!({
        "npm": OPENAI_COMPATIBLE_PACKAGE,
        "name": format!("Ensemble LLM backend '{}'", backend.name),
        "options": options,
        "models": { oc.model.clone(): model },
    })
}

/// [`config`] with every API key in it replaced by [`REDACTED`], for showing.
pub fn redact(cfg: &mut Value) {
    if let Some(providers) = cfg.get_mut("provider").and_then(Value::as_object_mut) {
        for p in providers.values_mut() {
            if let Some(key) = p.pointer_mut("/options/apiKey") {
                *key = json!(REDACTED);
            }
        }
    }
}

/// The model as OpenCode names it: `provider/model`, with the provider made
/// from the LLM backend when the agent runs on one.
pub fn model_ref(oc: &OpenCodeAdapter, backend: Option<&LlmBackend>) -> String {
    match backend {
        Some(b) => format!("{}/{}", provider_id(b), oc.model),
        None => oc.model.clone(),
    }
}

/// The command line, less the program. `--` keeps a task that starts with a
/// dash from being read as a flag.
pub fn arguments(
    agent: &AgentConfig,
    oc: &OpenCodeAdapter,
    backend: Option<&LlmBackend>,
    prompt: &str,
) -> Vec<String> {
    vec![
        "run".into(),
        "--standalone".into(),
        "--format".into(),
        "json".into(),
        "--agent".into(),
        agent_id(agent),
        "-m".into(),
        model_ref(oc, backend),
        "--".into(),
        prompt.into(),
    ]
}

/// [`run`], with the program given as an argv prefix, so tests can stand in a
/// script for `opencode`.
#[allow(clippy::too_many_arguments)]
fn run_program(
    program: &[String],
    agent: &AgentConfig,
    oc: &OpenCodeAdapter,
    backend: Option<&LlmBackend>,
    task: &str,
    context: Option<&str>,
    workdir: &Path,
    progress: &dyn Fn(&str),
    cancel: &AtomicBool,
) -> Result<AgentRun, Error> {
    let workspace = Workspace::new(workdir).map_err(|detail| Error::Unsupported {
        backend: OPENCODE_AGENT.into(),
        detail: format!("agent '{}' cannot start: {detail}", agent.name),
    })?;
    let say = |what: &str| progress(&format!("{} · {what}", agent.name));
    let prompt = match context.filter(|c| !c.trim().is_empty()) {
        Some(ctx) => format!("Context:\n{ctx}\n\n{task}"),
        None => task.to_string(),
    };

    // What OpenCode changed is found by comparing the project before and
    // after, when it has a tool that can change anything.
    let baseline = oc
        .tools
        .iter()
        .any(|t| opencode_tool_writes(t))
        .then(|| workspace::snapshot(&workspace))
        .flatten();

    say("starting OpenCode");
    let mut stream = Stream::new(&workspace, &say);
    let ended = process::Run {
        backend: OPENCODE_AGENT,
        program,
        args: arguments(agent, oc, backend, &prompt),
        env: vec![(
            "OPENCODE_CONFIG_CONTENT".into(),
            config(agent, oc, backend).to_string(),
        )],
        workdir: workspace.root(),
        max_seconds: agent.max_seconds,
        cancel,
    }
    .stream(&mut |line| stream.read(line))?;

    let mut out = AgentRun {
        text: String::new(),
        model: oc.model.clone(),
        usage: stream.usage,
        steps: stream.steps,
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
        out.text = format!(
            "[{why} after {} step(s); the work is unfinished]",
            out.steps
        );
        if !stream.last_text.trim().is_empty() {
            out.text.push('\n');
            out.text.push_str(&stream.last_text);
        }
        return Ok(out);
    }
    if ended.status != Some(0) || !stream.errors.is_empty() {
        let reason = if stream.errors.is_empty() {
            ended.stderr
        } else {
            stream.errors.join("; ")
        };
        return Err(Error::ProcessFailed {
            backend: OPENCODE_AGENT.into(),
            code: ended.status,
            stderr: if reason.trim().is_empty() {
                "it ended without an answer".into()
            } else {
                reason
            },
        });
    }
    out.text = if stream.step_text.is_empty() {
        stream.last_text.clone()
    } else {
        stream.step_text.join("\n\n")
    };
    say("done");
    Ok(out)
}

/// What the `--format json` events have said so far.
struct Stream<'a> {
    workspace: &'a Workspace,
    say: &'a dyn Fn(&str),
    steps: u32,
    tool_calls: u32,
    usage: Usage,
    commands: Vec<String>,
    /// The text of the step in progress; the last step's is the answer.
    step_text: Vec<String>,
    last_text: String,
    errors: Vec<String>,
}

impl<'a> Stream<'a> {
    fn new(workspace: &'a Workspace, say: &'a dyn Fn(&str)) -> Self {
        Stream {
            workspace,
            say,
            steps: 0,
            tool_calls: 0,
            usage: Usage::default(),
            commands: Vec::new(),
            step_text: Vec::new(),
            last_text: String::new(),
            errors: Vec::new(),
        }
    }

    /// One line of output. Anything that is not a JSON event is skipped.
    fn read(&mut self, line: &str) {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let part = event.get("part").cloned().unwrap_or(Value::Null);
        match event.get("type").and_then(Value::as_str) {
            Some("step_start") => {
                self.steps += 1;
                self.step_text.clear();
            }
            Some("text") => {
                if let Some(t) = part.get("text").and_then(Value::as_str)
                    && !t.trim().is_empty()
                {
                    self.step_text.push(t.to_string());
                    self.last_text = t.to_string();
                }
            }
            Some("tool_use") => self.tool(&part),
            Some("step_finish") => {
                let n = |path: &str| part.pointer(path).and_then(Value::as_u64).unwrap_or(0) as u32;
                self.usage.prompt_tokens +=
                    n("/tokens/input") + n("/tokens/cache/read") + n("/tokens/cache/write");
                self.usage.completion_tokens += n("/tokens/output") + n("/tokens/reasoning");
            }
            Some("error") => {
                let message = event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("an error with no message");
                self.errors.push(message.to_string());
            }
            _ => {}
        }
    }

    fn tool(&mut self, part: &Value) {
        self.tool_calls += 1;
        let name = part.get("tool").and_then(Value::as_str).unwrap_or("tool");
        let input = part.pointer("/state/input").cloned().unwrap_or(Value::Null);
        let done = part.pointer("/state/status").and_then(Value::as_str) == Some("completed");
        let shown = self.describe(&input);
        if name == "bash" && done {
            self.commands.push(shown.clone());
        }
        (self.say)(&format!("step {} · {name}({shown})", self.steps.max(1)));
    }

    /// A tool's input on one short line: the path for file tools, the command
    /// for `bash`, the arguments otherwise.
    fn describe(&self, input: &Value) -> String {
        for key in ["filePath", "path", "file_path"] {
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

/// Check that the CLI is there and starts. `--version` spends nothing.
pub fn health(command: &str) -> Result<(), Error> {
    process::health(OPENCODE_AGENT, command)
}

#[cfg(test)]
mod tests {
    use super::process::testing::{emitting, fake};
    use super::*;
    use crate::config::Adapter;
    use crate::workspace::testing::TempDir;

    fn agent(tools: &[&str], max_seconds: u32) -> (AgentConfig, OpenCodeAdapter) {
        let oc = OpenCodeAdapter {
            command: "opencode".into(),
            model: "ollama/qwen3.5:9b".into(),
            llm_backend: None,
            tools: tools.iter().map(|t| t.to_string()).collect(),
            max_steps: 9,
        };
        let a = AgentConfig {
            name: "code reviewer".into(),
            prompt: Some("You review code.".into()),
            context_window: None,
            max_seconds,
            adapter: Adapter::OpenCode(oc.clone()),
        };
        (a, oc)
    }

    fn step(events: Vec<Value>) -> Vec<Value> {
        let mut v = vec![json!({"type": "step_start", "part": {}})];
        v.extend(events);
        v.push(json!({"type": "step_finish", "part": {"reason": "stop",
            "tokens": {"input": 10, "output": 3, "reasoning": 1, "cache": {"read": 100, "write": 0}}}}));
        v
    }

    fn tool(name: &str, status: &str, input: Value) -> Value {
        json!({"type": "tool_use", "part": {"tool": name, "state": {"status": status, "input": input}}})
    }

    fn text(t: &str) -> Value {
        json!({"type": "text", "part": {"text": t}})
    }

    fn go(
        program: &[String],
        a: &AgentConfig,
        oc: &OpenCodeAdapter,
        dir: &Path,
    ) -> (Result<AgentRun, Error>, Vec<String>) {
        let seen = std::cell::RefCell::new(Vec::new());
        let r = run_program(
            program,
            a,
            oc,
            None,
            "review it",
            None,
            dir,
            &|l| seen.borrow_mut().push(l.to_string()),
            &AtomicBool::new(false),
        );
        (r, seen.into_inner())
    }

    fn local_backend(api_key: Option<&str>) -> LlmBackend {
        LlmBackend {
            name: "local box".into(),
            timeout_seconds: 600,
            settings: BackendSettings::OpenAiCompatible {
                base_url: "http://127.0.0.1:8888/v1".into(),
                api_key: api_key.map(String::from),
                base_url_env: "L".into(),
            },
        }
    }

    #[test]
    fn an_agent_on_an_llm_backend_gets_it_as_its_provider() {
        let (mut a, mut oc) = agent(&["read"], 60);
        oc.model = "qwen3.5-9b".into();
        oc.llm_backend = Some("local box".into());
        a.context_window = Some(65536);
        let b = local_backend(Some("sk-secret"));
        let cfg = config(&a, &oc, Some(&b));
        assert_eq!(
            cfg["provider"]["ensemble-local-box"],
            json!({
                "npm": "@ai-sdk/openai-compatible",
                "name": "Ensemble LLM backend 'local box'",
                "options": {
                    "baseURL": "http://127.0.0.1:8888/v1",
                    "timeout": 600000,
                    "apiKey": "sk-secret"
                },
                "models": {"qwen3.5-9b": {"name": "qwen3.5-9b",
                    "limit": {"context": 65536, "output": 16384}}}
            })
        );
        let args = arguments(&a, &oc, Some(&b), "go");
        assert!(
            args.windows(2)
                .any(|w| w == ["-m", "ensemble-local-box/qwen3.5-9b"]),
            "{args:?}"
        );
    }

    #[test]
    fn a_backend_without_a_key_sends_none_and_a_shown_config_hides_one() {
        let (a, mut oc) = agent(&["read"], 60);
        oc.llm_backend = Some("local box".into());
        let keyless = config(&a, &oc, Some(&local_backend(None)));
        assert!(
            keyless
                .pointer("/provider/ensemble-local-box/options/apiKey")
                .is_none()
        );

        let mut shown = config(&a, &oc, Some(&local_backend(Some("sk-secret"))));
        redact(&mut shown);
        let text = shown.to_string();
        assert!(!text.contains("sk-secret"), "{text}");
        assert!(text.contains(REDACTED), "{text}");
    }

    #[test]
    fn the_agent_is_defined_inline_with_everything_denied_but_its_tools() {
        let (a, oc) = agent(&["read", "glob", "bash(git *)", "bash(cargo test*)"], 60);
        let cfg = config(&a, &oc, None);
        assert!(cfg.get("provider").is_none(), "no backend, no provider");
        let def = &cfg["agent"]["ensemble-code-reviewer"];
        assert_eq!(def["prompt"], "You review code.");
        assert_eq!(def["steps"], 9);
        assert_eq!(def["mode"], "primary");
        assert_eq!(
            def["permission"],
            json!({
                "*": "deny",
                "read": "allow",
                "glob": "allow",
                "bash": {"*": "deny", "git *": "allow", "cargo test*": "allow"}
            })
        );

        let args = arguments(&a, &oc, None, "-v do it");
        assert_eq!(
            args,
            [
                "run",
                "--standalone",
                "--format",
                "json",
                "--agent",
                "ensemble-code-reviewer",
                "-m",
                "ollama/qwen3.5:9b",
                "--",
                "-v do it"
            ]
        );
    }

    #[test]
    fn a_run_reports_progress_commands_usage_and_the_last_steps_text() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("a.rs"), "x").unwrap();
        let file = tmp.0.canonicalize().unwrap().join("a.rs");
        let mut events = step(vec![
            text("Looking first."),
            tool(
                "read",
                "completed",
                json!({"filePath": file.display().to_string()}),
            ),
            tool("bash", "completed", json!({"command": "cargo test"})),
            tool("bash", "error", json!({"command": "rm -rf /"})),
        ]);
        events.extend(step(vec![text("All good."), text("No bugs.")]));
        // The definition must reach the CLI through its environment.
        let script = format!(
            "{}case \"$OPENCODE_CONFIG_CONTENT\" in *ensemble-code-reviewer*) ;; *) exit 9 ;; esac\n",
            emitting(&events)
        );
        let (a, oc) = agent(&["read", "bash"], 60);
        let (r, seen) = go(&fake(&tmp.0, &script), &a, &oc, &tmp.0);
        let r = r.unwrap();
        assert_eq!(r.text, "All good.\n\nNo bugs.");
        assert_eq!((r.steps, r.tool_calls), (2, 3));
        assert_eq!((r.usage.prompt_tokens, r.usage.completion_tokens), (220, 8));
        assert_eq!(
            r.commands,
            vec!["cargo test"],
            "a failed command is not listed"
        );
        assert_eq!(
            seen,
            vec![
                "code reviewer · starting OpenCode",
                "code reviewer · step 1 · read(a.rs)",
                "code reviewer · step 1 · bash(cargo test)",
                "code reviewer · step 1 · bash(rm -rf /)",
                "code reviewer · done",
            ]
        );
    }

    #[test]
    fn files_opencode_changed_are_listed() {
        let tmp = TempDir::new();
        let script = format!(
            "echo new > made.txt\n{}",
            emitting(&step(vec![text("made it")]))
        );
        let (a, oc) = agent(&["read", "edit"], 60);
        let (r, _) = go(&fake(&tmp.0, &script), &a, &oc, &tmp.0);
        assert_eq!(
            r.unwrap().changes.get("made.txt"),
            Some(&crate::workspace::Change::Added)
        );
    }

    #[test]
    fn an_error_event_is_a_failure_with_its_message() {
        let tmp = TempDir::new();
        let error = json!({"type": "error", "error": {"type": "provider.no-route",
            "message": "Model unavailable: ollama/nope"}});
        let (a, oc) = agent(&["read"], 60);
        let (r, _) = go(
            &fake(&tmp.0, &(emitting(&[error]) + "exit 1\n")),
            &a,
            &oc,
            &tmp.0,
        );
        let err = r.unwrap_err().to_string();
        assert!(
            err.contains("exited 1") && err.contains("Model unavailable"),
            "{err}"
        );
    }

    #[test]
    fn a_missing_cli_says_what_is_missing() {
        let tmp = TempDir::new();
        let (a, oc) = agent(&["read"], 60);
        let (r, _) = go(&["/nonexistent/opencode".to_string()], &a, &oc, &tmp.0);
        let err = r.unwrap_err().to_string();
        assert!(
            err.contains("could not start") && err.contains("PATH"),
            "{err}"
        );
    }

    #[test]
    fn the_time_limit_stops_it_and_keeps_what_was_said() {
        let tmp = TempDir::new();
        let script = format!(
            "{}sleep 30\n",
            emitting(&[
                json!({"type": "step_start", "part": {}}),
                text("working on it")
            ])
        );
        let (a, oc) = agent(&["read"], 1);
        let started = std::time::Instant::now();
        let (r, _) = go(&fake(&tmp.0, &script), &a, &oc, &tmp.0);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        let text = r.unwrap().text;
        assert!(
            text.starts_with("[stopped at the 1s time limit after 1 step(s)"),
            "{text}"
        );
        assert!(text.ends_with("working on it"), "{text}");
    }
}
