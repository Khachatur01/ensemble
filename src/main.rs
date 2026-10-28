//! The `ensemble` binary.
//!
//! `ensemble serve` is the one a harness points at: it puts the manager behind
//! an OpenAI-compatible URL so a tool like OpenCode can select
//! `ensemble/manager` and nothing else has to be configured. `ensemble mcp` is
//! the older surface, spawned by a harness that prefers it. Everything else is
//! for the person setting it up — validating the config, checking the backends
//! are reachable, and sending one task to one role with no harness in the way.

use ensemble::config::{self, Config, SystemEnv};
use ensemble::harness::{mcp, serve};
use ensemble::router::Ensemble;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "\
ensemble — one interface for several models, delegating work between them

USAGE:
    ensemble <COMMAND> [OPTIONS]

COMMANDS:
    serve                   Serve the manager over an OpenAI-compatible HTTP
                            API, as the model id 'ensemble/manager'. Point a
                            harness at it; nothing else has to be registered.
                            Spends one small model call at startup proving the
                            manager's server really returns tool calls, and
                            refuses to bind if it does not.
    mcp                     Serve the harness over MCP on stdio. This is what a
                            harness spawns; it is not meant to be run by hand.
    check                   Load and validate the config, then report the team.
    roles                   List the roles and what runs each.
    health                  Check that every backend in use is reachable.
    run <ROLE> <TASK>       Send one task to one role and print the answer.

OPTIONS:
    -c, --config <PATH>     Path to ensemble.yaml.
                            Default: the nearest one, searching upward from
                            --dir (or the current directory).
    -d, --dir <PATH>        The project directory roles work in.
                            Default: the current directory.
        --context <TEXT>    Extra context for `run`.
    -h, --help              This.
    -V, --version           Print the version.

SERVE OPTIONS:
        --host <ADDR>       Address to bind. Default: 127.0.0.1. A non-loopback
                            address requires --api-key-env.
        --port <PORT>       Port to bind. Default: 8787. 0 picks a free one.
        --api-key-env <VAR> Name of the environment variable holding the bearer
                            token clients must present. The token itself is
                            never written on the command line or logged.
        --max-steps <N>     How many times the manager may be asked within one
                            request before it has to answer. Default: 8.

ENVIRONMENT:
    Backends name the variables they read; `ensemble check` lists them. API keys
    are never read from the config file.
";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("ensemble: {message}");
            ExitCode::FAILURE
        }
    }
}

struct Args {
    command: String,
    rest: Vec<String>,
    config: Option<PathBuf>,
    dir: Option<PathBuf>,
    context: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    api_key_env: Option<String>,
    max_steps: Option<u32>,
}

fn parse_args(argv: Vec<String>) -> Result<Option<Args>, String> {
    let mut command: Option<String> = None;
    let mut rest = Vec::new();
    let mut config = None;
    let mut dir = None;
    let mut context = None;
    let mut host = None;
    let mut port = None;
    let mut api_key_env = None;
    let mut max_steps = None;

    let mut it = argv.into_iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| -> Result<String, String> {
            it.next().ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("ensemble {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-c" | "--config" => config = Some(PathBuf::from(value("--config")?)),
            "-d" | "--dir" => dir = Some(PathBuf::from(value("--dir")?)),
            "--context" => context = Some(value("--context")?),
            "--host" => host = Some(value("--host")?),
            "--port" => {
                let raw = value("--port")?;
                port = Some(
                    raw.parse()
                        .map_err(|_| format!("--port {raw}: not a port number"))?,
                );
            }
            "--api-key-env" => api_key_env = Some(value("--api-key-env")?),
            "--max-steps" => {
                let raw = value("--max-steps")?;
                let n: u32 = raw
                    .parse()
                    .map_err(|_| format!("--max-steps {raw}: not a number"))?;
                if n == 0 {
                    return Err("--max-steps must be at least 1".into());
                }
                max_steps = Some(n);
            }
            other if other.starts_with('-') && command.is_none() => {
                return Err(format!("unknown option '{other}'. Try --help."));
            }
            other if command.is_none() => command = Some(other.to_string()),
            other => rest.push(other.to_string()),
        }
    }

    match command {
        None => {
            print!("{USAGE}");
            Ok(None)
        }
        Some(command) => Ok(Some(Args {
            command,
            rest,
            config,
            dir,
            context,
            host,
            port,
            api_key_env,
            max_steps,
        })),
    }
}

fn run() -> Result<ExitCode, String> {
    let Some(args) = parse_args(std::env::args().skip(1).collect())? else {
        return Ok(ExitCode::SUCCESS);
    };

    let dir = match &args.dir {
        Some(d) => d
            .canonicalize()
            .map_err(|e| format!("--dir {}: {e}", d.display()))?,
        None => std::env::current_dir()
            .map_err(|e| format!("cannot read the current directory: {e}"))?,
    };

    let config_path = match &args.config {
        Some(p) => p.clone(),
        None => config::discover(&dir).ok_or_else(|| {
            format!(
                "no {} found in {} or any parent directory. Create one, or pass --config <path>.",
                config::CONFIG_FILENAME,
                dir.display()
            )
        })?,
    };

    let config = config::load(&config_path, &SystemEnv).map_err(|e| e.to_string())?;

    match args.command.as_str() {
        "serve" => cmd_serve(config, dir, &args),
        "mcp" => cmd_mcp(config, dir),
        "check" => cmd_check(config, dir),
        "roles" => cmd_roles(config, dir),
        "health" => cmd_health(config, dir),
        "run" => cmd_run(config, dir, &args),
        other => Err(format!("unknown command '{other}'. Try --help.")),
    }
}

fn build(config: Config, dir: PathBuf) -> Result<Ensemble, String> {
    Ensemble::new(config, Some(dir)).map_err(|e| e.to_string())
}

fn cmd_mcp(config: Config, dir: PathBuf) -> Result<ExitCode, String> {
    // A friendly nudge, on stderr so it cannot corrupt the protocol stream.
    if std::io::stdin().is_terminal() {
        eprintln!(
            "ensemble: speaking MCP (JSON-RPC 2.0, one object per line) on stdin/stdout.\n\
             ensemble: a harness spawns this; for a one-off try `ensemble run <role> <task>`."
        );
    }
    let ensemble = build(config, dir)?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    mcp::serve(&ensemble, stdin.lock(), stdout.lock()).map_err(|e| format!("mcp: {e}"))?;
    Ok(ExitCode::SUCCESS)
}

/// Serve the manager over HTTP.
///
/// The project directory is settled here, once, from `--dir` — never from a
/// request field and never from anything a model wrote. One workspace per
/// server instance; run a second server for a second checkout.
fn cmd_serve(config: Config, dir: PathBuf, args: &Args) -> Result<ExitCode, String> {
    // The flag names the variable; the value is read from the environment, so
    // the token never appears in a command line, a shell history or a log.
    let api_key = match &args.api_key_env {
        None => None,
        Some(var) => match std::env::var(var).ok().filter(|v| !v.trim().is_empty()) {
            Some(token) => Some(token),
            None => {
                return Err(format!(
                    "serve: --api-key-env names ${var}, but it is unset or empty. \
                     Export it, or drop the flag and bind loopback only."
                ));
            }
        },
    };

    let options = serve::Options {
        host: args.host.clone().unwrap_or_else(|| "127.0.0.1".into()),
        port: args.port.unwrap_or(serve::DEFAULT_PORT),
        api_key,
        max_steps: args.max_steps.unwrap_or(serve::manager::DEFAULT_MAX_STEPS),
    };

    let ensemble = Arc::new(build(config, dir)?);
    serve::serve(ensemble, &options)?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_check(config: Config, dir: PathBuf) -> Result<ExitCode, String> {
    // Reaching here means the config already loaded and validated; `load`
    // returns an error otherwise.
    println!(
        "{}: ok (version {})",
        config.source.display(),
        config.version
    );
    println!();
    println!("backends:");
    for (name, b) in &config.backends {
        let detail = match &b.settings {
            config::BackendSettings::ClaudeCodeCli {
                command,
                allowed_tools,
                ..
            } => {
                format!("{command}, tools: {allowed_tools}")
            }
            config::BackendSettings::OpenAiCompatible {
                base_url,
                api_key,
                base_url_env,
            } => {
                format!(
                    "{base_url} (from ${base_url_env}){}",
                    if api_key.is_some() {
                        ", authenticated"
                    } else {
                        ""
                    }
                )
            }
        };
        println!(
            "  {name}  [{}]  {detail}  timeout {}s",
            b.type_id(),
            b.timeout_seconds
        );
    }
    println!();
    println!("roles:");
    for role in config.role_names() {
        let a = config
            .assignment(role)
            .expect("role_names only yields configured roles");
        let marker = if role == Config::MANAGER {
            " (mandatory)"
        } else {
            ""
        };
        println!("  {role}{marker}  ->  {} / {}", a.backend, a.model);
    }

    // Whether the HTTP surface can run at all is a property of the manager's
    // backend, and finding that out should not require starting a server.
    println!();
    match Ensemble::new(config, Some(dir)) {
        Ok(ensemble) => println!("serve: {}", serve::availability(&ensemble)),
        Err(e) => println!("serve: unavailable — {e}"),
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_roles(config: Config, dir: PathBuf) -> Result<ExitCode, String> {
    let ensemble = build(config, dir)?;
    for role in ensemble.roles() {
        let (backend, model) = ensemble.route_of(role).map_err(|e| e.to_string())?;
        println!("{role}\t{backend}\t{model}");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_health(config: Config, dir: PathBuf) -> Result<ExitCode, String> {
    let ensemble = build(config, dir)?;
    let mut all_ok = true;
    for (backend, result) in ensemble.health() {
        match result {
            Ok(()) => println!("{backend}: ok"),
            Err(e) => {
                all_ok = false;
                println!("{backend}: UNREACHABLE");
                println!("  {e}");
            }
        }
    }
    // Exit code, so this is usable in a script.
    Ok(if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn cmd_run(config: Config, dir: PathBuf, args: &Args) -> Result<ExitCode, String> {
    let role = args
        .rest
        .first()
        .ok_or("run: needs a role. Try `ensemble roles` to see them.")?;
    let task = args.rest[1..].join(" ");
    if task.trim().is_empty() {
        return Err(
            "run: needs a task, e.g. `ensemble run planner \"outline the migration\"`".into(),
        );
    }

    let ensemble = build(config, dir)?;
    let outcome = ensemble
        .delegate(role, &task, args.context.as_deref())
        .map_err(|e| e.to_string())?;

    // The answer to stdout so it pipes; the provenance to stderr so it does not
    // end up in whatever consumed the answer.
    eprintln!(
        "ensemble: {} via {}/{} ({} in, {} out)",
        outcome.role,
        outcome.backend,
        outcome.model,
        outcome.prompt_tokens,
        outcome.completion_tokens
    );
    println!("{}", outcome.text);
    Ok(ExitCode::SUCCESS)
}
