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
use ensemble::state;
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
    init [--force|--stdout] Write a sample organization: config.yaml in the --org
                            directory, with every setting. Refuses to overwrite
                            it unless --force; --stdout prints it instead.
    migrate <FILE>          Rewrite a version 1 config (one team per project) as
                            an organization in --org, as the team `default`.
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
    -o, --org <DIR>         The organization: the directory with config.yaml, its
                            llm_backends and teams. Nothing is searched for.
        --env-file <PATH>   A file of NAME=value lines read for variables the
                            environment does not set.
        --state <DIR>       Keep tasks, runs and worktrees here instead of in
                            the per-user state directory.
    -d, --dir <PATH>        The project directory agents work in.
                            Default: the current directory.
        --context <TEXT>    Extra context for `run`.
    -h, --help              This.
    -V, --version           Print the version.

ENVIRONMENT:
    LLM backends name the variables they read; `ensemble check` lists them. API keys
    are never read from the config file. Variables missing from the environment
    are read from the --env-file, when one is given.
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
    org: Option<PathBuf>,
    env_file: Option<PathBuf>,
    state: Option<PathBuf>,
    dir: Option<PathBuf>,
    context: Option<String>,
}

fn parse_args(argv: Vec<String>) -> Result<Option<Args>, String> {
    let mut command: Option<String> = None;
    let mut rest = Vec::new();
    let mut org = None;
    let mut env_file = None;
    let mut state = None;
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
            "-o" | "--org" => org = Some(PathBuf::from(value("--org")?)),
            "--env-file" => env_file = Some(PathBuf::from(value("--env-file")?)),
            "--state" => state = Some(PathBuf::from(value("--state")?)),
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
            org,
            env_file,
            state,
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
        return cmd_init(&args);
    }
    if args.command == "migrate" {
        return cmd_migrate(&args);
    }

    let org = org_of(&args, &args.command)?;
    let config_path = config::config_path(&org);
    let config = config::load(&config_path, args.env_file.as_deref(), &SystemEnv)
        .map_err(|e| e.to_string())?;

    match args.command.as_str() {
        "mcp" => cmd_mcp(config, dir, &args),
        "check" => cmd_check(config, &dir, &args),
        "agents" => cmd_agents(config, dir, &args),
        "show" => cmd_show(&config, &args),
        "health" => cmd_health(config, dir, &args),
        "run" => cmd_run(config, dir, &args),
        other => Err(format!("unknown command '{other}'. Try --help.")),
    }
}

/// The organization directory, which every command but `init`, `migrate` and
/// `schema` needs. It is named, never found.
fn org_of(args: &Args, command: &str) -> Result<PathBuf, String> {
    match &args.org {
        Some(org) => Ok(org.clone()),
        None => Err(format!(
            "{command}: no organization given. Pass --org <dir>, the directory with config.yaml. `ensemble init --org <dir>` makes one."
        )),
    }
}

/// An `Ensemble` for `config`, keeping its state where `--state` says.
fn ensemble_for(config: Config, dir: PathBuf, args: &Args) -> Ensemble {
    let ensemble = Ensemble::new(config, Some(dir));
    match &args.state {
        Some(state) => ensemble.with_state(state.clone()),
        None => ensemble,
    }
}

/// Where tasks and runs are kept for the work done in `dir`.
fn state_for(dir: &Path, args: &Args) -> Result<PathBuf, String> {
    match &args.state {
        Some(state) => Ok(state.clone()),
        None => state::dir_for(dir),
    }
}

/// Write the sample organization, or print it.
fn cmd_init(args: &Args) -> Result<ExitCode, String> {
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
    let org = org_of(args, "init")?;
    let path = config::config_path(&org);
    if path.exists() && !has("--force") {
        return Err(format!(
            "{} already exists. Pass --force to overwrite it, or --stdout to print the sample.",
            path.display()
        ));
    }
    std::fs::create_dir_all(&org).map_err(|e| format!("cannot create {}: {e}", org.display()))?;
    std::fs::write(&path, config::SAMPLE)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!(
        "wrote {}: a sample organization with every setting",
        path.display()
    );
    println!(
        "Edit the teams, then run `ensemble check --org {}`. API keys go in the\n\
         environment, or in a file you pass with --env-file; never in this directory.",
        org.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// Rewrite a version 1 config as an organization in `--org`.
fn cmd_migrate(args: &Args) -> Result<ExitCode, String> {
    let source = args
        .rest
        .first()
        .ok_or("migrate: needs the version 1 config file, e.g. `ensemble migrate .ensemble/config.yaml --org ~/my-org`")?;
    let org = org_of(args, "migrate")?;
    let text = std::fs::read_to_string(source).map_err(|e| format!("migrate: {source}: {e}"))?;
    let v2 = config::migrate::to_v2(&text).map_err(|e| format!("migrate: {source}: {e}"))?;
    let path = config::config_path(&org);
    if path.exists() && !args.rest.iter().any(|a| a == "--force") {
        return Err(format!(
            "{} already exists. Pass --force to overwrite it.",
            path.display()
        ));
    }
    // Loading it now says what, if anything, the old file got wrong.
    config::parse(&v2, &path, &SystemEnv)
        .map_err(|e| format!("migrate: the result is not valid: {e}"))?;
    std::fs::create_dir_all(&org).map_err(|e| format!("cannot create {}: {e}", org.display()))?;
    std::fs::write(&path, v2).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!(
        "wrote {}: every agent is now `{}/<name>`. Comments were not kept. Give the team a real `prompt`.",
        path.display(),
        config::migrate::MIGRATED_TEAM
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_mcp(config: Config, dir: PathBuf, args: &Args) -> Result<ExitCode, String> {
    // A friendly nudge, on stderr so it cannot corrupt the protocol stream.
    if std::io::stdin().is_terminal() {
        eprintln!(
            "ensemble: speaking MCP (JSON-RPC 2.0, one object per line) on stdin/stdout.\n\
             ensemble: a harness spawns this; for a one-off try `ensemble run <agent> <task>`."
        );
    }
    let ensemble = ensemble_for(config, dir, args);
    let stdin = std::io::stdin();
    // Not locked: delegations reply from their own threads.
    mcp::serve(&ensemble, stdin.lock(), std::io::stdout()).map_err(|e| format!("mcp: {e}"))?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_check(config: Config, dir: &Path, args: &Args) -> Result<ExitCode, String> {
    // Reaching here means the config already loaded and validated; `load`
    // returns an error otherwise.
    println!(
        "{}: ok (version {})",
        config.source.display(),
        config.version
    );
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
    println!("teams:");
    for team in config.teams.values() {
        println!(
            "  {}  -  {}  ({} agents)",
            team.name,
            team.summary(),
            team.agents.len()
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
    let store = tasks::Store::new(&state_for(dir, args)?);
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
                if t.gate.as_ref().is_some_and(|g| {
                    config
                        .agent(&t.agent)
                        .is_some_and(|a| config.gate(&a.team, &g.name).is_none())
                }) =>
            {
                bad.push(format!(
                    "{}: gate '{}' is not configured under the team's `gates`; it would fail at once",
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

fn cmd_agents(config: Config, dir: PathBuf, args: &Args) -> Result<ExitCode, String> {
    let ensemble = ensemble_for(config, dir, args);
    for role in ensemble.agents() {
        let (runs_on, model) = ensemble.route_of(role).map_err(|e| e.to_string())?;
        println!("{role}\t{runs_on}\t{model}");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_health(config: Config, dir: PathBuf, args: &Args) -> Result<ExitCode, String> {
    let ensemble = ensemble_for(config, dir, args);
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

    let ensemble = ensemble_for(config, dir, args);
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
            let mut inline = opencode::config(
                agent,
                oc,
                backend,
                std::path::Path::new(opencode::SHOWN_PROMPT_FILE),
            );
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
