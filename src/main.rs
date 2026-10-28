//! The `ensemble` binary.
//!
//! `ensemble mcp` is what a harness spawns; everything else is for the person
//! setting it up — validating the config, checking the backends are reachable,
//! and sending one task to one role without a harness in the way.

use ensemble::config::{self, Config, SystemEnv};
use ensemble::harness::mcp;
use ensemble::router::Ensemble;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
ensemble — one interface for several models, delegating work between them

USAGE:
    ensemble <COMMAND> [OPTIONS]

COMMANDS:
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
}

fn parse_args(argv: Vec<String>) -> Result<Option<Args>, String> {
    let mut command: Option<String> = None;
    let mut rest = Vec::new();
    let mut config = None;
    let mut dir = None;
    let mut context = None;

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
        "mcp" => cmd_mcp(config, dir),
        "check" => cmd_check(config),
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

fn cmd_check(config: Config) -> Result<ExitCode, String> {
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
