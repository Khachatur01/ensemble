//! The Claude Code CLI adapter.
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
//! NO TOOL CALLING, DECLARED RATHER THAN DISCOVERED
//! ------------------------------------------------
//! `claude -p` emits Claude Code's *own* tool_use blocks, which Claude Code
//! executes itself against the filesystem, and returns prose. It never returns
//! an OpenAI `tool_calls` array against a schema the caller supplied. So
//! `capabilities().tool_calling` is `false`, and a [`Request`] that offers
//! tools is refused outright rather than answered as though none were offered.
//!
//! A role on this backend still does real work, using Claude Code's own tools,
//! and reports back in prose. What it cannot do is run Ensemble's agent loop
//! (`tools:` in the config), which needs a schema this process supplies;
//! `config.rs` refuses that combination at load time.

use crate::config::Backend;
use crate::port::{Capabilities, Error, ModelPort, Request, Response, Role, Usage};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// How long to wait for the output readers after the child has exited.
///
/// Reaching this means something still holds the pipe's write end — a
/// grandchild the backend left running. The answer is already lost at that
/// point; blocking for it would turn a finished call into a hung one.
const READER_GRACE: Duration = Duration::from_secs(10);

/// The same, after a timeout kill. Shorter, because nothing here is wanted:
/// the call already failed and the only question is how fast we say so.
const READER_GRACE_AFTER_KILL: Duration = Duration::from_secs(2);

/// Kill the child's whole process group.
///
/// `Child::kill` signals one pid. `claude` is a node wrapper that spawns
/// children, and a shell script spawns whatever it runs; killing only the
/// leader leaves those orphans alive, still holding the stdout pipe — which
/// made a 2-second timeout take as long as the orphan felt like, and left a
/// model call running that nobody was waiting for. The child is put in its own
/// group at spawn so the group id is its pid and this cannot reach anything
/// else.
#[cfg(unix)]
fn kill_group(pid: u32) {
    // SAFETY: `killpg` is a plain libc call with no memory arguments. The pgid
    // is this child's own, established by `process_group(0)` at spawn. A
    // failure (the group is already gone) is reported through errno, which is
    // exactly the case we do not care about.
    unsafe {
        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: u32) {}

/// The model ids a role may name, and the `claude --model` alias each means.
const MODEL_ALIASES: &[(&str, &str)] = &[
    ("claude-opus", "opus"),
    ("claude-sonnet", "sonnet"),
    ("claude-haiku", "haiku"),
];

/// An unknown model id is an error, not a default.
///
/// The obvious alternative — fall back to sonnet — answers a request for Opus
/// with something else and nothing in the transcript says so. A role pinned to a
/// model it does not get is exactly the silent failure this project exists to
/// make loud.
pub fn resolve_alias(model: &str) -> Result<&'static str, String> {
    // Tolerate a `provider/model` spelling, which is how harnesses name models.
    let tail = model.rsplit('/').next().unwrap_or(model).trim();
    MODEL_ALIASES
        .iter()
        .find(|(id, _)| *id == tail)
        .map(|(_, alias)| *alias)
        .ok_or_else(|| {
            let known: Vec<&str> = MODEL_ALIASES.iter().map(|(id, _)| *id).collect();
            format!(
                "unknown model '{model}'. This backend serves: {}.",
                known.join(", ")
            )
        })
}

/// Collapse a message list into the one prompt and one system prompt that
/// `claude -p` accepts.
///
/// Earlier assistant turns are folded into the prompt as labelled context
/// rather than dropped: the CLI is stateless per invocation, so anything not
/// put in the prompt is simply forgotten.
pub fn flatten(request: &Request) -> (String, String) {
    let system = request.system_text();
    let mut convo = Vec::new();
    for m in &request.messages {
        if m.content.trim().is_empty() {
            continue;
        }
        match m.role {
            Role::System => {}
            Role::Assistant => convo.push(format!("[previous assistant reply]\n{}", m.content)),
            Role::User => convo.push(m.content.clone()),
            // Unreachable in practice — `complete` refuses a request that
            // offers tools, and nothing else produces a tool turn. Rendered
            // rather than skipped anyway, so a future caller that does build
            // one loses nothing silently.
            Role::Tool => convo.push(format!("[tool result]\n{}", m.content)),
        }
    }
    (system, convo.join("\n\n"))
}

/// How the CLI is invoked, less the prompt. Comes straight from the backend's
/// config block; grouped so the argv builder takes a subject rather than a
/// parameter list nobody can read at the call site.
pub struct Invocation<'a> {
    pub command: &'a str,
    /// The `--model` alias, already resolved from the config's model id.
    pub alias: &'a str,
    pub allowed_tools: &'a str,
    pub permission_mode: &'a str,
    pub max_turns: u32,
}

/// Build the argv.
///
/// Never `--bare`: it bypasses the saved claude.ai login and demands an API key,
/// which bills per token instead of using the subscription.
pub fn argv(inv: &Invocation<'_>, system: &str, user: &str, workdir: Option<&str>) -> Vec<String> {
    let mut v: Vec<String> = vec![
        inv.command.to_string(),
        "-p".into(),
        user.to_string(),
        "--output-format".into(),
        "json".into(),
        "--model".into(),
        inv.alias.to_string(),
        "--permission-mode".into(),
        inv.permission_mode.to_string(),
        "--max-turns".into(),
        inv.max_turns.to_string(),
    ];
    if !inv.allowed_tools.trim().is_empty() {
        v.push("--allowedTools".into());
        v.push(inv.allowed_tools.to_string());
    }
    if !system.trim().is_empty() {
        v.push("--append-system-prompt".into());
        v.push(system.to_string());
    }
    if let Some(dir) = workdir.filter(|d| !d.trim().is_empty()) {
        v.push("--add-dir".into());
        v.push(dir.to_string());
    }
    v
}

/// Read `claude -p --output-format json`.
///
/// The `modelUsage` check guards against a stray `ANTHROPIC_BASE_URL` silently
/// redirecting the spend: if something that is not Claude answered, this is not
/// the thing the caller asked for, and a wrong answer is worse than no answer.
pub fn parse_reply(stdout: &str, backend: &str) -> Result<Response, Error> {
    let payload: serde_json::Value =
        serde_json::from_str(stdout.trim()).map_err(|e| Error::Protocol {
            backend: backend.into(),
            detail: format!("claude returned non-JSON ({e}): {stdout}"),
        })?;

    let text = payload
        .get("result")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            // `is_error: true` with a string result is the CLI's own failure
            // reporting; no `.result` at all is a different, structural problem.
            Error::Protocol {
                backend: backend.into(),
                detail: format!("claude returned no string `.result`: {stdout}"),
            }
        })?;

    let used: Vec<&str> = payload
        .get("modelUsage")
        .and_then(|v| v.as_object())
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default();
    if !used.is_empty() && !used.iter().any(|m| m.contains("claude")) {
        return Err(Error::Protocol {
            backend: backend.into(),
            detail: format!(
                "a non-Claude model ('{}') answered — refusing. Check ANTHROPIC_BASE_URL.",
                used.join(", ")
            ),
        });
    }

    let usage = payload.get("usage");
    let num = |k: &str| {
        usage
            .and_then(|u| u.get(k))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32
    };

    Ok(Response {
        text: text.to_string(),
        // This backend never produces them; see the module note.
        tool_calls: Vec::new(),
        model: used
            .first()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "claude".into()),
        usage: Usage {
            prompt_tokens: num("input_tokens"),
            completion_tokens: num("output_tokens"),
        },
        finish_reason: Some("stop".into()),
    })
}

// ---------------------------------------------------------------------------
// Running the subprocess
// ---------------------------------------------------------------------------

pub struct Output {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// Spawning is injected so the whole request path above is testable without a
/// subscription, a network, or a `claude` binary on PATH.
pub trait Runner: Send + Sync {
    fn run(&self, argv: &[String], cwd: Option<&str>, timeout: Duration) -> Result<Output, String>;
}

pub struct ProcessRunner;

impl Runner for ProcessRunner {
    fn run(&self, argv: &[String], cwd: Option<&str>, timeout: Duration) -> Result<Output, String> {
        let (program, args) = argv.split_first().ok_or("empty argv")?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = cwd.filter(|d| !d.is_empty()) {
            cmd.current_dir(dir);
        }
        // Its own process group, so a timeout can take the whole tree down
        // rather than just the leader. See `kill_group`.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let pid = child.id();

        // Take the pipes before waiting. A child that fills a 64 KiB pipe buffer
        // while nothing is reading blocks forever, and the timeout below would
        // then be the only thing that ever ends the call — turning every long
        // answer into a spurious timeout.
        let mut out = child.stdout.take().ok_or("no stdout pipe")?;
        let mut err = child.stderr.take().ok_or("no stderr pipe")?;
        let (tx_o, rx_o) = mpsc::channel();
        let (tx_e, rx_e) = mpsc::channel();
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = out.read_to_string(&mut s);
            let _ = tx_o.send(s);
        });
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            let _ = tx_e.send(s);
        });

        // std has no `wait_timeout`, so poll. 50 ms costs nothing against calls
        // that take seconds to minutes and keeps the deadline honest.
        let deadline = std::time::Instant::now() + timeout;
        let status = loop {
            match child.try_wait().map_err(|e| e.to_string())? {
                Some(s) => break Some(s),
                None if std::time::Instant::now() >= deadline => {
                    kill_group(pid);
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        };

        // Collect with a deadline rather than blocking.
        //
        // The readers end when the pipe's write end closes, and the child
        // exiting is not enough to guarantee that: anything it left running
        // still holds a copy. `kill_group` is what normally closes it, but a
        // process in uninterruptible sleep can outlive even SIGKILL for a
        // while, and waiting indefinitely for output nobody will read is how a
        // bounded timeout becomes an unbounded one.
        let grace = if status.is_some() {
            READER_GRACE
        } else {
            READER_GRACE_AFTER_KILL
        };
        let stdout = rx_o.recv_timeout(grace).unwrap_or_default();
        let stderr = rx_e.recv_timeout(grace).unwrap_or_default();

        Ok(match status {
            Some(s) => Output {
                status: s.code(),
                stdout,
                stderr,
                timed_out: false,
            },
            None => Output {
                status: None,
                stdout,
                stderr,
                timed_out: true,
            },
        })
    }
}

pub struct ClaudeCodeAdapter {
    name: String,
    command: String,
    allowed_tools: String,
    permission_mode: String,
    max_turns: u32,
    timeout: Duration,
    /// Where `--add-dir` points when a request does not say. The harness's
    /// project directory, captured at startup.
    default_workdir: Option<PathBuf>,
    runner: Box<dyn Runner>,
}

impl ClaudeCodeAdapter {
    pub fn new(
        backend: &Backend,
        default_workdir: Option<PathBuf>,
        runner: Box<dyn Runner>,
    ) -> Result<Self, Error> {
        let crate::config::BackendSettings::ClaudeCodeCli {
            command,
            allowed_tools,
            permission_mode,
            max_turns,
        } = &backend.settings
        else {
            return Err(Error::Unsupported {
                backend: backend.name.clone(),
                detail: "not a claude-code-cli backend".into(),
            });
        };
        Ok(ClaudeCodeAdapter {
            name: backend.name.clone(),
            command: command.clone(),
            allowed_tools: allowed_tools.clone(),
            permission_mode: permission_mode.clone(),
            max_turns: *max_turns,
            timeout: Duration::from_secs(backend.timeout_seconds),
            default_workdir,
            runner,
        })
    }
}

impl ModelPort for ClaudeCodeAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn type_id(&self) -> &'static str {
        crate::config::CLAUDE_CODE_CLI
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            tool_calling: false,
        }
    }

    fn complete(&self, request: &Request) -> Result<Response, Error> {
        // Refused, not ignored. Answering in prose while dropping the schema
        // would leave the caller believing its tools were offered. See the note
        // on `port::Request`.
        if request.offers_tools() {
            return Err(Error::Unsupported {
                backend: self.name.clone(),
                detail: format!(
                    "{} tool(s) were offered and this backend cannot call them: `claude -p` runs \
                     Claude Code's own tools and never returns a tool_calls array against a \
                     schema we supplied. Roles with `tools:` need an openai-compatible-api \
                     backend; this one already has Claude Code's own tools.",
                    request.tools.len()
                ),
            });
        }

        let alias = resolve_alias(&request.model).map_err(|detail| Error::Unsupported {
            backend: self.name.clone(),
            detail,
        })?;

        let (system, user) = flatten(request);
        if user.trim().is_empty() {
            return Err(Error::Unsupported {
                backend: self.name.clone(),
                detail: "nothing to ask: the request has no user content".into(),
            });
        }

        let workdir = request.workdir.clone().or_else(|| {
            self.default_workdir
                .as_ref()
                .map(|p| p.display().to_string())
        });

        let argv = argv(
            &Invocation {
                command: &self.command,
                alias,
                allowed_tools: &self.allowed_tools,
                permission_mode: &self.permission_mode,
                max_turns: self.max_turns,
            },
            &system,
            &user,
            workdir.as_deref(),
        );

        let out = self
            .runner
            .run(&argv, workdir.as_deref(), self.timeout)
            .map_err(|detail| Error::Spawn {
                backend: self.name.clone(),
                program: self.command.clone(),
                detail,
            })?;

        if out.timed_out {
            return Err(Error::Timeout {
                backend: self.name.clone(),
                seconds: self.timeout.as_secs(),
            });
        }
        if out.status != Some(0) {
            return Err(Error::ProcessFailed {
                backend: self.name.clone(),
                code: out.status,
                // stderr first, but fall back to stdout: the CLI reports some
                // failures as JSON on stdout with a non-zero exit, and an empty
                // error message is the least useful thing we could return.
                stderr: if out.stderr.trim().is_empty() {
                    out.stdout
                } else {
                    out.stderr
                },
            });
        }

        parse_reply(&out.stdout, &self.name)
    }

    fn health(&self) -> Result<(), Error> {
        // `--version` does not touch the subscription, which is the whole
        // requirement for a health check here.
        let out = self
            .runner
            .run(
                &[self.command.clone(), "--version".into()],
                None,
                Duration::from_secs(20),
            )
            .map_err(|detail| Error::Spawn {
                backend: self.name.clone(),
                program: self.command.clone(),
                detail,
            })?;
        if out.timed_out {
            return Err(Error::Timeout {
                backend: self.name.clone(),
                seconds: 20,
            });
        }
        if out.status != Some(0) {
            return Err(Error::ProcessFailed {
                backend: self.name.clone(),
                code: out.status,
                stderr: out.stderr,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tool_calling_tests {
    use super::*;
    use crate::config::{Backend, BackendSettings};
    use crate::port::{Message, ToolDef};

    fn backend() -> Backend {
        Backend {
            name: "claude".into(),
            timeout_seconds: 60,
            settings: BackendSettings::ClaudeCodeCli {
                command: "claude".into(),
                allowed_tools: "Read".into(),
                permission_mode: "dontAsk".into(),
                max_turns: 3,
            },
        }
    }

    #[test]
    fn a_request_that_offers_tools_is_refused_rather_than_half_honoured() {
        // Answering in prose while dropping the schema would leave the caller
        // believing its tools had been offered.
        struct NeverRuns;
        impl Runner for NeverRuns {
            fn run(&self, _: &[String], _: Option<&str>, _: Duration) -> Result<Output, String> {
                panic!("the subprocess must not be started for a refused request");
            }
        }
        let a = ClaudeCodeAdapter::new(&backend(), None, Box::new(NeverRuns)).unwrap();

        let mut req = Request::new("claude-sonnet", vec![Message::user("go")]);
        req.tools = vec![ToolDef {
            name: "read".into(),
            description: "Read a file.".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let err = a.complete(&req).unwrap_err();
        assert!(matches!(err, Error::Unsupported { .. }), "{err}");
        assert!(err.to_string().contains("openai-compatible-api"), "{err}");
        assert!(!err.is_retryable(), "the same request fails the same way");
    }

    #[test]
    fn a_tool_result_turn_is_rendered_rather_than_dropped() {
        let req = Request::new(
            "claude-sonnet",
            vec![
                Message::user("what is in it?"),
                Message::tool_result("call_abc", "the file says ok"),
            ],
        );
        let (_, user) = flatten(&req);
        assert!(user.contains("the file says ok"), "{user}");
        assert!(user.contains("[tool result]"), "{user}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Backend, BackendSettings};
    use crate::port::Message;
    use std::sync::{Arc, Mutex};

    struct ScriptedRunner {
        output: Mutex<Result<Output, String>>,
        seen: Mutex<Vec<Vec<String>>>,
        cwds: Mutex<Vec<Option<String>>>,
    }

    impl ScriptedRunner {
        fn returning(stdout: &str) -> Arc<Self> {
            Arc::new(ScriptedRunner {
                output: Mutex::new(Ok(Output {
                    status: Some(0),
                    stdout: stdout.into(),
                    stderr: String::new(),
                    timed_out: false,
                })),
                seen: Mutex::new(Vec::new()),
                cwds: Mutex::new(Vec::new()),
            })
        }
        fn with(out: Output) -> Arc<Self> {
            Arc::new(ScriptedRunner {
                output: Mutex::new(Ok(out)),
                seen: Mutex::new(Vec::new()),
                cwds: Mutex::new(Vec::new()),
            })
        }
        fn unspawnable() -> Arc<Self> {
            Arc::new(ScriptedRunner {
                output: Mutex::new(Err("No such file or directory (os error 2)".into())),
                seen: Mutex::new(Vec::new()),
                cwds: Mutex::new(Vec::new()),
            })
        }
        fn argv(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .last()
                .cloned()
                .unwrap_or_default()
        }
    }

    impl Runner for Arc<ScriptedRunner> {
        fn run(&self, argv: &[String], cwd: Option<&str>, _t: Duration) -> Result<Output, String> {
            self.seen.lock().unwrap().push(argv.to_vec());
            self.cwds.lock().unwrap().push(cwd.map(str::to_string));
            match &*self.output.lock().unwrap() {
                Ok(o) => Ok(Output {
                    status: o.status,
                    stdout: o.stdout.clone(),
                    stderr: o.stderr.clone(),
                    timed_out: o.timed_out,
                }),
                Err(e) => Err(e.clone()),
            }
        }
    }

    fn backend() -> Backend {
        Backend {
            name: "claude".into(),
            timeout_seconds: 30,
            settings: BackendSettings::ClaudeCodeCli {
                command: "claude".into(),
                allowed_tools: "Read,Grep,Glob".into(),
                permission_mode: "dontAsk".into(),
                max_turns: 12,
            },
        }
    }

    fn inv<'a>(alias: &'a str, allowed_tools: &'a str) -> Invocation<'a> {
        Invocation {
            command: "claude",
            alias,
            allowed_tools,
            permission_mode: "dontAsk",
            max_turns: 12,
        }
    }

    const GOOD: &str = r#"{"result":"ensemble-real-ok","modelUsage":{"claude-sonnet-5":{}},
        "usage":{"input_tokens":11,"output_tokens":4},"total_cost_usd":0.01}"#;

    #[test]
    fn model_aliases_resolve_and_unknown_ones_are_refused() {
        assert_eq!(resolve_alias("claude-sonnet").unwrap(), "sonnet");
        assert_eq!(resolve_alias("claude-opus").unwrap(), "opus");
        // A provider-qualified spelling is the same model.
        assert_eq!(resolve_alias("claude/claude-haiku").unwrap(), "haiku");
        let err = resolve_alias("gpt-4o").unwrap_err();
        assert!(
            err.contains("gpt-4o") && err.contains("claude-sonnet"),
            "{err}"
        );
    }

    #[test]
    fn argv_never_contains_bare() {
        // --bare bypasses the saved login and bills per token. Guarded by a
        // test because it is a one-word change with a billing consequence.
        let v = argv(&inv("sonnet", "Read"), "sys", "hi", Some("/w"));
        assert!(!v.iter().any(|a| a == "--bare"), "{v:?}");
    }

    #[test]
    fn argv_carries_the_model_prompt_system_and_directory() {
        let mut i = inv("opus", "Read,Grep");
        i.max_turns = 7;
        let v = argv(&i, "SYS", "DO IT", Some("/proj"));
        let joined = v.join(" ");
        assert!(joined.contains("-p DO IT"), "{joined}");
        assert!(joined.contains("--model opus"), "{joined}");
        assert!(joined.contains("--append-system-prompt SYS"), "{joined}");
        assert!(joined.contains("--allowedTools Read,Grep"), "{joined}");
        assert!(joined.contains("--max-turns 7"), "{joined}");
        assert!(joined.contains("--add-dir /proj"), "{joined}");
        assert!(joined.contains("--output-format json"), "{joined}");
    }

    #[test]
    fn empty_optional_arguments_are_omitted_rather_than_passed_blank() {
        let v = argv(&inv("sonnet", ""), "", "hi", None);
        assert!(!v.iter().any(|a| a == "--append-system-prompt"), "{v:?}");
        assert!(!v.iter().any(|a| a == "--allowedTools"), "{v:?}");
        assert!(!v.iter().any(|a| a == "--add-dir"), "{v:?}");
    }

    #[test]
    fn earlier_turns_are_folded_into_the_prompt_not_dropped() {
        let req = Request::new(
            "claude-sonnet",
            vec![
                Message::system("be terse"),
                Message::user("first question"),
                Message::assistant("first answer"),
                Message::user("follow up"),
            ],
        );
        let (system, user) = flatten(&req);
        assert_eq!(system, "be terse");
        assert!(user.contains("first question"), "{user}");
        assert!(
            user.contains("[previous assistant reply]\nfirst answer"),
            "{user}"
        );
        assert!(user.contains("follow up"), "{user}");
    }

    #[test]
    fn a_good_reply_is_parsed_with_its_usage() {
        let r = parse_reply(GOOD, "claude").unwrap();
        assert_eq!(r.text, "ensemble-real-ok");
        assert_eq!(r.usage.prompt_tokens, 11);
        assert_eq!(r.usage.completion_tokens, 4);
        assert_eq!(r.usage.total(), 15);
        assert_eq!(r.model, "claude-sonnet-5");
    }

    #[test]
    fn a_non_claude_model_answering_is_refused() {
        // A stray ANTHROPIC_BASE_URL redirecting the spend would otherwise be
        // completely invisible.
        let body = r#"{"result":"hi","modelUsage":{"some-proxy-model":{}}}"#;
        let err = parse_reply(body, "claude").unwrap_err();
        assert!(matches!(err, Error::Protocol { .. }), "{err}");
        assert!(err.to_string().contains("ANTHROPIC_BASE_URL"), "{err}");
    }

    #[test]
    fn non_json_and_missing_result_are_protocol_errors() {
        assert!(matches!(
            parse_reply("not json at all", "claude").unwrap_err(),
            Error::Protocol { .. }
        ));
        assert!(matches!(
            parse_reply(r#"{"session_id":"x"}"#, "claude").unwrap_err(),
            Error::Protocol { .. }
        ));
    }

    #[test]
    fn a_full_call_round_trips_through_the_adapter() {
        let a = ClaudeCodeAdapter::new(
            &backend(),
            Some("/proj".into()),
            Box::new(ScriptedRunner::returning(GOOD)),
        )
        .unwrap();
        let req = Request::new("claude-sonnet", vec![Message::user("hello")]);
        let r = a.complete(&req).unwrap();
        assert_eq!(r.text, "ensemble-real-ok");
        assert!(!a.capabilities().tool_calling, "declared, not discovered");
    }

    #[test]
    fn the_default_workdir_is_used_when_the_request_has_none() {
        let runner = ScriptedRunner::returning(GOOD);
        let a = ClaudeCodeAdapter::new(&backend(), Some("/proj".into()), Box::new(runner.clone()))
            .unwrap();
        a.complete(&Request::new("claude-sonnet", vec![Message::user("hi")]))
            .unwrap();
        assert!(
            runner.argv().join(" ").contains("--add-dir /proj"),
            "{:?}",
            runner.argv()
        );
        // The subprocess is also *started* there, so relative paths in the
        // prompt mean what the caller meant.
        assert_eq!(runner.cwds.lock().unwrap()[0].as_deref(), Some("/proj"));
    }

    #[test]
    fn a_request_workdir_overrides_the_backend_default() {
        let runner = ScriptedRunner::returning(GOOD);
        let a = ClaudeCodeAdapter::new(&backend(), Some("/proj".into()), Box::new(runner.clone()))
            .unwrap();
        let mut req = Request::new("claude-sonnet", vec![Message::user("hi")]);
        req.workdir = Some("/elsewhere".into());
        a.complete(&req).unwrap();
        assert!(
            runner.argv().join(" ").contains("--add-dir /elsewhere"),
            "{:?}",
            runner.argv()
        );
    }

    #[test]
    fn a_timeout_is_reported_as_a_timeout_and_is_retryable() {
        let out = Output {
            status: None,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
        };
        let a =
            ClaudeCodeAdapter::new(&backend(), None, Box::new(ScriptedRunner::with(out))).unwrap();
        let err = a
            .complete(&Request::new("claude-sonnet", vec![Message::user("hi")]))
            .unwrap_err();
        assert!(matches!(err, Error::Timeout { .. }), "{err}");
        assert!(err.is_retryable());
        assert!(
            err.to_string().contains("timeout_seconds"),
            "says the knob: {err}"
        );
    }

    #[test]
    fn a_non_zero_exit_carries_stderr() {
        let out = Output {
            status: Some(2),
            stdout: String::new(),
            stderr: "Invalid API key".into(),
            timed_out: false,
        };
        let a =
            ClaudeCodeAdapter::new(&backend(), None, Box::new(ScriptedRunner::with(out))).unwrap();
        let err = a
            .complete(&Request::new("claude-sonnet", vec![Message::user("hi")]))
            .unwrap_err();
        assert!(
            matches!(err, Error::ProcessFailed { code: Some(2), .. }),
            "{err}"
        );
        assert!(err.to_string().contains("Invalid API key"), "{err}");
        assert!(!err.is_retryable());
    }

    #[test]
    fn a_non_zero_exit_with_only_stdout_still_reports_something_useful() {
        let out = Output {
            status: Some(1),
            stdout: r#"{"is_error":true,"result":"rate limited"}"#.into(),
            stderr: "   ".into(),
            timed_out: false,
        };
        let a =
            ClaudeCodeAdapter::new(&backend(), None, Box::new(ScriptedRunner::with(out))).unwrap();
        let err = a
            .complete(&Request::new("claude-sonnet", vec![Message::user("hi")]))
            .unwrap_err();
        assert!(err.to_string().contains("rate limited"), "{err}");
    }

    #[test]
    fn a_missing_binary_says_what_is_missing() {
        let a = ClaudeCodeAdapter::new(&backend(), None, Box::new(ScriptedRunner::unspawnable()))
            .unwrap();
        let err = a
            .complete(&Request::new("claude-sonnet", vec![Message::user("hi")]))
            .unwrap_err();
        assert!(matches!(err, Error::Spawn { .. }), "{err}");
        assert!(
            err.to_string().contains("claude") && err.to_string().contains("PATH"),
            "{err}"
        );
    }

    #[test]
    fn an_unknown_model_fails_before_anything_is_spawned() {
        let runner = ScriptedRunner::returning(GOOD);
        let a = ClaudeCodeAdapter::new(&backend(), None, Box::new(runner.clone())).unwrap();
        let err = a
            .complete(&Request::new("qwen3-coder", vec![Message::user("hi")]))
            .unwrap_err();
        assert!(matches!(err, Error::Unsupported { .. }), "{err}");
        assert!(runner.seen.lock().unwrap().is_empty(), "must not spawn");
    }

    #[test]
    fn an_empty_prompt_is_refused() {
        let a = ClaudeCodeAdapter::new(&backend(), None, Box::new(ScriptedRunner::returning(GOOD)))
            .unwrap();
        let err = a
            .complete(&Request::new(
                "claude-sonnet",
                vec![Message::system("only a system prompt")],
            ))
            .unwrap_err();
        assert!(err.to_string().contains("nothing to ask"), "{err}");
    }
}
