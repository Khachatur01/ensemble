//! Adapters for external agents: CLIs that are whole agents of their own,
//! started in the project with a task and read as they work.

pub mod claude_code;
pub mod gate;
pub mod llm;
pub mod opencode;
mod process;

use crate::port::Usage;
use crate::workspace::Changes;

/// What one agent run produced.
#[derive(Debug, Clone)]
pub struct AgentRun {
    pub text: String,
    pub model: String,
    pub usage: Usage,
    /// Model calls made, the final answer included.
    pub steps: u32,
    /// Tool calls the agent made, whether or not they succeeded.
    pub tool_calls: u32,
    /// Files the run changed, found by comparing the project before and after.
    pub changes: Changes,
    /// The shell commands that ran, each on one short line.
    pub commands: Vec<String>,
}

/// A tool call's input squeezed onto one short line, for progress and the
/// list of commands run.
pub(crate) fn brief(input: &str) -> String {
    let one_line = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut shown: String = one_line.chars().take(70).collect();
    if shown.len() < one_line.len() {
        shown.push('…');
    }
    shown
}
