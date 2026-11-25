//! The `opencode` adapter: the OpenCode CLI, as a harness of the runner.
//!
//! OpenCode is a whole agent: it runs its own loop with its own tools in the
//! project directory, on whichever provider and model it is given. Its `run`
//! command has no flags for a system prompt or allowed tools, so Ensemble
//! hands it an agent of its own instead: `OPENCODE_CONFIG_CONTENT` carries an
//! agent definition with the step cap and a permission set that denies
//! everything (`"*": "deny"`) but the tools listed. OpenCode hides denied
//! tools from the model, so it cannot even try them. OpenCode keeps its own
//! system prompt: Ensemble adds none.
//!
//! The run.s model is on a provider of the ANN pool, which goes in as an
//! OpenCode provider in the same inline config, so the endpoint and its key
//! are kept by the runner alone and never written anywhere else.
//!
//! `opencode run --standalone` starts a private server for the run, which
//! inherits this environment; the shared background service would not.
//! `--format json` prints one event per line, which is where progress, the
//! commands it ran and its answer come from.

use super::AgentRun;
use super::process;
use crate::spec::{OpenCodeAdapter, RunSpec, opencode_tool_writes};
use ensemble_config::{LlmBackend, OPENCODE_AGENT};
use ensemble_core::port::{Error, Usage};
use ensemble_core::workspace::{self, Changes, Workspace};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// The AI SDK package OpenCode talks to an OpenAI-compatible endpoint with.
const OPENAI_COMPATIBLE_PACKAGE: &str = "@ai-sdk/openai-compatible";

/// Most tokens a reply on an LLM backend may take; a quarter of the context
/// when that is less.
const MAX_OUTPUT_TOKENS: u32 = 32_768;

/// Shown in place of an API key.
pub const REDACTED: &str = "<redacted>";

/// Run OpenCode on one task, in `workdir`, on its model of `backend`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    agent: &RunSpec,
    oc: &OpenCodeAdapter,
    backend: &LlmBackend,
    task: &str,
    context: Option<&str>,
    workdir: &Path,
    progress: &(dyn Fn(&str) + Sync),
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
    .await
}

/// The name the agent runs under inside OpenCode.
pub fn agent_id(agent: &RunSpec) -> String {
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

/// The inline OpenCode config that defines the agent and the provider of its
/// model. It holds the provider's API key: it goes only into the CLI's
/// environment, and [`redact`] hides it for display.
pub fn config(agent: &RunSpec, oc: &OpenCodeAdapter, backend: &LlmBackend) -> Value {
    let def = json!({
        "description": format!("Ensemble's run, {}", agent.name),
        "mode": "primary",
        "steps": oc.max_steps,
        "permission": permissions(&oc.tools),
    });
    json!({
        "agent": { agent_id(agent): def },
        "provider": { provider_id(backend): provider(backend, oc) },
    })
}

/// A provider of the ANN pool as an OpenCode provider serving the run's one
/// model.
fn provider(backend: &LlmBackend, oc: &OpenCodeAdapter) -> Value {
    // The picker gives OpenCode only OpenAI-compatible providers; the URL and
    // key are the same either way if one gets this far.
    let (base_url, api_key) = (backend.settings.base_url(), backend.settings.api_key());
    let mut options = json!({
        "baseURL": base_url,
        "timeout": backend.timeout_seconds.saturating_mul(1000),
    });
    if let Some(key) = api_key {
        options["apiKey"] = json!(key);
    }
    let model = json!({
        "name": oc.model,
        "limit": {
            "context": oc.context_window,
            "output": (oc.context_window / 4).min(MAX_OUTPUT_TOKENS),
        },
    });
    json!({
        "npm": OPENAI_COMPATIBLE_PACKAGE,
        "name": format!("Ensemble provider '{}'", backend.name),
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

/// The model as OpenCode names it: `provider/model`, the provider made from
/// the pool's.
pub fn model_ref(oc: &OpenCodeAdapter, backend: &LlmBackend) -> String {
    format!("{}/{}", provider_id(backend), oc.model)
}

/// The command line, less the program. `--` keeps a task that starts with a
/// dash from being read as a flag.
pub fn arguments(
    agent: &RunSpec,
    oc: &OpenCodeAdapter,
    backend: &LlmBackend,
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
async fn run_program(
    program: &[String],
    agent: &RunSpec,
    oc: &OpenCodeAdapter,
    backend: &LlmBackend,
    task: &str,
    context: Option<&str>,
    workdir: &Path,
    progress: &(dyn Fn(&str) + Sync),
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
    .stream(&mut |line| stream.read(line))
    .await?;

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
    say: &'a (dyn Fn(&str) + Sync),
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
    fn new(workspace: &'a Workspace, say: &'a (dyn Fn(&str) + Sync)) -> Self {
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

    #[allow(clippy::too_many_arguments)]
    fn run_program(
        program: &[String],
        agent: &RunSpec,
        oc: &OpenCodeAdapter,
        backend: &LlmBackend,
        task: &str,
        context: Option<&str>,
        workdir: &Path,
        progress: &(dyn Fn(&str) + Sync),
        cancel: &AtomicBool,
    ) -> Result<AgentRun, Error> {
        crate::runtime::block_on(super::run_program(
            program, agent, oc, backend, task, context, workdir, progress, cancel,
        ))
    }
    use crate::spec::Adapter;
    use ensemble_config::BackendSettings;
    use ensemble_core::workspace::testing::TempDir;

    fn agent(tools: &[&str], max_seconds: u32) -> (RunSpec, OpenCodeAdapter) {
        let oc = OpenCodeAdapter {
            command: "opencode".into(),
            provider: "local box".into(),
            model: "qwen3.5-9b".into(),
            context_window: 65536,
            tools: tools.iter().map(|t| t.to_string()).collect(),
            max_steps: 9,
        };
        let a = RunSpec {
            name: "opencode on local box:qwen3.5-9b".into(),
            harness: ensemble_config::HarnessKind::OpenCode,
            tier: ensemble_config::Tier::Medium,
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
        a: &RunSpec,
        oc: &OpenCodeAdapter,
        dir: &Path,
    ) -> (Result<AgentRun, Error>, Vec<String>) {
        let seen = std::sync::Mutex::new(Vec::new());
        let r = run_program(
            program,
            a,
            oc,
            &local_backend(None),
            "review it",
            None,
            dir,
            &|l| seen.lock().unwrap().push(l.to_string()),
            &AtomicBool::new(false),
        );
        (r, seen.into_inner().unwrap())
    }

    fn local_backend(api_key: Option<&str>) -> LlmBackend {
        LlmBackend {
            name: "local box".into(),
            timeout_seconds: 600,
            settings: BackendSettings::OpenAiCompatible {
                base_url: "http://127.0.0.1:8888/v1".into(),
                api_key: api_key.map(String::from),
            },
        }
    }

    /// Every saved event stream in `tests/fixtures/opencode/` still parses.
    #[test]
    fn every_saved_event_stream_still_parses() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let tmp = TempDir::new();
            let script = format!("cat '{}'\n", path.display());
            let (a, cc) = agent(&["Read"], 60);
            let (r, _) = go(&fake(&tmp.0, &script), &a, &cc, &tmp.0);
            let r = r.unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert!(!r.text.is_empty(), "{}: no answer", path.display());
            assert!(!r.model.is_empty(), "{}: no model", path.display());
            checked += 1;
        }
        assert!(checked > 0, "no fixtures in {}", dir.display());
    }

    #[test]
    fn a_run_gets_its_provider_of_the_pool_as_its_opencode_provider() {
        let (a, oc) = agent(&["read"], 60);
        let b = local_backend(Some("sk-secret"));
        let cfg = config(&a, &oc, &b);
        assert_eq!(
            cfg["provider"]["ensemble-local-box"],
            json!({
                "npm": "@ai-sdk/openai-compatible",
                "name": "Ensemble provider 'local box'",
                "options": {
                    "baseURL": "http://127.0.0.1:8888/v1",
                    "timeout": 600000,
                    "apiKey": "sk-secret"
                },
                "models": {"qwen3.5-9b": {"name": "qwen3.5-9b",
                    "limit": {"context": 65536, "output": 16384}}}
            })
        );
        let args = arguments(&a, &oc, &b, "go");
        assert!(
            args.windows(2)
                .any(|w| w == ["-m", "ensemble-local-box/qwen3.5-9b"]),
            "{args:?}"
        );
    }

    #[test]
    fn a_provider_without_a_key_sends_none_and_a_shown_config_hides_one() {
        let (a, oc) = agent(&["read"], 60);
        let keyless = config(&a, &oc, &local_backend(None));
        assert!(
            keyless
                .pointer("/provider/ensemble-local-box/options/apiKey")
                .is_none()
        );

        let mut shown = config(&a, &oc, &local_backend(Some("sk-secret")));
        redact(&mut shown);
        let text = shown.to_string();
        assert!(!text.contains("sk-secret"), "{text}");
        assert!(text.contains(REDACTED), "{text}");
    }

    #[test]
    fn the_agent_is_defined_inline_with_everything_denied_but_its_tools() {
        let (a, oc) = agent(&["read", "glob", "bash(git *)", "bash(cargo test*)"], 60);
        let cfg = config(&a, &oc, &local_backend(None));
        assert!(cfg.get("instructions").is_none(), "Ensemble adds no prompt");
        let def = &cfg["agent"]["ensemble-opencode-on-local-box-qwen3-5-9b"];
        assert!(def.get("prompt").is_none(), "OpenCode keeps its own");
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

        let args = arguments(&a, &oc, &local_backend(None), "-v do it");
        assert_eq!(
            args,
            [
                "run",
                "--standalone",
                "--format",
                "json",
                "--agent",
                "ensemble-opencode-on-local-box-qwen3-5-9b",
                "-m",
                "ensemble-local-box/qwen3.5-9b",
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
            "{}case \"$OPENCODE_CONFIG_CONTENT\" in *ensemble-opencode-on-local-box-qwen3-5-9b*) ;; *) exit 9 ;; esac\n",
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
                "opencode on local box:qwen3.5-9b · starting OpenCode",
                "opencode on local box:qwen3.5-9b · step 1 · read(a.rs)",
                "opencode on local box:qwen3.5-9b · step 1 · bash(cargo test)",
                "opencode on local box:qwen3.5-9b · step 1 · bash(rm -rf /)",
                "opencode on local box:qwen3.5-9b · done",
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
            Some(&ensemble_core::workspace::Change::Added)
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
