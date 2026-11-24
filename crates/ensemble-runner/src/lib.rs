//! The runner, `ensemble-runner`: this computer's power for the people of a
//! hub. It runs the tasks the hub keeps for its owner with the helper teams,
//! in the git repositories of its workspace, and serves the hub and its
//! owner's other runners the models it shares.
//!
//! ```text
//!   the hub (ensemble-hub)
//!         ^  the link, a WebSocket the runner opens (ensemble-link)
//!         |
//!   ensemble-runner serve                  (daemon, worker)
//!         |  a worker per repository of its workspace (ensemble-tasks: queue)
//!         v
//!   the router: agent -> adapter, model    (ensemble-agents: router)
//!         v
//!   claude -p, opencode run, codex exec, builtin, direct
//!                                          (ensemble-agents: external)
//! ```
//!
//! Each agent is a CLI the user already has, started headless in the
//! repository with the task, or `builtin` (a loop in this binary) or `direct`
//! (one chat request). Ensemble reads what the agent does as it works and
//! reports what it changed. The worker knows the router and nothing about the
//! CLIs; the router knows agents, and the adapters in `external` know how each
//! CLI is started and read. `ARCHITECTURE.md` is the map of the whole.
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
pub mod remote;
pub mod setup;
pub mod teams;
mod worker;

use clap::FromArgMatches;
use cmd::runner::RunnerContext;
use ensemble_config::{self as config, Config};
use ensemble_core::state;
use ensemble_ops::cli::matched;
use ensemble_ops::{Error, Registry};
use std::path::{Path, PathBuf};
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
    let ctx = Ctx::new(matches.get_one::<PathBuf>("dir"))?;
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
    cmd::runner::run(&ctx, cli.command)
}

/// What every command is given: the global options, resolved.
struct Ctx {
    /// The folder an agent works in, for the commands that run one.
    dir: PathBuf,
}

impl Ctx {
    /// From the global options: `--dir`, or else the current directory.
    fn new(dir: Option<&PathBuf>) -> Result<Self, String> {
        let dir = match dir {
            Some(d) => d
                .canonicalize()
                .map_err(|e| format!("--dir {}: {e}", d.display()))?,
            None => std::env::current_dir()
                .map_err(|e| format!("cannot read the current directory: {e}"))?,
        };
        Ok(Ctx { dir })
    }

    /// The runner's teams, as one helper team on its ANN pool, saying
    /// which wait for approval. Those it reaches through the hub are there
    /// only while `ensemble-runner serve` serves them; without runner settings,
    /// there are none.
    fn teams(&self) -> Result<Config, String> {
        let llm = self.runner_if_any()?.map(|r| r.llm).unwrap_or_default();
        let (config, waiting) = teams::load_all(&llm, None)?;
        for line in &waiting {
            eprintln!("ensemble: {line}");
        }
        Ok(config)
    }

    /// This computer's runner.
    fn runner(&self) -> Result<config::Runner, String> {
        this_runner()
    }

    /// This computer's runner, as [`Ctx::runner`], or none without runner
    /// settings.
    fn runner_if_any(&self) -> Result<Option<config::Runner>, String> {
        kept::load()
    }
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

/// The tasks of each project in this computer's runner's workspace, on its
/// hub: the project's name and its board.
fn boards(command: &str) -> Result<Vec<(String, ensemble_tasks::board::RemoteBoard)>, String> {
    let runner = this_runner().map_err(|e| format!("{command}: {e}"))?;
    let (url, token) = runner_hub(command)?;
    Ok(daemon::repositories(&runner.workspace)
        .iter()
        .filter_map(|dir| dir.file_name().map(|n| n.to_string_lossy().into_owned()))
        .map(|project| {
            let board = ensemble_tasks::board::RemoteBoard::new(&url, &token, &project);
            (project, board)
        })
        .collect())
}

/// `program` and `args` as one line a shell would read back the same way.
fn command_line(program: &str, args: &[String]) -> String {
    let quote = |s: &str| {
        let plain = !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-./:=@,+".contains(c));
        if plain {
            s.to_string()
        } else {
            format!("'{}'", s.replace('\'', r"'\''"))
        }
    };
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(quote)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `path` made absolute, with a leading `~` meaning the home directory.
fn absolute(path: &Path) -> Result<PathBuf, String> {
    let text = path.to_string_lossy();
    let expanded = match text.strip_prefix("~/") {
        Some(rest) => setup::home()
            .ok_or("cannot expand ~: HOME is not set")?
            .join(rest),
        None => path.to_path_buf(),
    };
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(std::env::current_dir()
            .map_err(|e| format!("cannot read the current directory: {e}"))?
            .join(expanded))
    }
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

/// After a team is added, changed or removed: have the running runner use it
/// now, and say what happened.
fn reload_runner() {
    let reloaded = state::runner_dir()
        .and_then(|d| ensemble_core::runtime::block_on(daemon::reload(&state::runner_socket(&d))));
    match reloaded {
        Ok(said) => println!("The runner uses it now: {said}."),
        Err(e) if e.starts_with("no runner is running") => {
            println!("No runner is running; it reads its teams when it starts.")
        }
        Err(e) => println!("{e}"),
    }
}
