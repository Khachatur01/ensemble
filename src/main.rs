//! The `ensemble` binary: the command line (`cli`), the commands made from the
//! hub's and the runner's operations (`ops_cli`), and what each other group of
//! commands does (`cmd`). Every command is for the person who runs this
//! computer's runner, or a hub; client agents reach a hub over MCP, and run
//! nothing of Ensemble.

use clap::FromArgMatches;
use ensemble::config::{self, Config, SystemEnv};
use ensemble::ops::Error;
use ensemble::teams;
use ensemble::{setup, state};

mod cli;
mod cmd;
mod daemon;
mod local;
mod ops_cli;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    if cfg!(windows) {
        eprintln!(
            "ensemble: Windows is not supported: it starts agent CLIs as process groups and runs gates with `sh -c`. Use WSL."
        );
        return ExitCode::FAILURE;
    }
    let hub = ensemble::link::accounts::operations();
    let runner = cmd::runner::operations();
    let matches = cli::command(&hub, &runner).get_matches();
    match dispatch(&matches, &hub, &runner) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("ensemble: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Run what `matches` names: one of the hub's or the runner's operations, or
/// one of the other commands.
fn dispatch(
    matches: &clap::ArgMatches,
    hub: &ensemble::ops::Registry<ensemble::link::accounts::HubContext>,
    runner: &ensemble::ops::Registry<cmd::runner::RunnerContext>,
) -> Result<ExitCode, String> {
    let ctx = Ctx::new(
        matches.get_one::<PathBuf>("env_file").cloned(),
        matches.get_one::<PathBuf>("dir"),
    )?;
    match matches.subcommand() {
        Some(("hub", under)) if ops_cli::matched(hub, under).is_some() || is_extra(under) => {
            // The running hub runs it, else this process does; a hub that
            // runs with no socket takes no commands.
            let hub_dir = state::hub_dir()?;
            let route = local::Route::to(&state::hub_socket(&hub_dir), || {
                if let Some(pid) = ensemble::lock::holder(&hub_dir) {
                    return Err(format!(
                        "the running hub (process {pid}) takes no commands: its `interfaces.cli` is off. Turn it on in hub.yaml, or in your Nix configuration where that sets it, and start the hub again"
                    ));
                }
                cmd::hub::context(&ctx)
            });
            let call = |name: &str, input| {
                route
                    .as_ref()
                    .map_err(|e| Error::failed(e.clone()))?
                    .call(hub, name, input)
            };
            if let Some((name, leaf)) = ops_cli::matched(hub, under) {
                return ops_cli::run(hub, &call, name, leaf);
            }
            let file = || cmd::hub::context(&ctx).map(|c| c.into_settings());
            if let Some(done) = cmd::config::run_extra(under, &call, file) {
                return done;
            }
        }
        Some(("runner", under)) if ops_cli::matched(runner, under).is_some() || is_extra(under) => {
            let socket = state::runner_socket(&state::runner_dir()?);
            let route = local::Route::to(&socket, cmd::runner::RunnerContext::new);
            let call = |name: &str, input| {
                route
                    .as_ref()
                    .map_err(|e| Error::failed(e.clone()))?
                    .call(runner, name, input)
            };
            if let Some((name, leaf)) = ops_cli::matched(runner, under) {
                return ops_cli::run(runner, &call, name, leaf);
            }
            let file = || cmd::runner::RunnerContext::new().map(|c| c.into_settings());
            if let Some(done) = cmd::config::run_extra(under, &call, file) {
                return done;
            }
        }
        _ => {}
    }
    let cli = cli::Cli::from_arg_matches(matches).map_err(|e| e.to_string())?;
    run(&ctx, cli, hub, runner)
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

    /// The file of `NAME=value` lines read for the variables the environment
    /// does not set, such as API keys: `--env-file` when it is given, or else
    /// `.env` in `dir` when there is one.
    fn env_file_in(&self, dir: PathBuf) -> Option<PathBuf> {
        if let Some(file) = &self.env_file {
            return Some(file.clone());
        }
        let file = dir.join(state::ENV_FILE);
        if !file.is_file() {
            return None;
        }
        #[cfg(unix)]
        if let Ok(meta) = std::fs::metadata(&file) {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o077 != 0 {
                eprintln!(
                    "ensemble: note: other users can read {}; `chmod 600` it",
                    file.display()
                );
            }
        }
        Some(file)
    }

    /// The runner's: `~/.config/ensemble/runner/.env`, or `--env-file`.
    fn runner_env(&self) -> Result<Option<PathBuf>, String> {
        Ok(self.env_file_in(state::runner_config_dir()?))
    }

    /// A hub's: `~/.config/ensemble/hub/.env`, or `--env-file`.
    fn hub_env(&self) -> Result<Option<PathBuf>, String> {
        Ok(self.env_file_in(state::hub_config_dir()?))
    }

    /// The runner's teams, as one helper team, saying which wait for approval.
    fn teams(&self) -> Result<Config, String> {
        let (config, waiting) = teams::load_all(self.runner_env()?.as_deref(), &SystemEnv)?;
        for line in &waiting {
            eprintln!("ensemble: {line}");
        }
        Ok(config)
    }
}

fn run(
    ctx: &Ctx,
    cli: cli::Cli,
    hub: &ensemble::ops::Registry<ensemble::link::accounts::HubContext>,
    runner: &ensemble::ops::Registry<cmd::runner::RunnerContext>,
) -> Result<ExitCode, String> {
    use cli::Command;
    match cli.command {
        Command::Runner(c) => cmd::runner::run(ctx, c),
        Command::Hub(c) => cmd::hub::run(ctx, c),
        Command::Completions { shell } => {
            use std::io::Write;
            let mut script = Vec::new();
            clap_complete::generate(
                shell,
                &mut cli::command(hub, runner),
                "ensemble",
                &mut script,
            );
            // Piped into `head` or the like, the rest is not wanted: no error.
            let _ = std::io::stdout().write_all(&script);
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// This computer's runner file, `~/.config/ensemble/runner/runner.yaml`: a
/// computer runs one runner.
fn runner_file() -> Result<PathBuf, String> {
    Ok(config::runner_path(&state::runner_config_dir()?))
}

/// This computer's runner, from its runner file.
fn this_runner() -> Result<config::Runner, String> {
    let path = runner_file()?;
    config::load_runner(&path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "no runner file at {}; `ensemble runner setup` or `ensemble runner init` writes one",
                path.display()
            )
        })
}

/// The hub server this computer's runner names, and its token.
fn runner_hub(command: &str) -> Result<(String, String), String> {
    let runner = this_runner().map_err(|e| format!("{command}: {e}"))?;
    let url = runner.hub.ok_or_else(|| {
        format!(
            "{command}: this computer's runner names no hub; `ensemble runner login <url>` names one in {}",
            runner.source.display()
        )
    })?;
    let token = ensemble::link::Tokens::load()?
        .get(&url)
        .ok_or_else(|| {
            format!("{command}: no token for the hub {url}. Sign in first: ensemble runner login {url} --user <name>")
        })?
        .to_string();
    Ok((url, token))
}

/// The tasks of each project in this computer's runner's workspace, on its
/// hub: the project's name and its board.
fn boards(command: &str) -> Result<Vec<(String, ensemble::board::RemoteBoard)>, String> {
    let runner = this_runner().map_err(|e| format!("{command}: {e}"))?;
    let (url, token) = runner_hub(command)?;
    Ok(daemon::repositories(&runner.workspace)
        .iter()
        .filter_map(|dir| dir.file_name().map(|n| n.to_string_lossy().into_owned()))
        .map(|project| {
            let board = ensemble::board::RemoteBoard::new(&url, &token, &project);
            (project, board)
        })
        .collect())
}

/// A secret, such as a password: typed at the terminal without echo, or read
/// from stdin when there is no terminal, without its line end.
fn read_secret(prompt: &str) -> Result<String, String> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut stdin.lock(), &mut text)
            .map_err(|e| format!("cannot read from stdin: {e}"))?;
        return Ok(text.trim_end_matches(['\n', '\r']).to_string());
    }
    eprint!("{prompt}");
    #[cfg(unix)]
    let restore = unsafe {
        let mut term: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut term) == 0 {
            let before = term;
            term.c_lflag &= !libc::ECHO;
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &term);
            Some(before)
        } else {
            None
        }
    };
    let mut line = String::new();
    let read = stdin.read_line(&mut line);
    #[cfg(unix)]
    if let Some(before) = restore {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &before);
        }
    }
    eprintln!();
    read.map_err(|e| format!("cannot read the terminal: {e}"))?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
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

/// Ask a question on the terminal and read the answer; empty means `default`.
fn ask(question: &str, default: Option<&str>) -> Result<String, String> {
    use std::io::Write;
    match default {
        Some(d) => eprint!("{question} [{d}]: "),
        None => eprint!("{question}: "),
    }
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("cannot read an answer: {e}"))?;
    let line = line.trim();
    match (line.is_empty(), default) {
        (true, Some(d)) => Ok(d.to_string()),
        (true, None) => Err(format!("{question}: an answer is needed")),
        _ => Ok(line.to_string()),
    }
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

/// This computer's name, offered as the runner's name when setup asks.
fn host_name() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .ok()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
}

/// This computer's name, for naming the tokens it holds.
fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "this computer".into())
}

/// After a team is added, changed or removed: have the running runner use it
/// now, and say what happened.
fn reload_runner() {
    let reloaded = state::runner_dir()
        .and_then(|d| ensemble::runtime::block_on(daemon::reload(&state::runner_socket(&d))));
    match reloaded {
        Ok(said) => println!("The runner uses it now: {said}."),
        Err(e) if e.starts_with("no runner is running") => {
            println!("No runner is running; it reads its teams when it starts.")
        }
        Err(e) => println!("{e}"),
    }
}

/// Set by SIGINT and SIGTERM, so that a runner or a hub server stops cleanly.
static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set with [`STOP`] when the running hub or runner stops to start again in the
/// same process, with its settings as they are now.
static RESTART: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Have the running hub or runner stop and start again, in this process.
fn restart() {
    RESTART.store(true, std::sync::atomic::Ordering::Relaxed);
    STOP.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Whether it stopped to start again; if so, it may run again.
fn restarting() -> bool {
    let again = RESTART.swap(false, std::sync::atomic::Ordering::Relaxed);
    if again {
        STOP.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    again
}

#[cfg(unix)]
extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Ask yes or no, or take `yes` (`--yes`) for the answer. Without a terminal
/// and without `--yes` there is nobody to ask, which is a refusal.
fn confirm(question: &str, yes: bool) -> Result<bool, String> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        return Err(format!(
            "{question} There is no terminal to ask on: pass --yes to approve."
        ));
    }
    let answer = ask(&format!("{question} [y/N]"), Some("n"))?;
    Ok(matches!(answer.to_lowercase().as_str(), "y" | "yes"))
}

/// Whether `matches`, under `hub` or `runner`, names `config edit` or `config
/// merge`, which only the command line has.
fn is_extra(matches: &clap::ArgMatches) -> bool {
    matches!(
        matches.subcommand(),
        Some(("config", c)) if matches!(c.subcommand_name(), Some("edit" | "merge"))
    )
}
