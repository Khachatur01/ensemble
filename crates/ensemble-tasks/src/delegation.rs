//! One run on a task, and what it came to: the run's work with a footer of
//! what it did, or why it failed and whether another attempt could work. The
//! queue runs each task, and each retry after a failed gate, as one.

use ensemble_agents::picker::Taken;
use ensemble_agents::router::Ensemble;
use std::sync::atomic::AtomicBool;

/// What a run came to: text, plus whether it is an error.
///
/// A failed run is text too, kept as the task's result: whoever reads it
/// learns why it failed.
pub struct RunResult {
    pub text: String,
    pub is_error: bool,
}

impl RunResult {
    pub fn ok(text: impl Into<String>) -> Self {
        RunResult {
            text: text.into(),
            is_error: false,
        }
    }

    pub fn err(text: impl Into<String>) -> Self {
        RunResult {
            text: text.into(),
            is_error: true,
        }
    }
}

/// Run what `taken` set up on `task` in `dir`, and what it came to: the
/// run's work and a footer of what it did, or why it failed and whether
/// another attempt could work.
pub(crate) async fn run_delegation(
    ensemble: &Ensemble,
    taken: &Taken,
    task: &str,
    context: Option<&str>,
    dir: &std::path::Path,
    progress: &(dyn Fn(&str) + Sync),
    cancel: &AtomicBool,
) -> RunResult {
    let name = &taken.spec.name;
    match ensemble
        .run(taken, task, context, dir, progress, cancel)
        .await
    {
        Ok(out) => RunResult {
            text: format!("{}\n\n{}", out.text, footer(name, &out.stats)),
            is_error: false,
        },
        Err(e) => {
            // Saying whether another attempt could work tells a retry worth
            // making from one that fails the same way.
            let hint = if matches!(e, ensemble_core::port::Error::Timeout { .. }) {
                " This may be transient; the same task is worth one retry."
            } else if e.is_retryable() {
                " `ensemble-runner harness list` says what the runner has; retry once it is fixed."
            } else {
                " Retrying this unchanged will fail the same way."
            };
            RunResult::err(format!("the run failed ({name}): {e}{hint}"))
        }
    }
}

/// Most shell commands listed in a footer; the rest are counted.
const MAX_LISTED_COMMANDS: usize = 10;

/// What a run did, under its report: enough to review the work without
/// asking the run to describe it.
fn footer(name: &str, a: &ensemble_agents::router::AgentStats) -> String {
    let mut out = format!(
        "[{name}: {} step(s), {} tool call(s)]",
        a.steps, a.tool_calls
    );
    if !a.changes.is_empty() {
        let files: Vec<String> = a
            .changes
            .iter()
            .map(|(path, change)| format!("{path} ({})", change.as_str()))
            .collect();
        out.push_str(&format!("\n[files changed: {}]", files.join(", ")));
    }
    if !a.commands.is_empty() {
        let mut shown: Vec<&str> = a
            .commands
            .iter()
            .take(MAX_LISTED_COMMANDS)
            .map(String::as_str)
            .collect();
        let more = a.commands.len().saturating_sub(MAX_LISTED_COMMANDS);
        let more = format!("and {more} more");
        if a.commands.len() > MAX_LISTED_COMMANDS {
            shown.push(&more);
        }
        out.push_str(&format!("\n[commands run: {}]", shown.join("; ")));
    }
    out
}
