//! The `ensemble` binary.
//!
//! `ensemble mcp` is the one a harness spawns: it offers the team's agents as MCP
//! tools. Everything else is for the person setting it up — validating the
//! config, checking the backends are reachable, and sending one task to one
//! agent with no harness in the way.

use ensemble::config::{self, Adapter, Config, SystemEnv};
use ensemble::external::{claude_code, opencode};
use ensemble::harness::mcp;
use ensemble::router::Ensemble;
use ensemble::tasks;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
ensemble — a team of coding agents behind one MCP server

USAGE:
    ensemble <COMMAND> [OPTIONS]

COMMANDS:
    mcp                     Serve the agents over MCP on stdio. This is what an
                            agent (Claude Code, OpenCode, ...) spawns, with the
                            project as its working directory; it is not meant
                            to be run by hand.
    init [--force|--stdout] Write .ensemble/ with a sample config.yaml that has
                            every setting, a .env for API keys and a .gitignore
                            that keeps .env out of git. Refuses to overwrite
                            config.yaml unless --force; --stdout prints it.
    show <AGENT>            Print how an agent is started: its command line and
                            the config handed to its CLI, API keys hidden.
    check                   Load and validate the config, then report the team.
    schema                  Print the JSON Schema for config.yaml, so an editor
                            can complete and check it. Needs no config.
    agents                  List the agents and what runs each.
    health                  Check that every agent CLI starts and every LLM
                            backend in use answers and serves its models.
    run <AGENT> <TASK>      Send one task to one agent and print the answer.

OPTIONS:
    -c, --config <PATH>     Path to the config file. A .env beside it is read.
                            Default: .ensemble/config.yaml in the project
                            directory.
                            Parent directories are not searched.
    -d, --dir <PATH>        The project directory agents work in.
                            Default: the current directory.
        --context <TEXT>    Extra context for `run`.
    -h, --help              This.
    -V, --version           Print the version.

ENVIRONMENT:
    LLM backends name the variables they read; `ensemble check` lists them. API keys
    are never read from the config file. Variables missing from the environment
    are read from .ensemble/.env, beside the config.
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

    if args.command == "schema" {
        print!("{}", config::SCHEMA);
        return Ok(ExitCode::SUCCESS);
    }

    let dir = match &args.dir {
        Some(d) => d
            .canonicalize()
            .map_err(|e| format!("--dir {}: {e}", d.display()))?,
        None => std::env::current_dir()
            .map_err(|e| format!("cannot read the current directory: {e}"))?,
    };

    if args.command == "init" {
        return cmd_init(&args, &dir);
    }

    let config_path = match &args.config {
        Some(p) => p.clone(),
        None => config::discover(&dir).ok_or_else(|| {
            format!(
                "no {} in the project directory {}. Run `ensemble init` there (parent directories are not searched), or pass --config <path>.",
                Path::new(config::CONFIG_DIR).join(config::CONFIG_FILENAME).display(),
                dir.display()
            )
        })?,
    };

    let config = config::load(&config_path, &SystemEnv).map_err(|e| e.to_string())?;

    match args.command.as_str() {
        "mcp" => cmd_mcp(config, dir),
        "check" => cmd_check(config, &dir),
        "agents" => cmd_agents(config, dir),
        "show" => cmd_show(&config, &args),
        "health" => cmd_health(config, dir),
        "run" => cmd_run(config, dir, &args),
        other => Err(format!("unknown command '{other}'. Try --help.")),
    }
}

/// Write the sample config, or print it.
fn cmd_init(args: &Args, dir: &std::path::Path) -> Result<ExitCode, String> {
    let has = |flag: &str| args.rest.iter().any(|a| a == flag);
    if let Some(other) = args
        .rest
        .iter()
        .find(|a| !["--force", "--stdout"].contains(&a.as_str()))
    {
        return Err(format!("init: unknown argument '{other}'. Try --help."));
    }
    if has("--stdout") {
        print!("{}", config::SAMPLE);
        return Ok(ExitCode::SUCCESS);
    }
    let path = args
        .config
        .clone()
        .unwrap_or_else(|| config::config_path(dir));
    if path.exists() && !has("--force") {
        return Err(format!(
            "{} already exists. Pass --force to overwrite it, or --stdout to print the sample.",
            path.display()
        ));
    }
    let folder = path.parent().unwrap_or(dir);
    std::fs::create_dir_all(folder)
        .map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
    let write = |file: &Path, text: &str| {
        std::fs::write(file, text).map_err(|e| format!("cannot write {}: {e}", file.display()))
    };
    write(&path, config::SAMPLE)?;
    println!("wrote {}: a sample team with every setting", path.display());
    // The secrets file is the user's once it exists: never overwritten.
    let env = config::env_path(&path);
    if !env.exists() {
        write(&env, SAMPLE_ENV)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&env, std::fs::Permissions::from_mode(0o600));
        }
        println!("wrote {}: put API keys here", env.display());
    }
    let ignore = folder.join(".gitignore");
    if !ignore.exists() {
        write(&ignore, ".env\n")?;
        println!("wrote {}: keeps .env out of git", ignore.display());
    }
    // Tasks belong to the project, wherever its config is.
    let tasks = dir.join(config::CONFIG_DIR).join(tasks::TASKS_DIR);
    let readme = tasks.join("README.md");
    if !readme.exists() {
        std::fs::create_dir_all(&tasks)
            .map_err(|e| format!("cannot create {}: {e}", tasks.display()))?;
        write(&readme, tasks::TASKS_README)?;
        println!(
            "wrote {}: how to write a task; runs/ beside it ignores itself",
            readme.display()
        );
    }
    println!("Edit the agents, then run `ensemble check`.");
    Ok(ExitCode::SUCCESS)
}

/// The `.env` `ensemble init` writes beside the sample config.
const SAMPLE_ENV: &str = "\
# Variables for config.yaml's api_key_env settings, read when they are not set
# in the environment. A variable that is set wins over this file. Keep it out
# of git; the .gitignore beside it does.
#
# OPENAI_LOCAL_API_KEY=
";

fn cmd_mcp(config: Config, dir: PathBuf) -> Result<ExitCode, String> {
    // A friendly nudge, on stderr so it cannot corrupt the protocol stream.
    if std::io::stdin().is_terminal() {
        eprintln!(
            "ensemble: speaking MCP (JSON-RPC 2.0, one object per line) on stdin/stdout.\n\
             ensemble: a harness spawns this; for a one-off try `ensemble run <agent> <task>`."
        );
    }
    let ensemble = Ensemble::new(config, Some(dir));
    let stdin = std::io::stdin();
    // Not locked: delegations reply from their own threads.
    mcp::serve(&ensemble, stdin.lock(), std::io::stdout()).map_err(|e| format!("mcp: {e}"))?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_check(config: Config, dir: &Path) -> Result<ExitCode, String> {
    // Reaching here means the config already loaded and validated; `load`
    // returns an error otherwise.
    println!(
        "{}: ok (version {})",
        config.source.display(),
        config.version
    );
    let env = config::env_path(&config.source);
    if env.is_file() {
        println!(
            "{}: read for variables the environment does not set",
            env.display()
        );
    }
    println!();
    println!("llm_backends:");
    for (name, b) in &config.llm_backends {
        let detail = match &b.settings {
            config::BackendSettings::OpenAiCompatible { base_url, api_key } => {
                format!(
                    "{base_url}{}",
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
    println!("agents:");
    for role in config.agent_names() {
        let a = config
            .agent(role)
            .expect("agent_names only yields configured agents");
        let kind = if a.has_tools() {
            format!("{}: {}", a.kind(), a.tool_names().join(", "))
        } else {
            format!("{}, no tools", a.kind())
        };
        println!("  {role}  ->  {} / {}  ({kind})", a.runs_on(), a.model());
    }
    if config.agents.is_empty() {
        println!("  (none — add agents under `agents:`)");
    }

    // A bad ticket fails only when the worker reaches it; say so now.
    let store = tasks::Store::new(&dir.join(config::CONFIG_DIR));
    let entries = store.scan();
    let mut bad = Vec::new();
    for entry in &entries {
        match entry {
            tasks::Entry::Unreadable { id, error } => bad.push(format!("{id}: {error}")),
            tasks::Entry::Task(t, None) if config.agent(&t.agent).is_none() => bad.push(format!(
                "{}: agent '{}' is not configured; it would fail at once",
                t.id, t.agent
            )),
            tasks::Entry::Task(t, None)
                if t.gate
                    .as_ref()
                    .is_some_and(|g| !config.gates.contains_key(&g.name)) =>
            {
                bad.push(format!(
                    "{}: gate '{}' is not configured under `gates`; it would fail at once",
                    t.id,
                    t.gate.as_ref().map_or("", |g| g.name.as_str())
                ))
            }
            _ => {}
        }
    }
    for entry in &entries {
        if let tasks::Entry::Task(t, None) = entry {
            for dep in tasks::waiting_for(t, &entries) {
                if dep.ends_with("(no such task)") || dep.ends_with("(unreadable)") {
                    bad.push(format!("{}: `after` {dep}; it can never start", t.id));
                }
            }
        }
    }
    for id in tasks::cyclic(&entries) {
        bad.push(format!(
            "{id}: its `after` leads back to itself; it can never start"
        ));
    }
    if !entries.is_empty() {
        let queued = entries
            .iter()
            .filter(|e| matches!(e, tasks::Entry::Task(_, None)))
            .count();
        println!();
        println!(
            "tasks: {} in {}, {queued} without a result",
            entries.len(),
            store.tasks_dir().display()
        );
        for line in &bad {
            println!("  {line}");
        }
    }

    println!();
    println!("mcp: ready. `ensemble health` checks the agent CLIs and LLM backends.");
    Ok(if bad.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn cmd_agents(config: Config, dir: PathBuf) -> Result<ExitCode, String> {
    let ensemble = Ensemble::new(config, Some(dir));
    for role in ensemble.agents() {
        let (runs_on, model) = ensemble.route_of(role).map_err(|e| e.to_string())?;
        println!("{role}\t{runs_on}\t{model}");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_health(config: Config, dir: PathBuf) -> Result<ExitCode, String> {
    let ensemble = Ensemble::new(config, Some(dir));
    let mut all_ok = true;
    for (backend, result) in ensemble.health() {
        match result {
            Ok(()) => println!("{backend}: ok"),
            Err(e) => {
                all_ok = false;
                println!("{backend}: FAILED");
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
        .ok_or("run: needs an agent. Try `ensemble agents` to see them.")?;
    let task = args.rest[1..].join(" ");
    if task.trim().is_empty() {
        return Err(
            "run: needs a task, e.g. `ensemble run planner \"outline the migration\"`".into(),
        );
    }

    let ensemble = Ensemble::new(config, Some(dir));
    let outcome = ensemble
        .delegate(role, &task, args.context.as_deref())
        .map_err(|e| e.to_string())?;

    // The answer to stdout so it pipes; the provenance to stderr so it does not
    // end up in whatever consumed the answer.
    eprintln!(
        "ensemble: {} via {} / {} ({} in, {} out)",
        outcome.agent,
        outcome.runs_on,
        outcome.model,
        outcome.prompt_tokens,
        outcome.completion_tokens
    );
    println!("{}", outcome.text);
    Ok(ExitCode::SUCCESS)
}

/// How an agent is started, without starting it: the command line, and the
/// config its CLI is handed. API keys are hidden.
fn cmd_show(config: &Config, args: &Args) -> Result<ExitCode, String> {
    let name = args
        .rest
        .first()
        .ok_or("show: needs an agent. Try `ensemble agents` to see them.")?;
    let agent = config.agent(name).ok_or_else(|| {
        format!(
            "show: no agent '{name}'. Agents: {}.",
            config.agent_names().join(", ")
        )
    })?;
    const TASK: &str = "<task>";
    match &agent.adapter {
        Adapter::ClaudeCode(cc) => {
            let argv = claude_code::arguments(agent, cc, TASK);
            println!("{}", command_line(&cc.command, &argv));
        }
        Adapter::Llm(la) => {
            let url = match config.llm_backends.get(&la.backend).map(|b| &b.settings) {
                Some(config::BackendSettings::OpenAiCompatible { base_url, .. }) => {
                    base_url.clone()
                }
                None => format!("<undefined backend {}>", la.backend),
            };
            println!("POST {url}/chat/completions");
            let body = ensemble::external::llm::request(agent, la, TASK, None);
            println!(
                "{}",
                serde_json::to_string_pretty(&body).map_err(|e| e.to_string())?
            );
        }
        Adapter::OpenCode(oc) => {
            let backend = oc.llm_backend().and_then(|b| config.llm_backends.get(b));
            let argv = opencode::arguments(agent, oc, backend, TASK);
            println!("{}", command_line(&oc.command, &argv));
            let mut inline = opencode::config(agent, oc, backend);
            opencode::redact(&mut inline);
            println!();
            println!("OPENCODE_CONFIG_CONTENT:");
            println!(
                "{}",
                serde_json::to_string_pretty(&inline).map_err(|e| e.to_string())?
            );
        }
    }
    Ok(ExitCode::SUCCESS)
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
