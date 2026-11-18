//! One run of an agent on a task, and what it came to: the agent's work with
//! a footer of what it did, or why it failed and whether another attempt could
//! work. The queue runs each task, and each retry after a failed gate, as one.

use ensemble_agents::router::{Ensemble, RouteError};
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

/// Run `role` on `task` in `dir`, and what it came to: the agent's work and a
/// footer of what it did, or why it failed and whether another attempt could
/// work.
pub(crate) async fn run_delegation(
    ensemble: &Ensemble,
    role: &str,
    task: &str,
    context: Option<&str>,
    dir: &std::path::Path,
    progress: &(dyn Fn(&str) + Sync),
    cancel: &AtomicBool,
) -> RunResult {
    let outcome = ensemble
        .delegate_in(role, task, context, dir, progress, cancel)
        .await;
    match outcome {
        Ok(out) => RunResult {
            text: format!("{}\n\n{}", out.text, agent_footer(role, &out.stats)),
            is_error: false,
        },
        Err(e) => {
            // Saying whether another attempt could work tells a retry worth
            // making from one against a typo'd agent name.
            let timed_out = matches!(
                &e,
                RouteError::Agent(ensemble_core::port::Error::Timeout { .. })
            );
            let hint = if timed_out {
                " This may be transient; the same task is worth one retry."
            } else if e.is_retryable() {
                " `ensemble-runner agent health` says what is wrong; retry once it is fixed."
            } else {
                " Retrying this unchanged will fail the same way."
            };
            let detail = match &e {
                RouteError::UnknownAgent { .. } | RouteError::NoBackend { .. } => String::new(),
                RouteError::Agent(_) => format!(" (agent '{role}')"),
            };
            RunResult::err(format!("delegation failed{detail}: {e}{hint}"))
        }
    }
}

/// Most shell commands listed in a footer; the rest are counted.
const MAX_LISTED_COMMANDS: usize = 10;

/// What an agent did, under its report: enough to review the work without
/// asking the agent to describe it.
fn agent_footer(role: &str, a: &ensemble_agents::router::AgentStats) -> String {
    let mut out = format!(
        "[{role}: {} step(s), {} tool call(s)]",
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
    if a.overlapped && !a.changes.is_empty() {
        out.push_str(
            "\n[another delegation ran in the project at the same time; some of these files may be its changes]",
        );
    }
    out
}

/// An agent prompt is a paragraph; a tool listing wants a line.
pub fn first_line(prompt: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_role_prompt_is_clipped_to_one_marked_line() {
        let long = "x".repeat(400);
        let out = first_line(&format!("{long}\nsecond line"));
        assert!(out.ends_with('…'), "{out}");
        assert!(out.chars().count() <= 161, "{}", out.chars().count());
    }
}
