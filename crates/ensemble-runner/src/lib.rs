//! The runner, `ensemble-runner`: this computer's power for the people of a
//! hub. It runs the tasks the hub keeps for its owner on its harnesses and
//! the models of its ANN pool, in the git repositories of its workspace, and
//! serves the hub and its owner's other runners the models it shares.
//!
//! ```text
//!   the hub (ensemble-hub)
//!         ^  the link, a WebSocket the runner opens (ensemble-link)
//!         |
//!   ensemble-runner serve                  (daemon, worker)
//!         |  a worker per repository of its workspace (ensemble-tasks: queue)
//!         v
//!   the picker: a harness and an LLM       (ensemble-agents: picker)
//!         v
//!   claude -p, opencode run, codex exec, builtin
//!                                          (ensemble-agents: external)
//! ```
//!
//! Each harness is a CLI the user already has, started headless in the
//! task's worktree, or `builtin` (a loop in this binary). Ensemble reads what
//! the run does as it works and reports what it changed. The worker knows the
//! picker and the router and nothing about the CLIs; the adapters in
//! `external` know how each CLI is started and read. `ARCHITECTURE.md` is the
//! map of the whole.
//!
//! The command line (`cli`), and what each group of commands does (`cmd`), is
//! for the person who runs this computer's runner; [`main`] is the
//! executable's. Client agents reach the hub over MCP, and run nothing of
//! Ensemble.

mod cli;
mod cmd;
mod daemon;
pub mod kept;
mod relay;
pub mod setup;
mod worker;

use clap::FromArgMatches;
use cmd::runner::RunnerContext;
use ensemble_agents::picker::Means;
use ensemble_config::{self as config, RelayAt};
use ensemble_core::state;
use ensemble_ops::cli::matched;
use ensemble_ops::{Error, Registry};
use std::path::PathBuf;
use std::process::ExitCode;

/// The `ensemble-runner` executable.
pub fn main() -> ExitCode {
    if cfg!(windows) {
        eprintln!(
            "ensemble-runner: Windows is not supported: it starts agent CLIs as process groups and runs gates with `sh -c`. Use WSL."
        );
        return ExitCode::FAILURE;
    }
    let runner = cmd::runner::operations();
    let matches = cli::command(&runner).get_matches();
    match dispatch(&matches, &runner) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("ensemble-runner: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Run what `matches` names: one of the runner's operations, or one of the
/// other commands.
fn dispatch(
    matches: &clap::ArgMatches,
    runner: &Registry<RunnerContext>,
) -> Result<ExitCode, String> {
    if let Some(dir) = matches.get_one::<PathBuf>("config_dir") {
        state::use_root(dir)?;
    }
    if matched(runner, matches).is_some() {
        // The running runner runs it, else this process does.
        let socket = state::runner_socket(&state::runner_dir()?);
        let route = ensemble_ops::local::Route::to(&socket, RunnerContext::new);
        let call = |name: &str, input| {
            route
                .as_ref()
                .map_err(|e| Error::failed(e.clone()))?
                .call(runner, name, input)
        };
        if let Some((name, leaf)) = matched(runner, matches) {
            return ensemble_ops::cli::run(runner, &call, name, leaf);
        }
    }
    let cli = cli::Cli::from_arg_matches(matches).map_err(|e| e.to_string())?;
    cmd::runner::run(cli.command)
}

/// What `runner` has to run tasks on: the harnesses a person added that are
/// found and not known to be logged out, and its ANN pool, its providers
/// reached through the hub at `relay` when it serves them there.
fn means(runner: &config::Runner, relay: Option<&RelayAt>) -> Means {
    let harnesses = runner
        .harnesses
        .each()
        .into_iter()
        .filter(|h| !h.missing && setup::logged_in(h.kind) != Some(false))
        .collect();
    Means::new(harnesses, runner.llm.clone(), runner.llm.backends(relay))
}

/// This computer's runner, from its settings, with the keys they name from
/// the environment or those it keeps.
fn this_runner() -> Result<config::Runner, String> {
    kept::load()?.ok_or_else(|| {
        "this computer has no runner yet; `ensemble-runner setup` or `ensemble-runner init` makes one".to_string()
    })
}

/// The hub server this computer's runner names, and its token.
fn runner_hub(command: &str) -> Result<(String, String), String> {
    let runner = this_runner().map_err(|e| format!("{command}: {e}"))?;
    let url = runner.hub.ok_or_else(|| {
        format!(
            "{command}: this computer's runner names no hub; `ensemble-runner login <url>` names one"
        )
    })?;
    let token = kept::Tokens::load()?
        .get(&url)
        .ok_or_else(|| {
            format!("{command}: no token for the hub {url}. Sign in first: ensemble-runner login {url} --user <name>, or ensemble-runner join {url} <code>")
        })?
        .to_string();
    Ok((url, token))
}

/// This computer's name, offered as the runner's name when setup asks, and
/// the name of its token when it signs in to a hub before it has a runner.
fn host_name() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .ok()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
}
