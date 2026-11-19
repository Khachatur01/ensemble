//! The hub, `ensemble-hub`: the services of an agentic development system.
//! A team runs one hub on a server. Client agents reach it over MCP (`mcp`),
//! signed in with an account's token, to share to and recall from the
//! organization's vault and to make and follow tasks. Each member's runner
//! opens WebSockets to it (`ensemble_link`), signed in with a token: to lease
//! and run the tasks the hub keeps for its account (`board`), and to say it
//! is connected, with its agents and the models it serves for others
//! (`runners`).
//!
//! ```text
//!   client agents (Claude Code, OpenCode, Codex, ...)
//!         |  MCP over HTTP                  (mcp)
//!         v
//!   the hub: knowledge, tasks, accounts    (server, ensemble-knowledge, board, users)
//!         ^  the link, a WebSocket each runner opens
//!         |
//!   ensemble-runner serve                  (ensemble-runner)
//! ```
//!
//! The hub's users and their tokens are kept in `users`, and its operations
//! in `accounts`: state, not config. `server` is the hub's side of the link.
//! The same port serves a REST API (`api`, over a little `http`) for signing
//! up and for admins. Issues of the hub's tracker become tasks (`inbox`).
//!
//! The command line (`cli`), and what each command does (`cmd`), is for the
//! one who runs the hub; [`main`] is the executable's.

pub mod accounts;
mod api;
pub mod board;
pub mod calls;
mod cli;
mod cmd;
mod http;
pub mod inbox;
#[cfg(test)]
mod link_tests;
mod mcp;
mod runners;
pub mod server;
pub mod users;

use accounts::HubContext;
use clap::FromArgMatches;
use ensemble_core::state;
use ensemble_ops::cli::{is_config_extra, matched, run_config_extra};
use ensemble_ops::{Error, Registry};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The `ensemble-hub` executable.
pub fn main() -> ExitCode {
    if cfg!(windows) {
        eprintln!(
            "ensemble-hub: Windows is not supported: it takes its commands on a Unix socket. Use WSL."
        );
        return ExitCode::FAILURE;
    }
    let hub = accounts::operations();
    let matches = cli::command(&hub).get_matches();
    match dispatch(&matches, &hub) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("ensemble-hub: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Run what `matches` names: one of the hub's operations, or one of the other
/// commands.
fn dispatch(matches: &clap::ArgMatches, hub: &Registry<HubContext>) -> Result<ExitCode, String> {
    let ctx = Ctx {
        env_file: matches.get_one::<PathBuf>("env_file").cloned(),
    };
    if matched(hub, matches).is_some() || is_config_extra(matches) {
        // The running hub runs it, else this process does; a hub that runs
        // with no socket takes no commands.
        let hub_dir = state::hub_dir()?;
        let route = ensemble_ops::local::Route::to(&state::hub_socket(&hub_dir), || {
            if let Some(pid) = ensemble_core::lock::holder(&hub_dir) {
                return Err(format!(
                    "the running hub (process {pid}) takes no commands: its `interfaces.cli` is off. Turn it on in hub.yaml, or in your Nix configuration where that sets it, and start the hub again"
                ));
            }
            cmd::context(&ctx)
        });
        let call = |name: &str, input| {
            route
                .as_ref()
                .map_err(|e| Error::failed(e.clone()))?
                .call(hub, name, input)
        };
        if let Some((name, leaf)) = matched(hub, matches) {
            return ensemble_ops::cli::run(hub, &call, name, leaf);
        }
        let merge = |from: &Path| {
            let file = cmd::context(&ctx)?.into_settings();
            file.merge(from).map_err(|e| e.to_string())
        };
        if let Some(done) = run_config_extra(matches, &call, merge) {
            return done;
        }
    }
    let cli = cli::Cli::from_arg_matches(matches).map_err(|e| e.to_string())?;
    cmd::run(&ctx, cli.command)
}

/// What every command is given: the global options, resolved.
struct Ctx {
    env_file: Option<PathBuf>,
}

impl Ctx {
    /// The hub's file of `NAME=value` lines, such as API keys:
    /// `~/.config/ensemble/hub/.env`, or `--env-file`.
    fn hub_env(&self) -> Result<Option<PathBuf>, String> {
        Ok(ensemble_ops::cli::env_file(
            self.env_file.as_deref(),
            &state::hub_config_dir()?,
        ))
    }
}
