//! The agent loop.

use super::sandbox::Sandbox;
use super::shell;
use super::tools::{self, Change, Changes};
use crate::config::{Adapter, AgentConfig, AgentTool, ENSEMBLE_AGENT};
use crate::port::{Error, Message, ModelPort, Request, Role, Usage, complete_retrying};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const PREAMBLE: &str = "\
You are an autonomous agent working inside a project directory. Use your tools \
to look at the real files before you answer or change anything; paths are \
relative to the project root. Work until the task is done, then reply with a \
short plain-text report of what you found or changed. Do not ask the user \
questions: make reasonable assumptions and state them.";

const OUT_OF_STEPS: &str = "\
You have used all your steps. Do not call any more tools. Reply now with what \
you did, what is still unfinished, and what you would do next.";

/// What one agent run produced.
#[derive(Debug, Clone)]
pub struct AgentRun {
    pub text: String,
    pub model: String,
    /// Summed over every model call in the run.
    pub usage: Usage,
    /// Model calls made, the final answer included.
    pub steps: u32,
    /// Tool calls executed, whether or not they succeeded.
    pub tool_calls: u32,
    /// Old tool results replaced by a note to keep within `context_window`.
    pub trimmed: u32,
    /// Files the run changed: by name for `edit_file` and `write_file`, and by
    /// comparing the project before and after each `bash` command.
    pub changes: Changes,
    /// The `bash` commands that ran, each on one short line.
    pub commands: Vec<String>,
}

/// Run an agent's tool loop. `progress` hears one short line per model call and
/// per tool call, as they happen, so a slow run can be told from a hung one.
///
/// The run ends early, reporting what it did, when `cancel` is set or the
/// agent's `max_seconds` pass.
pub fn run(
    port: &dyn ModelPort,
    assignment: &AgentConfig,
    task: &str,
    context: Option<&str>,
    workdir: &Path,
    progress: &dyn Fn(&str),
    cancel: &AtomicBool,
) -> Result<AgentRun, Error> {
    let Adapter::Ensemble(adapter) = &assignment.adapter else {
        return Err(Error::Unsupported {
            backend: port.name().to_string(),
            detail: format!(
                "agent '{}' is not an {ENSEMBLE_AGENT} agent",
                assignment.name
            ),
        });
    };
    let deadline = Instant::now() + Duration::from_secs(u64::from(assignment.max_seconds));
    let stopped = || cancel.load(Ordering::Relaxed) || Instant::now() >= deadline;
    let halt = |mut out: AgentRun, last_text: &str| {
        let why = if cancel.load(Ordering::Relaxed) {
            "cancelled".to_string()
        } else {
            format!("stopped at the {}s time limit", assignment.max_seconds)
        };
        progress(&format!("{} · {why}", assignment.name));
        out.text = format!(
            "[{why} after {} step(s); the work is unfinished]",
            out.steps
        );
        if !last_text.trim().is_empty() {
            out.text.push('\n');
            out.text.push_str(last_text);
        }
        out
    };

    let sandbox = Sandbox::new(workdir).map_err(|detail| Error::Unsupported {
        backend: port.name().to_string(),
        detail: format!("agent '{}' cannot start: {detail}", assignment.name),
    })?;
    let defs = tools::definitions(&adapter.tools);
    let policy = shell::Policy {
        allow_network: adapter.network,
        readable: adapter.readable.clone(),
    };

    let mut system = PREAMBLE.to_string();
    if let Some(p) = assignment
        .prompt
        .as_deref()
        .filter(|p| !p.trim().is_empty())
    {
        system.push_str("\n\n");
        system.push_str(p);
    }
    let mut messages = vec![Message::system(system)];
    if let Some(ctx) = context.filter(|c| !c.trim().is_empty()) {
        messages.push(Message::user(format!("Context:\n{ctx}")));
    }
    messages.push(Message::user(task.to_string()));

    let mut out = AgentRun {
        text: String::new(),
        model: adapter.model.clone(),
        usage: Usage::default(),
        steps: 0,
        tool_calls: 0,
        trimmed: 0,
        changes: Changes::new(),
        commands: Vec::new(),
    };

    let Some(window) = assignment.context_window else {
        return Err(Error::Unsupported {
            backend: port.name().to_string(),
            detail: format!("agent '{}' has no context_window", assignment.name),
        });
    };
    // A quarter of the window stays free for the reply: a history that fills
    // it leaves the model nowhere to write, and the server cuts it off.
    let budget = (window - window / 4) as usize;
    let result_cap = budget / RESULT_SHARE * BYTES_PER_TOKEN;
    let defs_bytes: usize = defs
        .iter()
        .map(|d| d.name.len() + d.description.len() + d.parameters.to_string().len())
        .sum();

    let request = |messages: &[Message], offer_tools: bool| {
        let mut r = Request::new(adapter.model.clone(), messages.to_vec());
        r.temperature = adapter.temperature;
        r.presence_penalty = adapter.presence_penalty;
        r.frequency_penalty = adapter.frequency_penalty;
        r.reasoning_effort = adapter.reasoning_effort.clone();
        r.deadline = Some(deadline);
        if offer_tools {
            r.tools = defs.clone();
        }
        r
    };

    let say =
        |step: u32, what: &str| progress(&format!("{} · step {step} · {what}", assignment.name));

    let mut baseline = if adapter.tools.contains(&AgentTool::Bash) {
        tools::snapshot(&sandbox)
    } else {
        None
    };

    // What the model last said, to report if the run is stopped.
    let mut last_text = String::new();

    for step in 1..=adapter.max_steps {
        if stopped() {
            return Ok(halt(out, &last_text));
        }
        let dropped = trim_history(&mut messages, budget, defs_bytes);
        if dropped > 0 {
            say(
                step,
                &format!("dropped {dropped} old tool output(s) to fit the context"),
            );
        }
        out.trimmed += dropped;
        say(step, "waiting for the model");
        let response =
            match complete_retrying(port, &request(&messages, true), &stopped, &|l| say(step, l)) {
                Ok(r) => r,
                Err(_) if stopped() => return Ok(halt(out, &last_text)),
                Err(e) => return Err(e),
            };
        out.steps += 1;
        out.usage.prompt_tokens += response.usage.prompt_tokens;
        out.usage.completion_tokens += response.usage.completion_tokens;
        out.model = response.model;
        if !response.text.trim().is_empty() {
            last_text.clone_from(&response.text);
        }

        if response.tool_calls.is_empty() {
            say(step, "done");
            out.text = response.text;
            return Ok(out);
        }

        messages.push(Message::assistant_calling(
            response.text,
            response.tool_calls.clone(),
        ));
        for call in &response.tool_calls {
            if stopped() {
                return Ok(halt(out, &last_text));
            }
            say(step, &format!("{}({})", call.name, brief(&call.arguments)));
            out.tool_calls += 1;
            let target = tools::target(&sandbox, &call.name, &call.arguments);
            let existed = target.as_ref().is_some_and(|p| p.exists());
            let result = tools::execute(
                &sandbox,
                &adapter.tools,
                &policy,
                &call.name,
                &call.arguments,
                result_cap,
                &stopped,
            );
            if result.is_ok() {
                if let Some(path) = &target {
                    let change = if existed {
                        Change::Modified
                    } else {
                        Change::Added
                    };
                    tools::record(&mut out.changes, sandbox.show(path), change);
                }
                if call.name == tools::wire_name(AgentTool::Bash) {
                    out.commands.push(command_of(&call.arguments));
                    // What a command changed is only knowable by looking.
                    if let Some(before) = &baseline
                        && let Some(after) = tools::snapshot(&sandbox)
                    {
                        tools::diff(before, &after, &mut out.changes);
                        baseline = Some(after);
                    }
                }
            }
            let result = result.unwrap_or_else(|e| format!("error: {e}"));
            messages.push(Message::tool_result(call.id.clone(), result));
        }
    }

    // Out of steps. One last call with no tools on offer, so the work done so
    // far is reported instead of discarded.
    messages.push(Message::user(OUT_OF_STEPS));
    say(adapter.max_steps + 1, "out of steps; asking for a report");
    out.trimmed += trim_history(&mut messages, budget, defs_bytes);
    if stopped() {
        return Ok(halt(out, &last_text));
    }
    let response = match complete_retrying(port, &request(&messages, false), &stopped, &|l| {
        say(adapter.max_steps + 1, l)
    }) {
        Ok(r) => r,
        Err(_) if stopped() => return Ok(halt(out, &last_text)),
        Err(e) => return Err(e),
    };
    out.steps += 1;
    out.usage.prompt_tokens += response.usage.prompt_tokens;
    out.usage.completion_tokens += response.usage.completion_tokens;
    out.model = response.model;
    out.text = format!(
        "[stopped at the {}-step limit]\n{}",
        adapter.max_steps, response.text
    );
    Ok(out)
}

/// The command a `bash` call ran, on one short line.
fn command_of(arguments: &str) -> String {
    let command = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|v| v.get("command")?.as_str().map(str::to_string))
        .unwrap_or_default();
    brief(&command)
}

/// A tool call's arguments squeezed onto one short line.
pub(crate) fn brief(arguments: &str) -> String {
    let one_line = arguments.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut shown: String = one_line.chars().take(70).collect();
    if shown.len() < one_line.len() {
        shown.push('…');
    }
    shown
}

/// Rough bytes per token. There is no tokenizer here; this errs toward
/// trimming a little early.
const BYTES_PER_TOKEN: usize = 4;

/// One tool result may take at most this share of the context budget.
const RESULT_SHARE: usize = 8;

fn estimate_tokens(messages: &[Message], defs_bytes: usize) -> usize {
    let bytes: usize = messages
        .iter()
        .map(|m| {
            m.content.len()
                + m.tool_calls
                    .iter()
                    .map(|c| c.name.len() + c.arguments.len())
                    .sum::<usize>()
        })
        .sum();
    (bytes + defs_bytes) / BYTES_PER_TOKEN
}

/// Replace the oldest tool results with a one-line note until the history
/// fits `budget` tokens. The system prompt, the task, the model's own turns and
/// the results of its latest step are never touched, so every call keeps its
/// answer. Returns how many results were replaced.
fn trim_history(messages: &mut [Message], budget: usize, defs_bytes: usize) -> u32 {
    let protect_from = messages
        .iter()
        .rposition(|m| !m.tool_calls.is_empty())
        .unwrap_or(messages.len());
    let mut dropped = 0;
    for i in 0..protect_from {
        if estimate_tokens(messages, defs_bytes) <= budget {
            break;
        }
        if messages[i].role != Role::Tool {
            continue;
        }
        let Some(id) = messages[i].tool_call_id.as_deref() else {
            continue;
        };
        let what = messages[..i]
            .iter()
            .flat_map(|m| &m.tool_calls)
            .find(|c| c.id == id)
            .map(|c| {
                let mut args: String = c.arguments.chars().take(80).collect();
                if args.len() < c.arguments.len() {
                    args.push('…');
                }
                format!("{}({args})", c.name)
            })
            .unwrap_or_else(|| "a tool call".to_string());
        let note = format!(
            "[output of {what} removed to save context; call the tool again if you need it]"
        );
        if messages[i].content.len() <= note.len() {
            continue;
        }
        messages[i].content = note;
        dropped += 1;
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::super::sandbox::testing::TempDir;
    use super::*;
    use crate::config::{AgentTool, EnsembleAdapter};
    use crate::port::Role;
    use crate::port::testing::{FakePort, Scripted, call};

    fn assignment(tools: Vec<AgentTool>, max_steps: u32) -> AgentConfig {
        AgentConfig {
            name: "coder".into(),
            prompt: Some("Be careful.".into()),
            max_seconds: 600,
            context_window: Some(16_000),
            adapter: Adapter::Ensemble(EnsembleAdapter {
                llm_backend: "b".into(),
                model: "m".into(),
                temperature: Some(0.1),
                presence_penalty: Some(0.0),
                frequency_penalty: None,
                reasoning_effort: None,
                tools,
                max_steps,
                network: false,
                readable: Vec::new(),
            }),
        }
    }

    fn ensemble(a: &mut AgentConfig) -> &mut EnsembleAdapter {
        match &mut a.adapter {
            Adapter::Ensemble(e) => e,
            _ => unreachable!(),
        }
    }

    fn project() -> TempDir {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("a.txt"), "old\n").unwrap();
        tmp
    }

    fn go(port: &FakePort, a: &AgentConfig, dir: &Path) -> Result<AgentRun, Error> {
        run(
            port,
            a,
            "do it",
            None,
            dir,
            &|_| {},
            &AtomicBool::new(false),
        )
    }

    #[test]
    fn progress_reports_each_model_call_and_tool_call_as_it_happens() {
        let tmp = project();
        let port = FakePort::scripted(
            "b",
            "unused",
            vec![
                Scripted::call("c1", "read_file", r#"{"path":"a.txt"}"#),
                Scripted::text("ok"),
            ],
        );
        let seen = std::cell::RefCell::new(Vec::new());
        let a = assignment(vec![AgentTool::Read], 5);
        run(
            &port,
            &a,
            "do it",
            None,
            &tmp.0,
            &|line| seen.borrow_mut().push(line.to_string()),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            seen.into_inner(),
            vec![
                "coder · step 1 · waiting for the model",
                r#"coder · step 1 · read_file({"path":"a.txt"})"#,
                "coder · step 2 · waiting for the model",
                "coder · step 2 · done",
            ]
        );
    }

    #[test]
    fn a_model_that_asks_for_nothing_just_answers() {
        let tmp = project();
        let port = FakePort::ok("b", "done");
        let r = go(&port, &assignment(vec![AgentTool::Read], 5), &tmp.0).unwrap();
        assert_eq!((r.text.as_str(), r.steps, r.tool_calls), ("done", 1, 0));
        let req = port.last().unwrap();
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "read_file");
        assert_eq!(req.temperature, Some(0.1));
        assert_eq!(req.presence_penalty, Some(0.0));
        assert_eq!(req.frequency_penalty, None);
        let system = &req.messages[0];
        assert_eq!(system.role, Role::System);
        assert!(system.content.contains("autonomous agent"));
        assert!(system.content.ends_with("Be careful."));
    }

    #[test]
    fn tool_calls_run_and_their_results_go_back_to_the_model() {
        let tmp = project();
        let port = FakePort::scripted(
            "b",
            "unused",
            vec![
                Scripted::call("c1", "read_file", r#"{"path":"a.txt"}"#),
                Scripted::call(
                    "c2",
                    "edit_file",
                    r#"{"path":"a.txt","old_string":"old","new_string":"new"}"#,
                ),
                Scripted::text("changed a.txt"),
            ],
        );
        let a = assignment(vec![AgentTool::Read, AgentTool::Edit], 5);
        let r = go(&port, &a, &tmp.0).unwrap();
        assert_eq!(r.text, "changed a.txt");
        assert_eq!((r.steps, r.tool_calls), (3, 2));
        assert_eq!((r.usage.prompt_tokens, r.usage.completion_tokens), (3, 6));
        assert_eq!(
            std::fs::read_to_string(tmp.0.join("a.txt")).unwrap(),
            "new\n"
        );

        let third = port.nth(2).unwrap();
        let results: Vec<_> = third
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| (m.tool_call_id.clone().unwrap(), m.content.clone()))
            .collect();
        assert_eq!(
            results,
            vec![
                ("c1".to_string(), "old".to_string()),
                ("c2".to_string(), "edited a.txt".to_string())
            ]
        );
    }

    #[test]
    fn a_bash_call_runs_in_the_project_and_its_output_goes_back_to_the_model() {
        let tmp = project();
        let port = FakePort::scripted(
            "b",
            "x",
            vec![
                Scripted::call("c1", "bash", r#"{"command":"cat a.txt && echo extra"}"#),
                Scripted::text("done"),
            ],
        );
        let r = go(&port, &assignment(vec![AgentTool::Bash], 5), &tmp.0).unwrap();
        assert_eq!(r.text, "done");
        let result = port
            .last()
            .unwrap()
            .messages
            .iter()
            .find(|m| m.role == Role::Tool)
            .unwrap()
            .content
            .clone();
        assert_eq!(result, "old\nextra\n[exit 0]");
    }

    #[test]
    fn the_run_reports_files_it_changed_and_commands_it_ran() {
        let tmp = project();
        let port = FakePort::scripted(
            "b",
            "x",
            vec![
                Scripted::call(
                    "c1",
                    "edit_file",
                    r#"{"path":"a.txt","old_string":"old","new_string":"new"}"#,
                ),
                Scripted::call("c2", "write_file", r#"{"path":"b.txt","content":"b"}"#),
                Scripted::call("c3", "bash", r#"{"command":"echo c > c.txt"}"#),
                Scripted::call(
                    "c4",
                    "edit_file",
                    r#"{"path":"nope.txt","old_string":"a","new_string":"b"}"#,
                ),
                Scripted::text("done"),
            ],
        );
        let a = assignment(vec![AgentTool::Edit, AgentTool::Write, AgentTool::Bash], 9);
        let r = go(&port, &a, &tmp.0).unwrap();
        let changes: Vec<(&str, Change)> =
            r.changes.iter().map(|(p, c)| (p.as_str(), *c)).collect();
        assert_eq!(
            changes,
            vec![
                ("a.txt", Change::Modified),
                ("b.txt", Change::Added),
                ("c.txt", Change::Added),
            ],
            "a failed edit changes nothing"
        );
        assert_eq!(r.commands, vec!["echo c > c.txt"]);
    }

    #[test]
    fn a_failing_tool_call_is_reported_to_the_model_not_raised() {
        let tmp = project();
        let port = FakePort::scripted(
            "b",
            "recovered",
            vec![
                Scripted::call("c1", "read_file", r#"{"path":"../secret"}"#),
                Scripted::calls(vec![call("c2", "bash", "{}")]),
            ],
        );
        let r = go(&port, &assignment(vec![AgentTool::Read], 5), &tmp.0).unwrap();
        assert_eq!(r.text, "recovered");
        let last = port.last().unwrap();
        let errors: Vec<_> = last
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.content.as_str())
            .collect();
        assert!(
            errors[0].starts_with("error: ") && errors[0].contains("'..'"),
            "{errors:?}"
        );
        assert!(errors[1].contains("no tool 'bash'"), "{errors:?}");
    }

    #[test]
    fn the_step_limit_ends_with_a_tool_free_summary_call() {
        let tmp = project();
        let looping = (0..2)
            .map(|i| Scripted::call(&format!("c{i}"), "glob", r#"{"pattern":"*"}"#))
            .chain([Scripted::text("so far: nothing")])
            .collect();
        let port = FakePort::scripted("b", "x", looping);
        let r = go(&port, &assignment(vec![AgentTool::Glob], 2), &tmp.0).unwrap();
        assert!(
            r.text.starts_with("[stopped at the 2-step limit]"),
            "{}",
            r.text
        );
        assert!(r.text.ends_with("so far: nothing"));
        assert_eq!(r.steps, 3);
        let last = port.last().unwrap();
        assert!(last.tools.is_empty(), "no tools on the closing call");
    }

    #[test]
    fn a_cancelled_run_stops_before_its_next_step_and_reports_what_it_did() {
        let tmp = project();
        let cancel = AtomicBool::new(false);
        let port = FakePort::scripted(
            "b",
            "never reached",
            vec![
                Scripted::Calls(
                    "looking first".into(),
                    vec![call("c1", "read_file", r#"{"path":"a.txt"}"#)],
                ),
                Scripted::text("unused"),
            ],
        );
        let a = assignment(vec![AgentTool::Read], 5);
        let r = run(
            &port,
            &a,
            "do it",
            None,
            &tmp.0,
            &|line| {
                if line.contains("read_file") {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
        )
        .unwrap();
        assert_eq!(port.call_count(), 1, "no model call after the cancel");
        assert!(
            r.text
                .starts_with("[cancelled after 1 step(s); the work is unfinished]\nlooking first"),
            "{}",
            r.text
        );
    }

    #[test]
    fn a_run_past_its_time_limit_stops_and_says_so() {
        let tmp = project();
        let port = FakePort::ok("b", "x");
        let mut a = assignment(vec![AgentTool::Read], 5);
        a.max_seconds = 0;
        let r = go(&port, &a, &tmp.0).unwrap();
        assert_eq!(port.call_count(), 0);
        assert!(
            r.text.starts_with("[stopped at the 0s time limit"),
            "{}",
            r.text
        );
    }

    #[test]
    fn every_request_carries_the_runs_deadline() {
        let tmp = project();
        let port = FakePort::ok("b", "done");
        go(&port, &assignment(vec![AgentTool::Read], 5), &tmp.0).unwrap();
        let deadline = port.last().unwrap().deadline.expect("a deadline");
        assert!(deadline > Instant::now() + Duration::from_secs(500));
    }

    #[test]
    fn a_missing_working_directory_is_an_error_naming_the_role() {
        let port = FakePort::ok("b", "x");
        let err = go(
            &port,
            &assignment(vec![AgentTool::Read], 2),
            Path::new("/no/such/dir"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("agent 'coder'"), "{err}");
    }

    fn reads_of_a_big_file(steps: usize) -> Vec<Scripted> {
        (0..steps)
            .map(|i| Scripted::call(&format!("c{i}"), "read_file", r#"{"path":"big.txt"}"#))
            .chain([Scripted::text("done")])
            .collect()
    }

    fn big_project() -> TempDir {
        let tmp = project();
        std::fs::write(tmp.0.join("big.txt"), "x".repeat(6_000)).unwrap();
        tmp
    }

    #[test]
    fn the_reasoning_effort_reaches_every_request() {
        let tmp = project();
        let port = FakePort::ok("b", "done");
        let mut a = assignment(vec![AgentTool::Read], 5);
        ensemble(&mut a).reasoning_effort = Some("none".into());
        go(&port, &a, &tmp.0).unwrap();
        assert_eq!(
            port.last().unwrap().reasoning_effort.as_deref(),
            Some("none")
        );
    }

    #[test]
    fn under_budget_nothing_is_dropped() {
        let tmp = big_project();
        let port = FakePort::scripted("b", "x", reads_of_a_big_file(2));
        let r = go(&port, &assignment(vec![AgentTool::Read], 5), &tmp.0).unwrap();
        assert_eq!(r.trimmed, 0);
        let last = port.last().unwrap();
        assert!(last.messages.iter().all(|m| !m.content.contains("removed")));
    }

    #[test]
    fn over_budget_the_oldest_results_go_first_and_the_latest_stay() {
        let tmp = big_project();
        std::fs::write(tmp.0.join("big.txt"), "x".repeat(1_000)).unwrap();
        let port = FakePort::scripted("b", "x", reads_of_a_big_file(10));
        let mut a = assignment(vec![AgentTool::Read], 12);
        a.context_window = Some(3_000); // results are ~250 tokens each; ten overflow the 2250 left for history
        let r = go(&port, &a, &tmp.0).unwrap();
        assert!(r.trimmed >= 1, "trimmed {}", r.trimmed);

        let last = port.last().unwrap();
        let results: Vec<&Message> = last
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .collect();
        assert_eq!(results.len(), 10, "every call keeps its answer");
        assert!(results[0].content.starts_with("[output of read_file("));
        assert!(results[0].content.contains("big.txt"));
        assert_eq!(results[9].content.len(), 1_000, "the latest is intact");
        assert_eq!(last.messages[0].role, Role::System);
        assert!(last.messages[0].content.contains("autonomous agent"));
        assert_eq!(last.messages[1].content, "do it");
    }

    #[test]
    fn one_oversized_result_is_cut_to_its_share_of_the_budget() {
        let tmp = big_project();
        let port = FakePort::scripted("b", "x", reads_of_a_big_file(1));
        let mut a = assignment(vec![AgentTool::Read], 5);
        a.context_window = Some(1_000); // a result may take 1/8: 500 bytes
        go(&port, &a, &tmp.0).unwrap();
        let last = port.last().unwrap();
        let result = last.messages.iter().find(|m| m.role == Role::Tool).unwrap();
        assert!(result.content.contains("[output truncated"));
        assert!(result.content.len() < 800, "{}", result.content.len());
    }
}
