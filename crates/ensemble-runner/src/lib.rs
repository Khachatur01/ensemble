//! The runner, `ensemble-runner`: this computer's power for the people of a
//! hub. It runs the tasks the hub keeps for its owner with the helper teams,
//! in the git repositories of its workspace, and, when it works for the hub,
//! others' tasks with its agents on model servers, and serves them its models.
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
mod relay;
pub mod remote;
pub mod setup;
pub mod teams;
mod worker;

use clap::FromArgMatches;
use cmd::runner::RunnerContext;
use ensemble_config::{self as config, Config, SystemEnv};
use ensemble_core::state;
use ensemble_ops::cli::{is_config_extra, matched, run_config_extra};
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
    let ctx = Ctx::new(
        matches.get_one::<PathBuf>("env_file").cloned(),
        matches.get_one::<PathBuf>("dir"),
    )?;
    if matched(runner, matches).is_some() || is_config_extra(matches) {
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
        let merge = |from: &Path| {
            let file = RunnerContext::new()?.into_settings();
            file.merge(from).map_err(|e| e.to_string())
        };
        if let Some(done) = run_config_extra(matches, &call, merge) {
            return done;
        }
    }
    let cli = cli::Cli::from_arg_matches(matches).map_err(|e| e.to_string())?;
    cmd::runner::run(&ctx, cli.command)
}

/// What every command is given: the global options, resolved.
struct Ctx {
    env_file: Option<PathBuf>,
    /// The folder an agent works in, for the commands that run one.
    dir: PathBuf,
}

impl Ctx {
    /// From the global options: `--env-file`, and `--dir`, or else the current
    /// directory.
    fn new(env_file: Option<PathBuf>, dir: Option<&PathBuf>) -> Result<Self, String> {
        let dir = match dir {
            Some(d) => d
                .canonicalize()
                .map_err(|e| format!("--dir {}: {e}", d.display()))?,
            None => std::env::current_dir()
                .map_err(|e| format!("cannot read the current directory: {e}"))?,
        };
        Ok(Ctx { env_file, dir })
    }

    /// The runner's file of `NAME=value` lines, such as API keys:
    /// `~/.config/ensemble/runner/.env`, or `--env-file`.
    fn runner_env(&self) -> Result<Option<PathBuf>, String> {
        Ok(ensemble_ops::cli::env_file(
            self.env_file.as_deref(),
            &state::runner_config_dir()?,
        ))
    }

    /// The runner's teams, as one helper team on its LLM backends, saying
    /// which wait for approval. Those it reaches through the hub are there
    /// only while `ensemble-runner serve` serves them; without a runner file,
    /// there are none.
    fn teams(&self) -> Result<Config, String> {
        let llm = self.runner_if_any()?.map(|r| r.llm).unwrap_or_default();
        let (config, waiting) = teams::load_all(&llm, None)?;
        for line in &waiting {
            eprintln!("ensemble: {line}");
        }
        Ok(config)
    }

    /// This computer's runner, its keys read from [`Ctx::runner_env`].
    fn runner(&self) -> Result<config::Runner, String> {
        load_this_runner(self.runner_env()?.as_deref())
    }

    /// This computer's runner, as [`Ctx::runner`], or none without a runner
    /// file.
    fn runner_if_any(&self) -> Result<Option<config::Runner>, String> {
        config::load_runner(&runner_file()?, self.runner_env()?.as_deref(), &SystemEnv)
            .map_err(|e| e.to_string())
    }
}

/// This computer's runner file, `~/.config/ensemble/runner/runner.yaml`: a
/// computer runs one runner.
fn runner_file() -> Result<PathBuf, String> {
    Ok(config::runner_path(&state::runner_config_dir()?))
}

/// This computer's runner, from its runner file, its keys read from its
/// `.env`.
fn this_runner() -> Result<config::Runner, String> {
    load_this_runner(ensemble_ops::cli::env_file(None, &state::runner_config_dir()?).as_deref())
}

/// This computer's runner, its keys read from `env_file` where the
/// environment lacks them.
fn load_this_runner(env_file: Option<&Path>) -> Result<config::Runner, String> {
    let path = runner_file()?;
    config::load_runner(&path, env_file, &SystemEnv)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "no runner file at {}; `ensemble-runner setup` or `ensemble-runner init` writes one",
                path.display()
            )
        })
}

/// The hub server this computer's runner names, and its token.
fn runner_hub(command: &str) -> Result<(String, String), String> {
    let runner = this_runner().map_err(|e| format!("{command}: {e}"))?;
    let url = runner.hub.ok_or_else(|| {
        format!(
            "{command}: this computer's runner names no hub; `ensemble-runner login <url>` names one in {}",
            runner.source.display()
        )
    })?;
    let token = ensemble_link::Tokens::load()?
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
