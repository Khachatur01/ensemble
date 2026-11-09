//! The `ensemble` binary.
//!
//! `ensemble mcp` is the one a harness spawns: it offers the team's agents as MCP
//! tools. Everything else is for the person setting it up — validating the
//! config, checking the backends are reachable, and sending one task to one
//! agent with no harness in the way.

use ensemble::config::{self, Adapter, Config, SystemEnv};
use ensemble::external::{claude_code, opencode};
use ensemble::harness::mcp;
use ensemble::remote;
use ensemble::router::Ensemble;
use ensemble::setup;
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
    setup [--org DIR] [--manager M] [--starter S] [--yes]
                            Set everything up: find the agent CLIs, write a
                            starter organization, register it with your agent
                            (the manager) and check it. Asks what it needs.
    connect --manager M --org DIR [--only T,T] [--env-file F] [--global]
                            Register Ensemble with a manager: claude-code or
                            opencode. Prints the command it runs.
    doctor                  Say what is wrong with the organization and the
                            agents it needs, and how to fix each thing.
    add-agent TEAM/NAME --adapter A [--model M] [--backend B] [--prompt P] [--writes]
                            Add an agent to a team, every setting written.
    remove-agent TEAM/NAME  Remove an agent from its team.
    org trust URL#COMMIT [--yes]
                            Read what a remote organization would run on this
                            machine and, if you approve, trust that commit.
    org update URL#COMMIT [--ref R]
                            Show what changed in a remote organization since
                            COMMIT and print its new pin. Trusts nothing.
    tasks [--status S]      List the tasks of this working folder and where each
                            stands. Reads files; starts nothing.
    task ID                 Show one task: its ticket, where it stands, its
                            question or its result.
    submit AGENT TITLE --text T   (or TITLE --team T: put it in a team's backlog)
           [--goal G] [--context C] [--repo R] [--gate N:K]
           [--after ID,ID] [--files F,F] [--constraints C,C] [--done-when D,D]
                            Write a task. A running `ensemble mcp` picks it up.
    answer ID TEXT          Answer the question a task stopped to ask.
    serve [--exit-when-idle]
                            Work through this folder's tasks with no agent session
                            around them, until stopped (Ctrl-C or SIGTERM), saying
                            when nothing is left to do; --exit-when-idle stops
                            there instead. Only one worker runs a folder's tasks.
    assign ID AGENT         Give a task to an agent: from the backlog, or to
                            someone else.
    stats [--since 7d] [--json]
                            What each agent has done: runs, failures, time and
                            tokens, from the record every run leaves.
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
    -o, --org <DIR|URL>     The organization: the directory with config.yaml, its
                            llm_backends and teams. Nothing is searched for. A
                            git repository is github.com/acme/org#COMMIT (a pin,
                            which it must be and be trusted), or @TAG for
                            `connect`, which pins it.
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
    org: Option<String>,
    /// Where usage counters live, when the organization is remote.
    usage: Option<PathBuf>,
    env_file: Option<PathBuf>,
    state: Option<PathBuf>,
    dir: Option<PathBuf>,
    context: Option<String>,
    /// The options that take a value and belong to one command: `--manager`,
    /// `--adapter`, `--model`, `--backend`, `--starter`, `--only`, `--prompt`,
    /// `--team-prompt`.
    opts: std::collections::BTreeMap<String, String>,
}

fn parse_args(argv: Vec<String>) -> Result<Option<Args>, String> {
    let mut command: Option<String> = None;
    let mut rest = Vec::new();
    let mut org = None;
    let mut env_file = None;
    let mut state = None;
    let mut dir = None;
    let mut context = None;
    let mut opts = std::collections::BTreeMap::new();

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
            "-o" | "--org" => org = Some(value("--org")?),
            "--env-file" => env_file = Some(PathBuf::from(value("--env-file")?)),
            "--state" => state = Some(PathBuf::from(value("--state")?)),
            "-d" | "--dir" => dir = Some(PathBuf::from(value("--dir")?)),
            "--context" => context = Some(value("--context")?),
            "--manager" | "--adapter" | "--model" | "--backend" | "--starter" | "--only"
            | "--prompt" | "--team-prompt" | "--status" | "--text" | "--goal" | "--repo"
            | "--gate" | "--team" | "--since" | "--after" | "--files" | "--constraints"
            | "--done-when" | "--ref" => {
                opts.insert(arg.clone(), value(&arg)?);
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
            org,
            usage: None,
            env_file,
            state,
            dir,
            context,
            opts,
        })),
    }
}

fn run() -> Result<ExitCode, String> {
    let Some(mut args) = parse_args(std::env::args().skip(1).collect())? else {
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
    match args.command.as_str() {
        "tasks" => return cmd_tasks(&args, &dir),
        "task" => return cmd_task(&args, &dir),
        "answer" => return cmd_answer(&args, &dir),
        "setup" => return cmd_setup(&args, &dir),
        "connect" => return cmd_connect(&args),
        "add-agent" => return cmd_add_agent(&args),
        "remove-agent" => return cmd_remove_agent(&args),
        _ => {}
    }

    if args.command == "org" {
        return cmd_org(&args);
    }

    let resolved = resolve_org(&args, &args.command)?;
    args.usage = resolved.usage.clone();
    let config_path = config::config_path(&resolved.dir);
    let mut config = config::load(&config_path, args.env_file.as_deref(), &SystemEnv)
        .map_err(|e| e.to_string())?;
    if let Some(only) = opt(&args, "--only") {
        let only: Vec<String> = only.split(',').map(|t| t.trim().to_string()).collect();
        config
            .retain_teams(&only)
            .map_err(|e| format!("--only: {e}"))?;
    }

    match args.command.as_str() {
        "mcp" => cmd_mcp(config, dir, &args),
        "check" => cmd_check(config, &dir, &args),
        "agents" => cmd_agents(config, dir, &args),
        "show" => cmd_show(&config, &args),
        "health" => cmd_health(config, dir, &args),
        "doctor" => cmd_doctor(config, &dir, &args),
        "submit" => cmd_submit(&config, &dir, &args),
        "serve" => cmd_serve(config, dir, &args),
        "stats" => cmd_stats(config, dir, &args),
        "assign" => cmd_assign(&config, &dir, &args),
        "run" => cmd_run(config, dir, &args),
        other => Err(format!("unknown command '{other}'. Try --help.")),
    }
}

/// The organization directory, which every command but `init`, `migrate` and
/// `schema` needs. It is named, never found.
fn org_of(args: &Args, command: &str) -> Result<PathBuf, String> {
    let spec = args.org.as_deref().ok_or_else(|| missing_org(command))?;
    match remote::parse_source(spec)? {
        remote::Source::Local(dir) => Ok(dir),
        remote::Source::Remote { .. } => Err(format!(
            "{command}: a remote organization is read-only. Clone it, edit your copy, and give {command} that directory."
        )),
    }
}

fn missing_org(command: &str) -> String {
    format!(
        "{command}: no organization given. Pass --org <dir>, the directory with config.yaml. `ensemble init --org <dir>` makes one."
    )
}

/// The organization to run: a directory, or a remote one that is pinned,
/// fetched and trusted.
fn resolve_org(args: &Args, command: &str) -> Result<remote::Resolved, String> {
    let spec = args.org.as_deref().ok_or_else(|| missing_org(command))?;
    remote::use_source(&remote::parse_source(spec)?)
}

/// Ask yes or no, or take `--yes` for the answer. Without a terminal and
/// without `--yes` there is nobody to ask, which is a refusal.
fn confirm(question: &str, args: &Args) -> Result<bool, String> {
    if has(args, "--yes") {
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

/// An `Ensemble` for `config`, keeping its state where `--state` says.
fn ensemble_for(config: Config, dir: PathBuf, args: &Args) -> Ensemble {
    let mut ensemble = Ensemble::new(config, Some(dir));
    if let Some(state) = &args.state {
        ensemble = ensemble.with_state(state.clone());
    }
    if let Some(usage) = &args.usage {
        ensemble = ensemble.with_usage_dir(usage.clone());
    }
    ensemble
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

    // Not errors: what the agents are allowed to do, for a second look.
    let risks = setup::risks(&config);
    if !risks.is_empty() {
        println!();
        println!("warnings:");
        for r in &risks {
            println!("  {}", r.what);
            if let Some(fix) = &r.fix {
                println!("    {fix}");
            }
        }
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

/// The value of an option that takes one, such as `--manager`.
fn opt<'a>(args: &'a Args, name: &str) -> Option<&'a str> {
    args.opts.get(name).map(String::as_str)
}

/// Was the switch `name`, such as `--global`, given?
fn has(args: &Args, name: &str) -> bool {
    args.rest.iter().any(|a| a == name)
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

/// Run a registration, saying first what it is. The manager's own words come
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

fn cmd_connect(args: &Args) -> Result<ExitCode, String> {
    let manager = opt(args, "--manager").ok_or(format!(
        "connect: needs --manager, the agent you talk to: {}",
        setup::MANAGERS.join(" or ")
    ))?;
    let spec = args.org.as_deref().ok_or_else(|| missing_org("connect"))?;
    let org = match remote::parse_source(spec)? {
        remote::Source::Local(dir) => absolute(&dir)?,
        remote::Source::Remote { url, sha, git_ref } => {
            // Pin it to the commit it is now, show what it would run, and
            // record the approval, before anything is registered.
            let sha = match sha {
                Some(sha) => sha,
                None => remote::resolve(&url, git_ref.as_deref())?,
            };
            approve(&url, &sha, args)?;
            PathBuf::from(remote::pinned(&url, &sha))
        }
    };
    let env_file = args.env_file.as_deref().map(absolute).transpose()?;
    let reg = setup::registration(
        manager,
        &setup::ensemble_command(),
        &org,
        env_file.as_deref(),
        opt(args, "--only"),
        has(args, "--global"),
    )?;
    run_registration(&reg)?;
    println!("Registered. Restart {manager} so that it starts the server.");
    Ok(ExitCode::SUCCESS)
}

fn cmd_setup(args: &Args, dir: &Path) -> Result<ExitCode, String> {
    let interactive = std::io::stdin().is_terminal() && !has(args, "--yes");
    let found = setup::detect(true);
    println!(
        "Found: claude {}, opencode {}{}",
        if found.claude { "yes" } else { "no" },
        if found.opencode { "yes" } else { "no" },
        if found.servers.is_empty() {
            String::new()
        } else {
            format!(
                ", local model servers: {}",
                found
                    .servers
                    .iter()
                    .map(|(n, u)| format!("{n} ({u})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    );
    let manager = match opt(args, "--manager") {
        Some(m) => m.to_string(),
        None => match (found.claude, found.opencode) {
            (true, false) => "claude-code".into(),
            (false, true) => "opencode".into(),
            (false, false) => {
                return Err("setup: no agent CLI found on PATH. Install `claude` (Claude Code) or `opencode`, then run `ensemble setup` again.".into());
            }
            (true, true) if interactive => ask(
                "Which agent do you talk to? (claude-code or opencode)",
                Some("claude-code"),
            )?,
            (true, true) => {
                return Err("setup: both claude and opencode are installed; say which one you talk to with --manager claude-code or --manager opencode.".into());
            }
        },
    };
    let org = match &args.org {
        Some(org) => PathBuf::from(org),
        None if interactive => {
            let home = setup::home().unwrap_or_else(|| dir.to_path_buf());
            PathBuf::from(ask(
                "Where should the organization live?",
                Some(&home.join("ensemble-org").display().to_string()),
            )?)
        }
        None => {
            return Err("setup: say where the organization lives with --org <dir>.".into());
        }
    };
    let org = absolute(&org)?;
    let path = config::config_path(&org);
    if path.exists() {
        println!("{} exists; keeping it.", path.display());
    } else {
        let starter = setup::starter(opt(args, "--starter").unwrap_or("software"))?;
        let text = setup::starter_org(starter, &found)?;
        std::fs::create_dir_all(&org)
            .map_err(|e| format!("cannot create {}: {e}", org.display()))?;
        std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        println!(
            "Wrote {}: the {} starter. Edit it, or change it with `ensemble add-agent` and `remove-agent`.",
            path.display(),
            starter.name
        );
    }
    let env_file = args.env_file.as_deref().map(absolute).transpose()?;
    let reg = setup::registration(
        &manager,
        &setup::ensemble_command(),
        &org,
        env_file.as_deref(),
        None,
        has(args, "--global"),
    )?;
    let registered = run_registration(&reg);
    if let Err(e) = &registered {
        println!("{e}");
    }
    println!();
    let config =
        config::load(&path, args.env_file.as_deref(), &SystemEnv).map_err(|e| e.to_string())?;
    let healthy = report_doctor(&config, &org, dir, args)?;
    println!();
    if registered.is_ok() && healthy {
        println!(
            "Done. In {manager}, try: \"have the team implement X\" (restart {manager} first if it was running)."
        );
        Ok(ExitCode::SUCCESS)
    } else {
        println!(
            "Setup is not finished: fix what is listed above, then run `ensemble doctor --org {}`.",
            org.display()
        );
        Ok(ExitCode::FAILURE)
    }
}

fn cmd_doctor(config: Config, dir: &Path, args: &Args) -> Result<ExitCode, String> {
    let org = absolute(&org_of(args, "doctor")?)?;
    Ok(if report_doctor(&config, &org, dir, args)? {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Print what is wrong, each with its fix, and say whether nothing was.
fn report_doctor(config: &Config, org: &Path, dir: &Path, args: &Args) -> Result<bool, String> {
    let mut findings = setup::structure(config, org, dir);
    let ensemble = ensemble_for(config.clone(), dir.to_path_buf(), args);
    for (name, result) in ensemble.health() {
        findings.push(setup::health_finding(&name, &result, config, org));
    }
    let mut all_ok = true;
    for f in &findings {
        let label = match f.level {
            setup::Level::Ok => "ok",
            setup::Level::Note => "note",
            setup::Level::Problem => {
                all_ok = false;
                "problem"
            }
        };
        println!("{label}: {}", f.what);
        if let (Some(fix), false) = (&f.fix, f.level == setup::Level::Ok) {
            println!("  fix: {fix}");
        }
    }
    Ok(all_ok)
}

fn cmd_add_agent(args: &Args) -> Result<ExitCode, String> {
    let (team, name) = args
        .rest
        .iter()
        .find(|a| !a.starts_with("--"))
        .and_then(|a| a.split_once('/'))
        .ok_or("add-agent: needs TEAM/NAME, such as software/coder")?;
    let adapter =
        opt(args, "--adapter").ok_or("add-agent: needs --adapter claude-code, opencode or llm")?;
    let org = org_of(args, "add-agent")?;
    let path = config::config_path(&org);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("add-agent: cannot read {}: {e}", path.display()))?;
    let block = config::edit::agent_block(
        name,
        &config::edit::NewAgent {
            adapter,
            prompt: opt(args, "--prompt"),
            model: opt(args, "--model"),
            backend: opt(args, "--backend"),
            writes: has(args, "--writes"),
        },
    )
    .map_err(|e| format!("add-agent: {e}"))?;
    let edited = config::edit::add_agent(
        &text,
        team,
        name,
        &block,
        opt(args, "--team-prompt").unwrap_or("Describe what this team is for."),
    )
    .map_err(|e| format!("add-agent: {e}"))?;
    config::parse(&edited, &path, &std::collections::BTreeMap::new()).map_err(|e| {
        format!("add-agent: the result would not load, so nothing was written: {e}")
    })?;
    std::fs::write(&path, edited).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!("added {team}/{name} to {}", path.display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_remove_agent(args: &Args) -> Result<ExitCode, String> {
    let (team, name) = args
        .rest
        .iter()
        .find(|a| !a.starts_with("--"))
        .and_then(|a| a.split_once('/'))
        .ok_or("remove-agent: needs TEAM/NAME, such as software/coder")?;
    let org = org_of(args, "remove-agent")?;
    let path = config::config_path(&org);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("remove-agent: cannot read {}: {e}", path.display()))?;
    let edited =
        config::edit::remove_agent(&text, team, name).map_err(|e| format!("remove-agent: {e}"))?;
    config::parse(&edited, &path, &std::collections::BTreeMap::new()).map_err(|e| {
        format!("remove-agent: the result would not load, so nothing was written: {e}")
    })?;
    std::fs::write(&path, edited).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!("removed {team}/{name} from {}", path.display());
    Ok(ExitCode::SUCCESS)
}

/// Fetch `url` at `sha`, say what its config would run, and, if the person
/// approves, record that they trust exactly this commit.
fn approve(url: &str, sha: &str, args: &Args) -> Result<(), String> {
    let dir = remote::fetch(url, sha)?;
    let config = config::load(
        &config::config_path(&dir),
        None,
        &std::collections::BTreeMap::<String, String>::new(),
    )
    .map_err(|e| format!("{url} at {sha} has no usable config: {e}"))?;
    println!("{url} at {sha}\n");
    print!("{}", remote::describe(&config));
    println!();
    if remote::is_trusted(url, sha)? {
        println!("Already trusted.");
        return Ok(());
    }
    if !confirm(
        "Trust this commit? It will run the commands above on this machine.",
        args,
    )? {
        return Err("not trusted, so nothing was done.".into());
    }
    remote::trust(url, sha)?;
    println!("Trusted {}.", remote::pinned(url, sha));
    Ok(())
}

/// `ensemble org trust <url>#<sha>` and `ensemble org update <url>#<sha>`.
fn cmd_org(args: &Args) -> Result<ExitCode, String> {
    let what = args
        .rest
        .first()
        .map(String::as_str)
        .ok_or("org: needs trust or update. Try --help.")?;
    let spec = args
        .rest
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .ok_or(format!(
            "org {what}: needs the organization, such as github.com/acme/org#<commit>"
        ))?;
    let (url, sha, git_ref) = match remote::parse_source(spec)? {
        remote::Source::Remote { url, sha, git_ref } => (url, sha, git_ref),
        remote::Source::Local(_) => {
            return Err(format!(
                "org {what}: a directory needs no trust; this is for git repositories"
            ));
        }
    };
    match what {
        "trust" => {
            let sha = sha.ok_or(format!(
                "org trust: pin the commit: {spec}#<commit>. `ensemble connect` pins and trusts in one step."
            ))?;
            approve(&url, &sha, args)?;
            Ok(ExitCode::SUCCESS)
        }
        "update" => {
            let old = sha;
            let git_ref = git_ref.or_else(|| opt(args, "--ref").map(str::to_string));
            let new = remote::resolve(&url, git_ref.as_deref())?;
            if old.as_deref() == Some(new.as_str()) {
                println!("Already at {new}.");
                return Ok(ExitCode::SUCCESS);
            }
            let dir = remote::fetch(&url, &new)?;
            match &old {
                Some(old) => match remote::diff(&dir, old, &new) {
                    Ok(diff) if diff.trim().is_empty() => {
                        println!("config.yaml did not change between {old} and {new}.")
                    }
                    Ok(diff) => println!("{diff}"),
                    Err(e) => println!("cannot show what changed since {old}: {e}"),
                },
                None => println!("Nothing to compare with: no commit was given to update from."),
            }
            println!(
                "\nThe new pin is {}.\nRead the changes above, then: ensemble org trust {}\nand register it again with ensemble connect --org {}",
                remote::pinned(&url, &new),
                remote::pinned(&url, &new),
                remote::pinned(&url, &new)
            );
            Ok(ExitCode::SUCCESS)
        }
        other => Err(format!("org: unknown '{other}'. Use trust or update.")),
    }
}

/// The task store of the working folder, kept where `--state` says.
fn store_for(dir: &Path, args: &Args) -> Result<tasks::Store, String> {
    Ok(tasks::Store::new(&state_for(dir, args)?))
}

fn cmd_tasks(args: &Args, dir: &Path) -> Result<ExitCode, String> {
    let store = store_for(dir, args)?;
    let entries = store.scan();
    let wanted = opt(args, "--status");
    let worker = ensemble::harness::lock::holder(store.root()).is_some();
    let mut shown = 0;
    for entry in &entries {
        let line = tasks::entry_line(Some(&store), entry, &entries, None, worker);
        let status = line.split(" · ").nth(2).unwrap_or("");
        if wanted.is_none_or(|w| status.starts_with(w)) {
            println!("{line}");
            shown += 1;
        }
    }
    if shown == 0 {
        println!(
            "{}",
            match wanted {
                Some(w) => format!("No tasks with status '{w}'."),
                None => format!("No tasks in {}.", store.tasks_dir().display()),
            }
        );
    }
    if wanted.is_none()
        && !worker
        && let Some(idle) = tasks::idle_line(&entries, Some(&store))
    {
        println!("{idle}");
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_task(args: &Args, dir: &Path) -> Result<ExitCode, String> {
    let id = args
        .rest
        .first()
        .ok_or("task: needs a task id. `ensemble tasks` lists them.")?;
    let store = store_for(dir, args)?;
    let entries = store.scan();
    let Some(entry) = entries.iter().find(|e| e.id() == id) else {
        return Err(format!(
            "task: no task '{id}'. `ensemble tasks` lists them."
        ));
    };
    let worker = ensemble::harness::lock::holder(store.root()).is_some();
    println!(
        "{}",
        tasks::entry_line(Some(&store), entry, &entries, None, worker)
    );
    if let tasks::Entry::Task(t, _) = entry {
        println!("\n{}", t.prompt());
        if let Some(repo) = &t.repo {
            println!("\nrepo: {repo}");
        }
        if let Some(gate) = &t.gate {
            println!("gate: {} ({} attempts)", gate.name, gate.attempts);
        }
    }
    if let Some(q) = store.open_question(id) {
        println!("\nWaiting for an answer to: {q}\nAnswer it with: ensemble answer {id} <text>");
    }
    if let Some(outcome) = store.outcome(id) {
        println!("\n{}", outcome.text);
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_answer(args: &Args, dir: &Path) -> Result<ExitCode, String> {
    let id = args
        .rest
        .first()
        .ok_or("answer: needs a task id and the answer")?;
    let answer = args.rest[1..].join(" ");
    if answer.trim().is_empty() {
        return Err(
            "answer: needs the answer, e.g. `ensemble answer 0001-add-retry use ureq`".into(),
        );
    }
    store_for(dir, args)?.answer(id, &answer)?;
    println!("answered {id}; a running `ensemble mcp` carries the task on.");
    Ok(ExitCode::SUCCESS)
}

/// A comma-separated option as a list.
fn list_opt(args: &Args, name: &str) -> Vec<String> {
    opt(args, name)
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn cmd_submit(config: &Config, dir: &Path, args: &Args) -> Result<ExitCode, String> {
    // An agent and a title, or a title and `--team`, which is the backlog.
    let positional: Vec<&String> = args.rest.iter().filter(|a| !a.starts_with("--")).collect();
    let (agent, title): (String, &String) = match (opt(args, "--team"), positional.as_slice()) {
        (None, [agent, title, ..]) => ((*agent).clone(), *title),
        (Some(_), [title, ..]) => (String::new(), *title),
        _ => {
            return Err("submit: needs an agent and a title: `ensemble submit software/coder \"Add retry\" --text ...`, or a title and --team for the backlog".into());
        }
    };
    let mut text = opt(args, "--text").map(str::to_string);
    if text.is_none() && !std::io::stdin().is_terminal() {
        use std::io::Read;
        let mut piped = String::new();
        std::io::stdin()
            .read_to_string(&mut piped)
            .map_err(|e| format!("submit: cannot read the task from stdin: {e}"))?;
        text = Some(piped);
    }
    let text = text
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .ok_or("submit: needs the task: --text <text>, or pipe it in")?;
    if title.trim().is_empty() || title.contains('\n') {
        return Err("submit: the title is one non-empty line".into());
    }
    let (qualified, team) = if agent.is_empty() {
        let team = opt(args, "--team").unwrap_or_default();
        if !config.teams.contains_key(team) {
            let known: Vec<&str> = config.teams.keys().map(String::as_str).collect();
            return Err(format!(
                "submit: no team '{team}'. Teams: {}.",
                known.join(", ")
            ));
        }
        (String::new(), team.to_string())
    } else {
        let qualified = config.qualify(&agent).map_err(|e| format!("submit: {e}"))?;
        let team = config.agents[&qualified].team.clone();
        (qualified, team)
    };
    let gate = match opt(args, "--gate") {
        None => None,
        Some(spec) => {
            let (name, attempts) = spec.split_once(':').unwrap_or((spec, "1"));
            let attempts: u32 = attempts
                .parse()
                .ok()
                .filter(|n| *n >= 1)
                .ok_or("submit: --gate is name:attempts, with attempts at least 1")?;
            if config.gate(&team, name).is_none() {
                return Err(format!("submit: no gate '{name}' for team '{team}'"));
            }
            Some(tasks::GateRef {
                name: name.to_string(),
                attempts,
            })
        }
    };
    let repo = opt(args, "--repo").map(str::to_string);
    if let Some(repo) = &repo {
        ensemble::workspace::resolve_repo(dir, repo).map_err(|e| format!("submit: {e}"))?;
    }
    let ticket = tasks::Ticket {
        title: title.trim().to_string(),
        agent: qualified,
        team: team.clone(),
        context: args.context.clone(),
        goal: opt(args, "--goal").map(str::to_string),
        constraints: list_opt(args, "--constraints"),
        files: list_opt(args, "--files"),
        done_when: list_opt(args, "--done-when"),
        after: list_opt(args, "--after"),
        repo,
        gate,
        text,
        ..Default::default()
    };
    let store = store_for(dir, args)?;
    let id = store
        .create(&ticket)
        .map_err(|e| format!("submit: cannot write the task: {e}"))?;
    println!(
        "task {id} written in {}. A running `ensemble mcp` for this folder picks it up.",
        store.tasks_dir().join(&id).display()
    );
    Ok(ExitCode::SUCCESS)
}

/// Set by SIGINT and SIGTERM, so that `serve` can stop its tasks cleanly.
static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn cmd_serve(config: Config, dir: PathBuf, args: &Args) -> Result<ExitCode, String> {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    let ensemble = ensemble_for(config, dir, args);
    ensemble::harness::serve::run(
        &ensemble,
        &STOP,
        &|line| eprintln!("ensemble: {line}"),
        has(args, "--exit-when-idle"),
    )?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_stats(config: Config, dir: PathBuf, args: &Args) -> Result<ExitCode, String> {
    let ensemble = ensemble_for(config, dir, args);
    let records_dir = ensemble
        .records_dir()
        .ok_or("stats: cannot tell where runs are recorded")?;
    let since = match opt(args, "--since") {
        Some(text) => {
            let ago =
                ensemble::records::parse_duration(text).map_err(|e| format!("--since: {e}"))?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            Some(now.saturating_sub(ago))
        }
        None => None,
    };
    let summaries = ensemble::records::summarize(&ensemble::records::read(&records_dir), since);
    if has(args, "--json") {
        println!("{}", ensemble::records::json(&summaries));
    } else {
        print!("{}", ensemble::records::table(&summaries));
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_assign(config: &Config, dir: &Path, args: &Args) -> Result<ExitCode, String> {
    let (id, agent) =
        match args.rest.as_slice() {
            [id, agent, ..] => (id, agent),
            _ => return Err(
                "assign: needs a task id and an agent: `ensemble assign 0003-fix software/coder`"
                    .into(),
            ),
        };
    let store = store_for(dir, args)?;
    if !store.exists(id) {
        return Err(format!(
            "assign: no task '{id}'. `ensemble tasks` lists them."
        ));
    }
    let to = config.qualify(agent).map_err(|e| format!("assign: {e}"))?;
    if store.outcome(id).is_some() {
        return Err(format!("assign: {id} has ended; submit it again."));
    }
    store
        .assign(id, &to)
        .map_err(|e| format!("assign: cannot write: {e}"))?;
    println!("{id} is assigned to {to}.");
    Ok(ExitCode::SUCCESS)
}
