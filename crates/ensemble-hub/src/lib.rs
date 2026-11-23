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
//! Everything the hub keeps is in its folder, `hub/` in `--config-dir`
//! (`ensemble_core::state::HubDir`): its core's database, with its settings
//! and the keys of its backends (`kept`) and its users and their tokens
//! (`users`), and a folder for each connector that keeps anything. Its
//! operations are in `accounts`. `server` is the hub's side of the link.
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
mod goals;
mod http;
pub mod inbox;
pub mod kept;
#[cfg(test)]
mod link_tests;
mod mcp;
mod openapi;
mod runners;
mod served;
pub mod server;
pub mod ui;
pub mod users;

use accounts::HubContext;
use clap::FromArgMatches;
use ensemble_core::state;
use ensemble_ops::cli::matched;
use ensemble_ops::{Error, Registry};
use std::path::PathBuf;
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
    if let Some(dir) = matches.get_one::<PathBuf>("config_dir") {
        state::use_root(dir)?;
    }
    let ctx = Ctx {
        dir: state::HubDir::this()?,
    };
    if let Some((name, leaf)) = matched(hub, matches) {
        let route = route(&ctx);
        let call = |name: &str, input| {
            route
                .as_ref()
                .map_err(|e| Error::failed(e.clone()))?
                .call(hub, name, input)
        };
        return ensemble_ops::cli::run(hub, &call, name, leaf);
    }
    let cli = cli::Cli::from_arg_matches(matches).map_err(|e| e.to_string())?;
    cmd::run(&ctx, cli.command)
}

/// Where the hub's operations run: in the running hub, over its socket, else
/// in this process. A hub that runs with no socket takes no commands.
fn route(ctx: &Ctx) -> Result<ensemble_ops::local::Route<HubContext>, String> {
    let hub_dir = ctx.dir.path();
    ensemble_ops::local::Route::to(&state::hub_socket(hub_dir), || {
        if let Some(pid) = ensemble_core::lock::holder(hub_dir) {
            return Err(format!(
                "the running hub (process {pid}) does not answer on its socket; stop it and start it again"
            ));
        }
        cmd::context(ctx)
    })
}

/// What every command is given: the global options, resolved.
struct Ctx {
    /// The hub's folder: `hub/` in `--config-dir`.
    dir: state::HubDir,
}
