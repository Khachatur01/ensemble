//! The `ensemble` binary: the command line (`cli`), the commands made from the
//! hub's and the node's operations (`ops_cli`), and what each other group of
//! commands does (`cmd`). `ensemble mcp` is what an agent CLI starts, a
//! session of the node; everything else is for the person who runs the node,
//! or a hub, and the work in their folders.

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
    let node = cmd::node::operations();
    let matches = cli::command(&hub, &node).get_matches();
    match dispatch(&matches, &hub, &node) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("ensemble: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Run what `matches` names: one of the hub's or the node's operations, or
/// one of the other commands.
fn dispatch(
    matches: &clap::ArgMatches,
    hub: &ensemble::ops::Registry<ensemble::link::accounts::HubContext>,
    node: &ensemble::ops::Registry<cmd::node::NodeContext>,
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
        Some(("node", under)) if ops_cli::matched(node, under).is_some() || is_extra(under) => {
            let socket = state::node_socket(&state::node_dir()?);
            let route = local::Route::to(&socket, cmd::node::NodeContext::new);
            let call = |name: &str, input| {
                route
                    .as_ref()
                    .map_err(|e| Error::failed(e.clone()))?
                    .call(node, name, input)
            };
            if let Some((name, leaf)) = ops_cli::matched(node, under) {
                return ops_cli::run(node, &call, name, leaf);
            }
            let file = || cmd::node::NodeContext::new().map(|c| c.into_settings());
            if let Some(done) = cmd::config::run_extra(under, &call, file) {
                return done;
            }
        }
        _ => {}
    }
    let cli = cli::Cli::from_arg_matches(matches).map_err(|e| e.to_string())?;
    run(&ctx, cli, hub, node)
}

/// What every command is given: the global options, resolved.
struct Ctx {
    env_file: Option<PathBuf>,
    /// The folder the command works in.
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

    /// The node's: `~/.config/ensemble/node/.env`, or `--env-file`.
    fn node_env(&self) -> Result<Option<PathBuf>, String> {
        Ok(self.env_file_in(state::node_config_dir()?))
    }

    /// A hub's: `~/.config/ensemble/hub/.env`, or `--env-file`.
    fn hub_env(&self) -> Result<Option<PathBuf>, String> {
        Ok(self.env_file_in(state::hub_config_dir()?))
    }

    /// The node's teams, as one helper team, saying which wait for approval.
    fn teams(&self) -> Result<Config, String> {
        let (config, waiting) = teams::load_all(self.node_env()?.as_deref(), &SystemEnv)?;
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
    node: &ensemble::ops::Registry<cmd::node::NodeContext>,
) -> Result<ExitCode, String> {
    use cli::Command;
    match cli.command {
        Command::Node(c) => cmd::node::run(ctx, c),
        Command::Hub(c) => cmd::hub::run(ctx, c),
        Command::Task(c) => cmd::task::run(ctx, c),
        Command::Knowledge(c) => cmd::knowledge::run(ctx, c),
        Command::Completions { shell } => {
            use std::io::Write;
            let mut script = Vec::new();
            clap_complete::generate(shell, &mut cli::command(hub, node), "ensemble", &mut script);
            // Piped into `head` or the like, the rest is not wanted: no error.
            let _ = std::io::stdout().write_all(&script);
            Ok(ExitCode::SUCCESS)
        }
        Command::Mcp => cmd::node::mcp(ctx),
        Command::Hook => cmd::node::hook(),
    }
}

/// This computer's node file, `~/.config/ensemble/node/node.yaml`: a computer
/// runs one node.
fn node_file() -> Result<PathBuf, String> {
    Ok(config::node_path(&state::node_config_dir()?))
}

/// This computer's node, from its node file.
fn this_node() -> Result<config::Node, String> {
    let path = node_file()?;
    config::load_node(&path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "no node file at {}; `ensemble node setup` or `ensemble node init` writes one",
                path.display()
            )
        })
}

/// The hub server this computer's node names, and its token.
fn node_hub(command: &str) -> Result<(String, String), String> {
    let node = this_node().map_err(|e| format!("{command}: {e}"))?;
    let url = node.hub.ok_or_else(|| {
        format!(
            "{command}: this computer's node names no hub; add `hub: wss://…` to {}",
            node.source.display()
        )
    })?;
    let token = ensemble::link::Tokens::load()?
        .get(&url)
        .ok_or_else(|| {
            format!("{command}: no token for the hub {url}. Sign in first: ensemble node login {url} --user <name>")
        })?
        .to_string();
    Ok((url, token))
}

/// The knowledge of this computer's node's hub, for work in `dir`.
fn knowledge_for(dir: &Path, command: &str) -> Result<ensemble::link::RemoteMemory, String> {
    let (url, token) = node_hub(command)?;
    let project = dir.file_name().map(|n| n.to_string_lossy().into_owned());
    Ok(ensemble::link::RemoteMemory::new(&url, &token, project))
}

/// The tasks of the working folder's project, on this computer's node's
/// hub.
fn board_for(dir: &Path, command: &str) -> Result<ensemble::board::RemoteBoard, String> {
    let (url, token) = node_hub(command)?;
    let project = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| {
            format!(
                "{command}: {} has no name to keep tasks under",
                dir.display()
            )
        })?;
    Ok(ensemble::board::RemoteBoard::new(&url, &token, &project))
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

/// Where tasks and runs are kept for the work done in `dir`.
fn state_for(dir: &Path) -> Result<PathBuf, String> {
    state::dir_for(dir)
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

/// Run a registration, saying first what it is. The client's own words come
/// through, and so does its exit status.
fn run_registration(reg: &setup::Registration) -> Result<(), String> {
    println!("$ {}", reg.line());
    match std::process::Command::new(reg.program)
        .args(&reg.args)
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!(
            "`{}` failed ({status}). Run the command above yourself to see why.",
            reg.program
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "`{}` is not on PATH. Install it, then run the command above.",
            reg.program
        )),
        Err(e) => Err(format!("cannot run `{}`: {e}", reg.program)),
    }
}

/// This computer's name, offered as the node's name when setup asks.
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

/// After a team is added, changed or removed: have the running node use it
/// now, and say what happened.
fn reload_node() {
    let reloaded = state::node_dir()
        .and_then(|d| ensemble::runtime::block_on(daemon::reload(&state::node_socket(&d))));
    match reloaded {
        Ok(said) => println!("The node uses it now: {said}."),
        Err(e) if e.starts_with("no node is running") => {
            println!("No node is running; it reads its teams when it starts.")
        }
        Err(e) => println!("{e}"),
    }
}

/// Tell this computer's node about `dir`, so that its worker runs the
/// folder's tasks, and say what happens next.
fn tell_node(dir: &Path) -> String {
    let told = state::node_dir().and_then(|node| {
        ensemble::runtime::block_on(daemon::touch(&state::node_socket(&node), dir))
    });
    match told {
        Ok(()) => "your node runs it, or another of yours with its agent".into(),
        Err(e) => format!(
            "{e}; another of your nodes with its agent runs it, or this one once it does (`ensemble node serve`)"
        ),
    }
}

/// Set by SIGINT and SIGTERM, so that a node or a hub server stops cleanly.
static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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

/// Whether `matches`, under `hub` or `node`, names `config edit` or `config
/// merge`, which only the command line has.
fn is_extra(matches: &clap::ArgMatches) -> bool {
    matches!(
        matches.subcommand(),
        Some(("config", c)) if matches!(c.subcommand_name(), Some("edit" | "merge"))
    )
}
