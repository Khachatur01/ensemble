//! The `ensemble` binary.
//!
//! `ensemble mcp` is the one a harness spawns: it offers the helper team's agents as MCP
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

mod daemon;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
ensemble — a helper team of coding agents behind one MCP server

USAGE:
    ensemble <COMMAND> [OPTIONS]

COMMANDS:
    mcp [--node FILE]       Serve the agents over MCP on stdio. This is what an
                            agent (Claude Code, OpenCode, ...) spawns, with the
                            project as its working directory; it is not meant
                            to be run by hand. With --team it runs that helper
                            team in this session alone; without, it is a
                            session of the running node (`node serve`), whose
                            tasks go on after the session ends.
    init [--force|--stdout] Write a sample helper team: config.yaml in the --team
                            directory, with every setting. Refuses to overwrite
                            it unless --force; --stdout prints it instead.
    migrate <FILE>          Rewrite a version 1 or 2 config as a version 3
                            helper team in --team: every agent in one flat list.
    setup [--team DIR] [--manager M] [--starter S] [--yes]
                            Set everything up: find the agent CLIs, write a
                            starter helper team, register it with the agent you
                            talk to and check it. Asks what it needs.
    connect --manager M [--team DIR | --node FILE] [--hub DIR] [--env-file F] [--global] [--hooks]
                            Register Ensemble with the agent you talk to:
                            claude-code, opencode or codex. Prints the command
                            it runs. With --team, each session runs that team;
                            without, sessions reach your node. --hooks (Claude Code, Codex; needs --hub)
                            also adds session hooks that put the vault's notes
                            about each prompt into the conversation: in the
                            project's .claude/settings.local.json or
                            .codex/hooks.json, or your own with --global.
    hub init [--server] [--force]
                            Write a sample hub: hub.yaml in the --hub directory,
                            with every setting, and make its knowledge vault a
                            git repository. --server writes a hub server for a
                            team instead of a hub for you alone.
    node init [--node FILE] [--force|--stdout]
                            Write a node: ~/.config/ensemble/node.yaml, or FILE.
                            It names the node, says whom it works for (private:
                            you; shared: also those whose role has `compute`),
                            the helper team it runs, and what it offers through
                            a hub: agents of that team, models.
    node serve [--node FILE] [--hub HUB]
                            Run that node until stopped (Ctrl-C or SIGTERM):
                            `ensemble mcp` sessions from any folder reach its
                            team, and each folder's tasks go on between them.
                            With a hub server it also takes your tasks from the
                            hub (into the folder of the project's name where a
                            session last ran), and runs the agents and serves
                            the models it offers. A computer may run any number
                            of nodes, each from its own file.
    nodes --hub URL         The nodes of a hub server you may use now, with the
                            agents and models each offers.
    hub check               Load and validate hub.yaml, and say which model
                            would distill shares now.
    hub serve               Run the --hub as a server for a team, until stopped
                            (Ctrl-C or SIGTERM): members' nodes connect, share
                            to and recall from its vault, as their roles allow.
    hub user add NAME --role R | list | password NAME | tracker NAME [TRACKER_NAME]
             | approve NAME --role R | role NAME --role R | token NAME
             | revoke NAME | remove NAME
                            A hub server's users. `add` and `token` print a new
                            token once; only its hash is kept. `password` sets a
                            password (typed without echo, or on stdin), ending
                            the user's sessions. `approve` lets in someone who
                            signed up; `remove` turns them down.
    share --title T [--text T]
                            Keep knowledge in the --hub vault, as an agent's
                            `share` tool does; the text is read from stdin
                            when --text is not given.
    login URL --user NAME | --token T
                            Sign in to a hub server (ws:// or wss://): with your
                            account (the password is asked, or read from stdin),
                            which makes an access token for this computer, or
                            with a token made elsewhere. The token is kept in
                            ~/.config/ensemble/hubs.json, readable only by you.
                            Then --hub URL works like --hub <dir>.
    logout URL              Forget a hub server's token.
    inbox [--once]          The issues assigned to you in the --hub tracker
                            with its label, where each stands and the task it
                            became. --once (needs --team) first makes new
                            issues into tasks and reports tasks back, as a
                            running `ensemble mcp` or `serve` does.
    recall TOPIC            Print the notes of the --hub vault about TOPIC, as
                            an agent's `recall` tool gets them.
    hook --hub HUB          What a Claude Code or Codex session hook runs: reads
                            the event on stdin and prints the notes about the
                            prompt (or, at the start, the project) not given in
                            the session yet. Set up by `connect --hooks`.
    knowledge status        Shares, those not yet distilled, notes, and what
                            waits in review/.
    knowledge distill       Turn every pending share into notes now, instead of
                            waiting for `ensemble mcp` or `serve` to.
    knowledge rebuild [--yes]
                            Delete the notes and make them again from every
                            share, in one commit.
    doctor                  Say what is wrong with the helper team and the
                            agents it needs, and how to fix each thing.
    add-agent NAME --adapter A [--model M] [--backend B] [--prompt P] [--writes]
                            Add an agent to the team, every setting written.
    remove-agent NAME       Remove an agent from the team.
    team trust URL#COMMIT [--yes]
                            Read what a remote helper team would run on this
                            machine and, if you approve, trust that commit.
    team update URL#COMMIT [--ref R]
                            Show what changed in a remote helper team since
                            COMMIT and print its new pin. Trusts nothing.
    review ID               The branch, the change and the result of a task that
                            worked in a git worktree. Changes nothing.
    diff ID                 The whole change on that branch.
    merge ID [--squash] [--cleanup]
                            Merge that branch into the repository, once the task
                            is done. Refuses a dirty repository and a conflict,
                            and undoes the latter. --cleanup then removes the
                            task's worktree and branch. Only you run this: the
                            manager has no tool for it.
    tasks [--status S]      List the tasks of this working folder and where each
                            stands. Reads files; starts nothing.
    task ID                 Show one task: its ticket, where it stands, its
                            question or its result.
    submit AGENT TITLE --text T   (or TITLE --backlog: put it in the backlog)
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
    show <AGENT> [--probe]  Print how an agent is started: its command line and
                            the config handed to its CLI, API keys hidden, and
                            its models in order. --probe asks their backends
                            which one a run would use now.
    check                   Load and validate the config, then report the helper
                            team.
    schema                  Print the JSON Schema for config.yaml, so an editor
                            can complete and check it. Needs no config.
    agents                  List the agents and what runs each.
    health                  Check that every agent CLI starts and every LLM
                            backend in use answers and serves its models.
    run <AGENT> <TASK>      Send one task to one agent and print the answer.

OPTIONS:
    -t, --team <DIR|URL>    The helper team: the directory with config.yaml, its
                            llm_backends and agents. Nothing is searched for. A
                            git repository is github.com/acme/team#COMMIT (a
                            pin, which it must be and be trusted), or @TAG for
                            `connect`, which pins it. --org is its old name.
        --hub <DIR|URL>     The hub: the directory with hub.yaml, which keeps
                            your knowledge, or a hub server's ws:// or wss://
                            URL after `ensemble login`. Without it there is no
                            knowledge.
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
    if cfg!(windows) {
        eprintln!(
            "ensemble: Windows is not supported: it starts agent CLIs as process groups and runs gates with `sh -c`. Use WSL."
        );
        return ExitCode::FAILURE;
    }
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
    team: Option<String>,
    /// The hub's directory, with `hub.yaml`. None: no hub, so no knowledge.
    hub: Option<PathBuf>,
    /// Where usage counters live, when the helper team is remote.
    usage: Option<PathBuf>,
    env_file: Option<PathBuf>,
    state: Option<PathBuf>,
    dir: Option<PathBuf>,
    context: Option<String>,
    /// The options that take a value and belong to one command: `--manager`,
    /// `--adapter`, `--model`, `--backend`, `--starter`, `--prompt`, and those
    /// of `submit`.
    opts: std::collections::BTreeMap<String, String>,
}

fn parse_args(argv: Vec<String>) -> Result<Option<Args>, String> {
    let mut command: Option<String> = None;
    let mut rest = Vec::new();
    let mut team = None;
    let mut hub = None;
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
            "-t" | "--team" => team = Some(value("--team")?),
            "-o" | "--org" => {
                eprintln!(
                    "ensemble: --org is now --team; --org stops working in the next release."
                );
                team = Some(value("--org")?);
            }
            "--hub" => hub = Some(PathBuf::from(value("--hub")?)),
            "--env-file" => env_file = Some(PathBuf::from(value("--env-file")?)),
            "--state" => state = Some(PathBuf::from(value("--state")?)),
            "-d" | "--dir" => dir = Some(PathBuf::from(value("--dir")?)),
            "--context" => context = Some(value("--context")?),
            "--manager" | "--adapter" | "--model" | "--backend" | "--starter" | "--prompt"
            | "--status" | "--text" | "--goal" | "--repo" | "--gate" | "--since" | "--after"
            | "--files" | "--constraints" | "--done-when" | "--ref" | "--title" | "--role"
            | "--token" | "--user" | "--node" => {
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
            team,
            hub,
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
        "review" | "diff" | "merge" => return cmd_review(&args, &dir),
        "tasks" => return cmd_tasks(&args, &dir),
        "task" => return cmd_task(&args, &dir),
        "answer" => return cmd_answer(&args, &dir),
        "setup" => return cmd_setup(&args, &dir),
        "connect" => return cmd_connect(&args),
        "add-agent" => return cmd_add_agent(&args),
        "remove-agent" => return cmd_remove_agent(&args),
        _ => {}
    }

    if args.command == "team" || args.command == "org" {
        return cmd_team(&args);
    }
    if args.command == "hub" {
        return cmd_hub(&args);
    }
    if args.command == "node" {
        return cmd_node(&args);
    }
    if args.command == "nodes" {
        return cmd_nodes(&args);
    }
    if args.command == "share" {
        return cmd_share(&args, &dir);
    }
    if args.command == "knowledge" {
        return cmd_knowledge(&args, &dir);
    }
    if args.command == "inbox" {
        return cmd_inbox(&args);
    }
    if args.command == "recall" {
        if args.hub.is_none() {
            hub_of(&args, "recall")?;
        }
        let knowledge = knowledge_for(&args, &dir)?.ok_or("recall: the hub keeps no knowledge")?;
        let query = args.rest.join(" ");
        if query.trim().is_empty() {
            return Err(
                "recall: needs a topic, such as `ensemble recall --hub <dir> deploys`".into(),
            );
        }
        let notes = knowledge
            .access()
            .call(
                ensemble::knowledge::RECALL,
                &serde_json::json!({ "query": query }),
            )
            .expect("recall is a knowledge tool")?;
        println!("{notes}");
        return Ok(ExitCode::SUCCESS);
    }
    if args.command == "hook" {
        return cmd_hook(&args);
    }
    if args.command == "mcp" && args.team.is_none() {
        return cmd_mcp_bridge(&args, &dir);
    }
    if args.command == "login" {
        return cmd_login(&args);
    }
    if args.command == "logout" {
        return cmd_logout(&args);
    }

    let resolved = resolve_team(&args, &args.command)?;
    args.usage = resolved.usage.clone();
    let config_path = config::config_path(&resolved.dir);
    let config = config::load(&config_path, args.env_file.as_deref(), &SystemEnv)
        .map_err(|e| e.to_string())?;

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

/// The helper team's directory, which every command but `init`, `migrate` and
/// `schema` needs. It is named, never found.
fn team_of(args: &Args, command: &str) -> Result<PathBuf, String> {
    let spec = args.team.as_deref().ok_or_else(|| missing_team(command))?;
    match remote::parse_source(spec)? {
        remote::Source::Local(dir) => Ok(dir),
        remote::Source::Remote { .. } => Err(format!(
            "{command}: a remote helper team is read-only. Clone it, edit your copy, and give {command} that directory."
        )),
    }
}

fn missing_team(command: &str) -> String {
    format!(
        "{command}: no helper team given. Pass --team <dir>, the directory with config.yaml. `ensemble init --team <dir>` makes one."
    )
}

/// The helper team to run: a directory, or a remote one that is pinned,
/// fetched and trusted.
fn resolve_team(args: &Args, command: &str) -> Result<remote::Resolved, String> {
    let spec = args.team.as_deref().ok_or_else(|| missing_team(command))?;
    remote::use_source(&remote::parse_source(spec)?)
}

/// The hub's directory, for a command that needs one.
fn hub_of(args: &Args, command: &str) -> Result<PathBuf, String> {
    if let Some(url) = hub_url(args) {
        return Err(format!(
            "{command}: needs the hub's directory, with hub.yaml; {url} is a hub server, and this runs on the server itself"
        ));
    }
    args.hub.clone().ok_or_else(|| {
        format!(
            "{command}: no hub given. Pass --hub <dir>, the directory with hub.yaml. `ensemble hub init --hub <dir>` makes one."
        )
    })
}

/// The hub server `--hub` names, when it is a `ws://` or `wss://` URL rather
/// than a directory.
fn hub_url(args: &Args) -> Option<String> {
    let hub = args.hub.as_ref()?.to_str()?;
    (hub.starts_with("ws://") || hub.starts_with("wss://")).then(|| hub.to_string())
}

/// A hub's knowledge: embedded in this node, or on a hub server.
enum Knowledge {
    Local(Box<ensemble::knowledge::Memory>),
    Remote(Box<ensemble::link::RemoteMemory>),
}

impl Knowledge {
    fn access(&self) -> &dyn ensemble::knowledge::KnowledgeAccess {
        match self {
            Knowledge::Local(m) => m.as_ref(),
            Knowledge::Remote(r) => r.as_ref(),
        }
    }

    /// The knowledge this process distills, which only an embedded hub has.
    fn local(&self) -> Option<&ensemble::knowledge::Memory> {
        match self {
            Knowledge::Local(m) => Some(m.as_ref()),
            Knowledge::Remote(_) => None,
        }
    }
}

/// The knowledge of the hub `--hub` names, embedded or on a server, for work
/// in `dir`.
fn knowledge_for(args: &Args, dir: &Path) -> Result<Option<Knowledge>, String> {
    let Some(url) = hub_url(args) else {
        return Ok(memory_for(args, dir)?.map(|m| Knowledge::Local(Box::new(m))));
    };
    let tokens = ensemble::link::Tokens::load()?;
    let token = tokens.get(&url).ok_or_else(|| {
        format!("no token for the hub {url}. Sign in first: ensemble login {url} --token <token>")
    })?;
    let project = dir.file_name().map(|n| n.to_string_lossy().into_owned());
    Ok(Some(Knowledge::Remote(Box::new(
        ensemble::link::RemoteMemory::new(&url, token, project),
    ))))
}

/// `ensemble hook --hub H`: what a CLI's session hook prints, the vault's notes
/// about the prompt or the project, each once per session. It never fails the
/// CLI's turn: a problem is said on stderr, and nothing is printed.
fn cmd_hook(args: &Args) -> Result<ExitCode, String> {
    if let Err(e) = hook(args) {
        eprintln!("ensemble: hook: {e}");
    }
    Ok(ExitCode::SUCCESS)
}

fn hook(args: &Args) -> Result<(), String> {
    use ensemble::hooks::{Given, MAX_PRINTED, fresh_notes, read_event};
    use std::io::Read;
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("cannot read the event: {e}"))?;
    let event = read_event(&input)?;
    let mut given = Given::load(&state::root()?.join("hooks"), &event.session);
    if event.forgets() {
        given.notes.clear();
    }
    let Some(topic) = event.topic() else {
        return given.save();
    };
    let dir = match &event.cwd {
        Some(cwd) => cwd.clone(),
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    let Some(knowledge) = knowledge_for(args, &dir)? else {
        return Ok(());
    };
    let answer = match knowledge.access().call(
        ensemble::knowledge::RECALL,
        &serde_json::json!({ "query": topic }),
    ) {
        Some(answer) => answer?,
        None => return Ok(()),
    };
    let answer = ensemble::knowledge::recalled_within(&answer, MAX_PRINTED);
    if let Some(notes) = fresh_notes(&answer, &mut given.notes) {
        print!("{notes}");
    }
    given.save()
}

/// `ensemble login URL [--token T]`: check a hub server's token and keep it.
fn cmd_login(args: &Args) -> Result<ExitCode, String> {
    let url = args
        .rest
        .first()
        .filter(|u| u.starts_with("ws://") || u.starts_with("wss://"))
        .ok_or("login: needs the hub's URL, such as wss://hub.example.com")?;
    let token = match (opt(args, "--user"), opt(args, "--token")) {
        (Some(user), None) => token_from_account(url, user)?,
        (None, Some(t)) => t.to_string(),
        _ => {
            return Err(
                "login: give either --user NAME (your password is asked) or --token TOKEN".into(),
            );
        }
    };
    let hello = ensemble::link::RemoteMemory::new(url, &token, None)
        .hello()
        .map_err(|e| format!("login: {e}"))?;
    let mut tokens = ensemble::link::Tokens::load()?;
    tokens.set(url, &token);
    tokens.save()?;
    println!(
        "signed in to {url} as {} ({}: {})",
        hello.user,
        hello.role,
        hello.permissions.join(", ")
    );
    Ok(ExitCode::SUCCESS)
}

/// Log in to the hub at `url` as `user`, with the password typed or on stdin,
/// make an access token for this computer, and log out again.
fn token_from_account(url: &str, user: &str) -> Result<String, String> {
    let base = url
        .replacen("wss://", "https://", 1)
        .replacen("ws://", "http://", 1);
    let base = base.trim_end_matches('/');
    let password = read_secret(&format!("Password for {user} at {url}: "))?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into();
    let call = |path: &str, bearer: Option<&str>, body: serde_json::Value| {
        let mut req = agent
            .post(format!("{base}{path}"))
            .header("content-type", "application/json");
        if let Some(b) = bearer {
            req = req.header("authorization", &format!("Bearer {b}"));
        }
        let mut resp = req
            .send(body.to_string())
            .map_err(|e| format!("login: cannot reach {base}: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        if (200..300).contains(&status) {
            Ok(value)
        } else {
            Err(format!(
                "login: the hub said {status}: {}",
                value["error"].as_str().unwrap_or(&text)
            ))
        }
    };
    let session = call(
        "/api/v1/login",
        None,
        serde_json::json!({ "name": user, "password": password }),
    )?;
    let session = session["token"]
        .as_str()
        .ok_or("login: the hub sent no session")?
        .to_string();
    let made = call(
        "/api/v1/me/tokens",
        Some(&session),
        serde_json::json!({ "name": format!("ensemble on {}", hostname()) }),
    );
    let _ = call("/api/v1/logout", Some(&session), serde_json::json!({}));
    made?["token"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| "login: the hub sent no token".to_string())
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

/// `ensemble logout URL`: forget a hub server's token.
fn cmd_logout(args: &Args) -> Result<ExitCode, String> {
    let url = args.rest.first().ok_or("logout: needs the hub's URL")?;
    let mut tokens = ensemble::link::Tokens::load()?;
    if tokens.remove(url) {
        tokens.save()?;
        println!("signed out of {url}");
    } else {
        println!("no token kept for {url}");
    }
    Ok(ExitCode::SUCCESS)
}

/// `ensemble hub init` and `ensemble hub check`.
fn cmd_hub(args: &Args) -> Result<ExitCode, String> {
    let what = args
        .rest
        .first()
        .map(String::as_str)
        .ok_or("hub: needs init, check, user or serve. Try --help.")?;
    match what {
        "init" => {
            let dir = hub_of(args, "hub init")?;
            let path = config::hub_path(&dir);
            if path.exists() && !has(args, "--force") {
                return Err(format!(
                    "{} already exists. Pass --force to overwrite it.",
                    path.display()
                ));
            }
            let (sample, what) = if has(args, "--server") {
                (
                    config::hub::HUB_SERVER_SAMPLE,
                    "a sample hub server for a team",
                )
            } else {
                (config::hub::HUB_SAMPLE, "a sample hub for you alone")
            };
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            std::fs::write(&path, sample)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            println!("wrote {}: {what}, with every setting", path.display());
            let hub = config::parse_hub(sample, &path, &SystemEnv).map_err(|e| e.to_string())?;
            if let Some(knowledge) = &hub.knowledge {
                ensemble::knowledge::Vault::new(&knowledge.vault).init()?;
                println!(
                    "made the knowledge vault, a git repository: {}",
                    knowledge.vault.display()
                );
            }
            println!(
                "Name yourself as `owner`, choose the models that distill, then run `ensemble hub check --hub {}`.",
                dir.display()
            );
            Ok(ExitCode::SUCCESS)
        }
        "check" => {
            let dir = hub_of(args, "hub check")?;
            let hub = config::load_hub(
                &config::hub_path(&dir),
                args.env_file.as_deref(),
                &SystemEnv,
            )
            .map_err(|e| e.to_string())?;
            println!("{}: valid, for {}", hub.source.display(), hub.owner);
            let Some(knowledge) = &hub.knowledge else {
                println!("knowledge: not used (no `knowledge` block)");
                return Ok(ExitCode::SUCCESS);
            };
            println!("knowledge vault: {}", knowledge.vault.display());
            if !ensemble::knowledge::Vault::new(&knowledge.vault).exists() {
                println!("  it is not a git repository yet; `ensemble hub init` makes it");
                return Ok(ExitCode::FAILURE);
            }
            let candidates: Vec<(&config::LlmBackend, &str)> = knowledge
                .distill
                .fallback
                .candidates
                .iter()
                .filter_map(|c| {
                    hub.llm_backends
                        .get(&c.backend)
                        .map(|b| (b, c.model.as_str()))
                })
                .collect();
            match ensemble::backends::resolve::Resolver::default().choose(&candidates) {
                Ok(i) => {
                    let c = &knowledge.distill.fallback.candidates[i];
                    println!(
                        "distill: shares would be distilled by {} on {}",
                        c.model, c.backend
                    );
                    Ok(ExitCode::SUCCESS)
                }
                Err(refusals) => {
                    println!("distill: no model can be used now:");
                    for r in refusals {
                        println!("  {} / {}: {}", r.backend, r.model, r.why);
                    }
                    Ok(ExitCode::FAILURE)
                }
            }
        }
        "user" => cmd_hub_user(args),
        "serve" => cmd_hub_serve(args),
        other => Err(format!(
            "hub: unknown '{other}'. Use init, check, user or serve."
        )),
    }
}

/// `ensemble node init`: write `~/.config/ensemble/node.yaml`, which maps the
/// projects of a hub server's tasks to folders on this computer.
/// The node file `--node` names, else ~/.config/ensemble/node.yaml.
fn node_file(args: &Args) -> Result<PathBuf, String> {
    match opt(args, "--node") {
        Some(file) => absolute(Path::new(file)),
        None => Ok(config::node_path(&state::config_root()?)),
    }
}

fn cmd_node(args: &Args) -> Result<ExitCode, String> {
    match args.rest.first().map(String::as_str) {
        Some("init") => {
            if has(args, "--stdout") {
                print!("{}", config::node::NODE_SAMPLE);
                return Ok(ExitCode::SUCCESS);
            }
            let path = node_file(args)?;
            if path.exists() && !has(args, "--force") {
                return Err(format!(
                    "{} already exists. Pass --force to overwrite it.",
                    path.display()
                ));
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            }
            std::fs::write(&path, config::node::NODE_SAMPLE)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            println!(
                "wrote {}: name the node, say whom it works for and what it offers",
                path.display()
            );
            Ok(ExitCode::SUCCESS)
        }
        Some("serve") => cmd_node_serve(args),
        _ => Err("node: needs init or serve. Try --help.".into()),
    }
}

/// `ensemble node serve [--node FILE] [--hub HUB]`: run a node until stopped.
/// It serves `ensemble mcp` sessions from any folder with its team; with a hub
/// server, it also takes your tasks from the hub and offers what it offers.
fn cmd_node_serve(args: &Args) -> Result<ExitCode, String> {
    let path = node_file(args)?;
    let node = config::load_node(&path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "node serve: no node file at {}; `ensemble node init` writes one",
                path.display()
            )
        })?;
    let team = match &node.team {
        Some(dir) => Some(
            config::load(
                &config::config_path(dir),
                args.env_file.as_deref(),
                &SystemEnv,
            )
            .map_err(|e| format!("node serve: team: {e}"))?,
        ),
        None => None,
    };
    let dir = state::node_dir(&node.name)?;
    let _lock = ensemble::lock::acquire(&dir).map_err(|pid| {
        format!(
            "node serve: process {pid} already runs the node '{}'",
            node.name
        )
    })?;
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    let say = |line: &str| eprintln!("ensemble: {line}");
    let daemon = daemon::Daemon::new(args, node.clone(), team.clone(), &dir, &say);

    // With a hub server: the link that offers what the node offers, and the
    // tasks the hub sends you.
    let url = hub_url(args);
    let token = match &url {
        Some(url) => Some(
            ensemble::link::Tokens::load()?
                .get(url)
                .ok_or_else(|| {
                    format!("node serve: no token for the hub {url}. Sign in first: ensemble login {url} --user <name>")
                })?
                .to_string(),
        ),
        None => None,
    };
    let worker = match (&url, &token) {
        (Some(url), Some(token)) if node.offers.runs.is_some() || node.offers.models.is_some() => {
            Some(
                ensemble::node::Worker::new(
                    url,
                    token,
                    node.clone(),
                    team.clone(),
                    dir.join("runs"),
                )
                .map_err(|e| format!("node serve: {e}"))?,
            )
        }
        _ => None,
    };
    let inbox = match (&url, &token, &team) {
        (Some(url), Some(token), Some(team)) => Some(ensemble::inbox::NodeInbox::new(
            url,
            token,
            daemon.folders().clone(),
            team.agent_names().into_iter().map(String::from).collect(),
            dir.join("inbox"),
        )),
        _ => None,
    };
    // An embedded hub's own workers: shares into notes, issues into tasks.
    let agents: Vec<String> = team
        .as_ref()
        .map(|t| t.agent_names().into_iter().map(String::from).collect())
        .unwrap_or_default();
    let local_inbox = inbox_for(args, agents)?;
    let distill = match hub_url(args) {
        None => memory_for(args, &dir)?,
        Some(_) => None,
    };

    let socket = state::node_socket(&dir);
    let refused = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        let stop = &STOP;
        // What is not async yet runs on threads of its own.
        if let Some(inbox) = &local_inbox {
            scope.spawn(move || inbox.work(stop, &say));
        }
        if let Some(memory) = &distill {
            scope.spawn(move || memory.work(stop, &say));
        }
        let link = async {
            if let Some(worker) = &worker
                && let Err(e) = worker.work(stop, &say).await
            {
                *refused.lock().unwrap() = Some(e);
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        };
        let tasks = async {
            if let Some(inbox) = &inbox {
                inbox.work(stop, &say).await;
            }
        };
        let (served, (), ()) = ensemble::runtime::shared()
            .block_on(async { tokio::join!(daemon.serve(&socket), link, tasks) });
        STOP.store(true, std::sync::atomic::Ordering::Relaxed);
        served
    })
    .map_err(|e| format!("node serve: {e}"))?;
    if let Some(e) = refused.into_inner().unwrap() {
        return Err(format!("node serve: the hub refused this node: {e}"));
    }
    say("stopped");
    Ok(ExitCode::SUCCESS)
}

/// `ensemble mcp` with no `--team`: a session of the running node, which
/// `--node` names (else ~/.config/ensemble/node.yaml), in `dir`.
fn cmd_mcp_bridge(args: &Args, dir: &Path) -> Result<ExitCode, String> {
    let path = node_file(args)?;
    let node = config::load_node(&path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "mcp: no --team, and no node file at {} to reach a node instead. Pass --team <dir>, or write a node with `ensemble node init` and start it with `ensemble node serve`",
                path.display()
            )
        })?;
    let socket = state::node_socket(&state::node_dir(&node.name)?);
    ensemble::runtime::shared()
        .block_on(daemon::bridge(&socket, dir, &node.name))
        .map_err(|e| format!("mcp: {e}"))?;
    Ok(ExitCode::SUCCESS)
}

/// `ensemble nodes --hub URL`: the nodes of a hub server you may use now.
fn cmd_nodes(args: &Args) -> Result<ExitCode, String> {
    let url = hub_url(args).ok_or("nodes: needs --hub, a hub server's ws:// or wss:// address")?;
    let tokens = ensemble::link::Tokens::load()?;
    let token = tokens.get(&url).ok_or_else(|| {
        format!(
            "nodes: no token for the hub {url}. Sign in first: ensemble login {url} --user <name>"
        )
    })?;
    let listed = ensemble::runtime::block_on(async {
        let mut link = ensemble::link::client::Connection::open(&url, token).await?;
        let listed = link
            .call(ensemble::link::protocol::NODES, serde_json::json!({}))
            .await
            .map_err(|f| f.message());
        link.close().await;
        listed
    })?;
    let nodes = listed["nodes"].as_array().cloned().unwrap_or_default();
    if nodes.is_empty() {
        println!("No node you may use is connected to {url}.");
    }
    for n in nodes {
        let names = |key: &str, field: Option<&str>| -> String {
            let all: Vec<&str> = n[key]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| match field {
                            Some(f) => v[f].as_str(),
                            None => v.as_str(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            if all.is_empty() {
                "-".into()
            } else {
                all.join(", ")
            }
        };
        println!(
            "{}  ({}, {} running)\n  agents: {}\n  models: {}",
            n["node"].as_str().unwrap_or("?"),
            n["access"].as_str().unwrap_or("?"),
            n["running"].as_u64().unwrap_or(0),
            names("agents", Some("name")),
            names("models", None)
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `ensemble hub serve`: the hub as a server for a team, until stopped.
fn cmd_hub_serve(args: &Args) -> Result<ExitCode, String> {
    let dir = hub_of(args, "hub serve")?;
    let hub = config::load_hub(
        &config::hub_path(&dir),
        args.env_file.as_deref(),
        &SystemEnv,
    )
    .map_err(|e| e.to_string())?;
    let server = hub
        .server
        .ok_or("hub serve: hub.yaml has no `server` block; add one with `listen` and `roles`")?;
    let memory = ensemble::knowledge::Memory::new(&hub, None)
        .ok_or("hub serve: a hub server needs `knowledge`")?;
    if !memory.vault().exists() {
        return Err(format!(
            "hub serve: the knowledge vault {} is not a git repository yet; `ensemble hub init` makes it",
            memory.vault().root().display()
        ));
    }
    let users = state::hub_dir(&dir)?;
    let listener = std::net::TcpListener::bind(server.listen)
        .map_err(|e| format!("hub serve: cannot listen on {}: {e}", server.listen))?;
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    let say = |line: &str| eprintln!("ensemble: {line}");
    say(&format!(
        "hub {} listening on {}",
        hub.owner,
        listener.local_addr().unwrap_or(server.listen)
    ));
    // The server and what it borrows live until the process ends: each
    // connection is a task of its own.
    let hub: &'static config::Hub = Box::leak(Box::new(hub));
    let memory: &'static ensemble::knowledge::Memory = Box::leak(Box::new(memory));
    let inbox: Option<&'static ensemble::inbox::ServerInbox> = hub.tasks.as_ref().map(|tasks| {
        &*Box::leak(Box::new(ensemble::inbox::ServerInbox::new(
            tasks,
            &hub.roles,
            users.clone(),
        )))
    });
    let mut server = ensemble::link::Server::new(memory, &hub.roles, server, users);
    if let Some(inbox) = inbox {
        server = server.with_relay(inbox);
    }
    let server: &'static ensemble::link::Server<'static> = Box::leak(Box::new(server));
    std::thread::scope(|scope| {
        scope.spawn(|| memory.work(&STOP, &say));
        if let Some(inbox) = inbox {
            scope.spawn(|| inbox.work(&STOP, &say));
        }
        ensemble::runtime::shared()
            .block_on(server.run(listener, &STOP, &|line| eprintln!("ensemble: {line}")));
    });
    say("stopped");
    Ok(ExitCode::SUCCESS)
}

/// `ensemble hub user add|list|token|revoke|remove`: a hub server's users,
/// managed on the server by whoever can read the hub, which is its admin.
fn cmd_hub_user(args: &Args) -> Result<ExitCode, String> {
    let dir = hub_of(args, "hub user")?;
    let hub = config::load_hub(
        &config::hub_path(&dir),
        args.env_file.as_deref(),
        &SystemEnv,
    )
    .map_err(|e| e.to_string())?;
    if hub.server.is_none() {
        return Err(
            "hub user: only a hub server has users; add `server` and `roles` to hub.yaml".into(),
        );
    }
    let state = state::hub_dir(&dir)?;
    let mut users = ensemble::link::Users::load(&state)?;
    let what = args.rest.get(1).map(String::as_str).unwrap_or("");
    let name = || {
        args.rest
            .get(2)
            .map(String::as_str)
            .ok_or(format!("hub user {what}: needs the user's name"))
    };
    let show_token = |name: &str, token: &str| {
        println!("{token}");
        eprintln!(
            "This is {name}'s token, shown once. On their computer: ensemble login <hub-url> --token <token>"
        );
    };
    match what {
        "add" => {
            let name = name()?;
            let role =
                opt(args, "--role").ok_or("hub user add: needs --role, one of the hub's roles")?;
            let token = users.add(name, role, &hub.roles)?;
            users.save()?;
            show_token(name, &token);
        }
        "token" => {
            let name = name()?;
            let token = users.token(name)?;
            users.save()?;
            show_token(name, &token);
        }
        "tracker" => {
            let name = name()?;
            let tracker = args.rest.get(3).map(String::as_str);
            users.set_tracker_user(name, tracker)?;
            users.save()?;
            match tracker {
                Some(t) => println!("{name} is {t} in the tracker"),
                None => println!("{name} is {name} in the tracker, the account's own name"),
            }
        }
        "password" => {
            let name = name()?;
            if users.get(name).is_none() {
                return Err(format!("hub user password: no user called {name}"));
            }
            let password = read_secret(&format!("New password for {name}: "))?;
            users.set_password(name, &password)?;
            users.save()?;
            println!("set {name}'s password; their sessions were ended");
        }
        "approve" | "role" => {
            let name = name()?;
            let role = opt(args, "--role").ok_or(format!(
                "hub user {what}: needs --role, one of the hub's roles"
            ))?;
            if what == "approve" {
                users.approve(name, role, &hub.roles)?;
                println!("approved {name} as {role}; their token works now");
            } else {
                users.set_role(name, role, &hub.roles)?;
                println!("{name} is {role} now");
            }
            users.save()?;
        }
        "revoke" => {
            let name = name()?;
            let n = users.revoke(name)?;
            users.save()?;
            println!(
                "revoked {n} token(s) of {name}; their nodes are signed out at their next request"
            );
        }
        "remove" => {
            let name = name()?;
            users.remove(name)?;
            users.save()?;
            println!("removed {name}");
        }
        "list" => {
            for user in users.list() {
                println!(
                    "{:<16} {:<8} {:<12} {} token(s)",
                    user.name,
                    user.status.as_str(),
                    if user.role.is_empty() {
                        "-"
                    } else {
                        &user.role
                    },
                    user.tokens.len()
                );
            }
        }
        other => {
            return Err(format!(
                "hub user: unknown '{other}'. Use add, list, password, tracker, approve, role, token, revoke or remove."
            ));
        }
    }
    Ok(ExitCode::SUCCESS)
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

/// Write the sample helper team, or print it.
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
    let team = team_of(args, "init")?;
    let path = config::config_path(&team);
    if path.exists() && !has("--force") {
        return Err(format!(
            "{} already exists. Pass --force to overwrite it, or --stdout to print the sample.",
            path.display()
        ));
    }
    std::fs::create_dir_all(&team).map_err(|e| format!("cannot create {}: {e}", team.display()))?;
    std::fs::write(&path, config::SAMPLE)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!(
        "wrote {}: a sample helper team with every setting",
        path.display()
    );
    println!(
        "Edit the agents, then run `ensemble check --team {}`. API keys go in the\n\
         environment, or in a file you pass with --env-file; never in this directory.",
        team.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// Rewrite an older config as a version 3 helper team in `--team`.
fn cmd_migrate(args: &Args) -> Result<ExitCode, String> {
    let source = args
        .rest
        .first()
        .ok_or("migrate: needs the old config file, e.g. `ensemble migrate ~/my-org/config.yaml --team ~/my-team`")?;
    let team = team_of(args, "migrate")?;
    let text = std::fs::read_to_string(source).map_err(|e| format!("migrate: {source}: {e}"))?;
    let v3 = config::migrate::to_v3(&text).map_err(|e| format!("migrate: {source}: {e}"))?;
    let path = config::config_path(&team);
    if path.exists() && !args.rest.iter().any(|a| a == "--force") {
        return Err(format!(
            "{} already exists. Pass --force to overwrite it.",
            path.display()
        ));
    }
    // Loading it now says what, if anything, the old file got wrong.
    config::parse(&v3, &path, &SystemEnv)
        .map_err(|e| format!("migrate: the result is not valid: {e}"))?;
    std::fs::create_dir_all(&team).map_err(|e| format!("cannot create {}: {e}", team.display()))?;
    std::fs::write(&path, v3).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!(
        "wrote {}: one helper team with every agent. Comments were not kept; an agent or gate that two teams both had is now `<team>-<name>`.",
        path.display()
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
    let knowledge = knowledge_for(args, &dir)?;
    let agents: Vec<String> = config.agent_names().into_iter().map(String::from).collect();
    let inbox = inbox_for(args, agents)?;
    let ensemble = ensemble_for(config, dir, args);
    mcp::serve_with(
        &ensemble,
        mcp::Hub {
            knowledge: knowledge.as_ref().map(Knowledge::access),
            distill: knowledge.as_ref().and_then(Knowledge::local),
            inbox: inbox.as_ref(),
            node: None,
        },
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .map_err(|e| format!("mcp: {e}"))?;
    Ok(ExitCode::SUCCESS)
}

/// The knowledge of the hub `--hub` names, for work in `dir`. None without a
/// hub, or with one that keeps no knowledge, which is said on stderr.
fn memory_for(args: &Args, dir: &Path) -> Result<Option<ensemble::knowledge::Memory>, String> {
    let Some(hub_dir) = &args.hub else {
        return Ok(None);
    };
    let hub = config::load_hub(
        &config::hub_path(hub_dir),
        args.env_file.as_deref(),
        &SystemEnv,
    )
    .map_err(|e| e.to_string())?;
    let project = dir.file_name().map(|n| n.to_string_lossy().into_owned());
    let memory = ensemble::knowledge::Memory::new(&hub, project);
    if memory.is_none() {
        eprintln!(
            "ensemble: the hub {} has no `knowledge` block, so it keeps no knowledge",
            hub_dir.display()
        );
    }
    Ok(memory)
}

/// The inbox of the hub `--hub` names, for a helper team with `agents`. None
/// without a hub, or with one that has no `tasks` block.
fn inbox_for(args: &Args, agents: Vec<String>) -> Result<Option<ensemble::inbox::Inbox>, String> {
    // A hub server's inbox is not built yet; it would run on the server.
    if hub_url(args).is_some() {
        return Ok(None);
    }
    let Some(hub_dir) = &args.hub else {
        return Ok(None);
    };
    let hub = config::load_hub(
        &config::hub_path(hub_dir),
        args.env_file.as_deref(),
        &SystemEnv,
    )
    .map_err(|e| e.to_string())?;
    // A hub server's inbox runs in `ensemble hub serve`, not here.
    let Some(tasks) = hub.tasks.as_ref().filter(|t| t.personal.is_some()) else {
        return Ok(None);
    };
    let dir = state::hub_dir(hub_dir).map_err(|e| format!("--hub {e}"))?;
    ensemble::inbox::Inbox::new(tasks, agents, dir).map(Some)
}

/// `ensemble inbox [--once]`: the issues the hub's inbox sees and the task each
/// became; with `--once`, one pass over the tracker first.
fn cmd_inbox(args: &Args) -> Result<ExitCode, String> {
    hub_of(args, "inbox")?;
    let once = has(args, "--once");
    // `<label>:<agent>` names an agent of the helper team, so a pass needs it.
    let agents = if once {
        let resolved = resolve_team(args, "inbox --once")?;
        let config = config::load(
            &config::config_path(&resolved.dir),
            args.env_file.as_deref(),
            &SystemEnv,
        )
        .map_err(|e| e.to_string())?;
        config.agent_names().into_iter().map(String::from).collect()
    } else {
        Vec::new()
    };
    let inbox =
        inbox_for(args, agents)?.ok_or(
            "inbox: the hub has no `tasks` block for one person; a hub server's inbox runs in `ensemble hub serve`",
        )?;
    if once {
        let _lock = inbox.lock()?;
        for line in inbox.pass()? {
            println!("{line}");
        }
    }
    let (tasks, user) = (inbox.settings(), &inbox.personal().user);
    let issues = inbox.tracker().assigned(user, &tasks.label);
    let seen = inbox.seen();
    println!(
        "issues assigned to {user} with the label {}: {}",
        tasks.label,
        issues.len()
    );
    for issue in &issues {
        let task = seen
            .get(&issue.id)
            .and_then(|s| s.task.as_deref())
            .map(|t| format!("  -> task {t}"))
            .unwrap_or_default();
        println!(
            "  {:<12} {:<12} {}{}{task}",
            issue.id,
            issue.status.as_str(),
            issue.title,
            issue
                .project
                .as_deref()
                .map(|p| format!("  [{p}]"))
                .unwrap_or_default()
        );
    }
    let bad = inbox.tracker().unreadable();
    if !bad.is_empty() {
        println!("issues that cannot be read:");
        for (id, why) in &bad {
            println!("  {id}: {why}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `ensemble knowledge status|distill|rebuild`.
fn cmd_knowledge(args: &Args, dir: &Path) -> Result<ExitCode, String> {
    let what = args
        .rest
        .first()
        .map(String::as_str)
        .ok_or("knowledge: needs status, distill or rebuild. Try --help.")?;
    hub_of(args, &format!("knowledge {what}"))?;
    let memory = memory_for(args, dir)?.ok_or("knowledge: the hub keeps no knowledge")?;
    let vault = memory.vault();
    let report = |run: &ensemble::knowledge::Run| {
        for line in &run.done {
            println!(
                "{} -> {}{}",
                line.share,
                line.outcome,
                if line.notes.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", line.notes.join(", "))
                }
            );
        }
        match &run.waiting {
            Some(why) => {
                println!("stopped: no model could be used: {why}");
                ExitCode::FAILURE
            }
            None => ExitCode::SUCCESS,
        }
    };
    match what {
        "status" => {
            println!("vault: {}", vault.root().display());
            println!("shares: {}", vault.shares().len());
            println!(
                "pending: {}",
                ensemble::knowledge::pipeline::pending(&vault).len()
            );
            println!("notes: {}", vault.notes().len());
            println!("in review/: {}", vault.review_count());
            Ok(ExitCode::SUCCESS)
        }
        "distill" => Ok(report(&memory.distill()?)),
        "rebuild" => {
            if !confirm(
                &format!(
                    "Delete every note and review item in {} and make them again from the {} shares?",
                    vault.root().display(),
                    vault.shares().len()
                ),
                args,
            )? {
                return Ok(ExitCode::FAILURE);
            }
            Ok(report(&memory.rebuild()?))
        }
        other => Err(format!(
            "knowledge: unknown '{other}'. Use status, distill or rebuild."
        )),
    }
}

/// `ensemble share`: keep knowledge in the hub's vault by hand, as the `share`
/// tool does for an agent.
fn cmd_share(args: &Args, dir: &Path) -> Result<ExitCode, String> {
    if args.hub.is_none() {
        hub_of(args, "share")?;
    }
    let knowledge = knowledge_for(args, dir)?.ok_or("share: the hub keeps no knowledge")?;
    let title = opt(args, "--title").ok_or("share: needs --title, what it is about")?;
    let text = match opt(args, "--text") {
        Some(text) => text.to_string(),
        None => {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
                .map_err(|e| format!("share: cannot read the text from stdin: {e}"))?;
            text
        }
    };
    if text.trim().is_empty() {
        return Err("share: the text is empty; give it with --text or on stdin".into());
    }
    let knowledge = knowledge.access();
    knowledge.set_client("ensemble-cli");
    let said = knowledge
        .call(
            ensemble::knowledge::SHARE,
            &serde_json::json!({ "title": title, "text": text }),
        )
        .expect("share is a knowledge tool")?;
    println!("{said}");
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
        let detail = format!(
            "{}{}",
            b.settings.base_url(),
            if b.settings.api_key().is_some() {
                ", authenticated"
            } else {
                ""
            }
        );
        println!(
            "  {name}  [{}]  {detail}  timeout {}s",
            b.type_id(),
            b.timeout_seconds
        );
    }
    println!();
    println!("gates:");
    for (name, g) in &config.gates {
        println!(
            "  {name}  ->  {}  (timeout {}s)",
            g.command, g.timeout_seconds
        );
    }
    if config.gates.is_empty() {
        println!("  (none)");
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
                if t.gate
                    .as_ref()
                    .is_some_and(|g| config.gate(&g.name).is_none()) =>
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
    let outcome =
        ensemble::runtime::block_on(ensemble.delegate(role, &task, args.context.as_deref()))
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
/// config its CLI is handed, on its first model. API keys are hidden. An agent
/// with several models lists them in order; `--probe` asks their backends which
/// one a run would use now.
fn cmd_show(config: &Config, args: &Args) -> Result<ExitCode, String> {
    let name = args
        .rest
        .iter()
        .find(|a| !a.starts_with("--"))
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
        Adapter::Codex(cx) => {
            let argv = ensemble::external::codex::arguments(agent, cx, TASK);
            println!("{}", command_line(&cx.command, &argv));
        }
        Adapter::Direct(la) => {
            let (path, body) = match config.llm_backends.get(la.backend()) {
                Some(backend) => {
                    let (path, body) =
                        ensemble::external::direct::request_for(backend, agent, la, TASK, None);
                    (format!("{}{path}", backend.settings.base_url()), body)
                }
                None => (
                    format!("<undefined backend {}>", la.backend()),
                    ensemble::external::direct::request(agent, la, TASK, None),
                ),
            };
            println!("POST {path}");
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
        Adapter::Builtin(ba) => {
            let backend = config.llm_backends.get(ba.backend()).map_or_else(
                || format!("<undefined backend {}>", ba.backend()),
                |b| b.settings.base_url().to_string(),
            );
            println!(
                "Ensemble's own agent loop: {} on {backend}, up to {} steps of {} tokens each",
                ba.model(),
                ba.max_steps,
                ba.max_tokens
            );
            println!("tools: {}", ba.tools.join(", "));
            if let Some(prompt) = agent.prompt.as_deref() {
                println!("system prompt:\n{prompt}");
            }
        }
        Adapter::Node(na) => {
            println!(
                "{} `run` {}",
                na.hub,
                serde_json::json!({
                    "agent": na.agent,
                    "nodes": na.nodes,
                    "task": TASK,
                    "files": "<the files git tracks in the working folder>",
                })
            );
        }
    }
    let Some(fallback) = agent.fallback().filter(|f| f.candidates.len() > 1) else {
        return Ok(ExitCode::SUCCESS);
    };
    println!();
    println!(
        "models, in order (the first is shown above; when none works: {}):",
        match fallback.when_none {
            config::WhenNone::Fail => "fail",
            config::WhenNone::Wait => "wait",
        }
    );
    for (i, c) in fallback.candidates.iter().enumerate() {
        println!("  {}. {} / {}", i + 1, c.backend, c.model);
    }
    if has(args, "--probe") {
        let candidates: Vec<(&config::LlmBackend, &str)> = agent
            .llm_backends()
            .into_iter()
            .filter_map(|(b, m)| config.llm_backends.get(b).map(|b| (b, m)))
            .collect();
        match ensemble::backends::resolve::Resolver::default().choose(&candidates) {
            Ok(i) => println!("a run now would use {}.", i + 1),
            Err(refusals) => {
                println!("none can be used now:");
                for r in refusals {
                    println!("  {} / {}: {}", r.backend, r.model, r.why);
                }
                return Ok(ExitCode::FAILURE);
            }
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
/// The `--hub` to register: a directory made absolute, or a hub server's URL
/// as it is.
fn hub_for_registration(args: &Args) -> Result<Option<PathBuf>, String> {
    match (&args.hub, hub_url(args)) {
        (_, Some(url)) => Ok(Some(PathBuf::from(url))),
        (Some(dir), None) => absolute(dir).map(Some),
        (None, None) => Ok(None),
    }
}

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
    let env_file = args.env_file.as_deref().map(absolute).transpose()?;
    let hub = hub_for_registration(args)?;
    let reg = match args.team.as_deref() {
        // No team: the agent's sessions go to the running node, which has one.
        None => {
            let node = opt(args, "--node")
                .map(|f| absolute(Path::new(f)))
                .transpose()?;
            let file = match &node {
                Some(file) => file.clone(),
                None => config::node_path(&state::config_root()?),
            };
            if !file.exists() {
                return Err(format!(
                    "connect: needs --team <dir>, a helper team to run in each session, or a node to reach; there is no node file at {}. `ensemble node init` writes one",
                    file.display()
                ));
            }
            setup::node_registration(
                manager,
                &setup::ensemble_command(),
                node.as_deref(),
                has(args, "--global"),
            )?
        }
        Some(spec) => {
            let team = match remote::parse_source(spec)? {
                remote::Source::Local(dir) => absolute(&dir)?,
                remote::Source::Remote { url, sha, git_ref } => {
                    // Pin it to the commit it is now, show what it would run,
                    // and record the approval, before anything is registered.
                    let sha = match sha {
                        Some(sha) => sha,
                        None => remote::resolve(&url, git_ref.as_deref())?,
                    };
                    approve(&url, &sha, args)?;
                    PathBuf::from(remote::pinned(&url, &sha))
                }
            };
            setup::registration(
                manager,
                &setup::ensemble_command(),
                &team,
                hub.as_deref(),
                env_file.as_deref(),
                has(args, "--global"),
            )?
        }
    };
    // The hooks are checked before anything is registered, so that a request
    // that cannot be met changes nothing.
    let hooks = if has(args, "--hooks") {
        let hub = hub.as_deref().ok_or(
            "connect: --hooks needs --hub: the hooks put the notes of its vault into your sessions",
        )?;
        let home =
            setup::home().ok_or("connect: cannot find your home directory: HOME is not set")?;
        let dir = absolute(
            &args
                .dir
                .clone()
                .unwrap_or(std::env::current_dir().map_err(|e| e.to_string())?),
        )?;
        let path = ensemble::hooks::settings_path(manager, has(args, "--global"), &home, &dir)
            .map_err(|e| format!("connect: --hooks: {e}"))?;
        let command =
            ensemble::hooks::hook_command(&setup::ensemble_command(), hub, env_file.as_deref());
        Some((path, command))
    } else {
        None
    };
    run_registration(&reg)?;
    if let Some((path, command)) = hooks {
        ensemble::hooks::install_into(&path, &command)?;
        println!("Hooks written to {}: `{command}`", path.display());
        if manager == config::CODEX_AGENT {
            println!("Codex runs them once you trust them: open /hooks in Codex.");
        }
    }
    println!("Registered. Restart {manager} so that it starts the server.");
    Ok(ExitCode::SUCCESS)
}

fn cmd_setup(args: &Args, dir: &Path) -> Result<ExitCode, String> {
    let interactive = std::io::stdin().is_terminal() && !has(args, "--yes");
    let found = setup::detect(true);
    println!(
        "Found: claude {}, opencode {}, codex {}{}",
        if found.claude { "yes" } else { "no" },
        if found.opencode { "yes" } else { "no" },
        if found.codex { "yes" } else { "no" },
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
        None => {
            let there: Vec<&str> = [
                (found.claude, "claude-code"),
                (found.opencode, "opencode"),
                (found.codex, "codex"),
            ]
            .iter()
            .filter(|(yes, _)| *yes)
            .map(|(_, name)| *name)
            .collect();
            match there.as_slice() {
                [] => {
                    return Err("setup: no agent CLI found on PATH. Install `claude` (Claude Code), `opencode` or `codex`, then run `ensemble setup` again.".into());
                }
                [only] => only.to_string(),
                several if interactive => ask(
                    &format!("Which agent do you talk to? ({})", several.join(", ")),
                    Some(several[0]),
                )?,
                several => {
                    return Err(format!(
                        "setup: {} are installed; say which one you talk to with --manager.",
                        several.join(", ")
                    ));
                }
            }
        }
    };
    let team = match &args.team {
        Some(team) => PathBuf::from(team),
        None if interactive => {
            let home = setup::home().unwrap_or_else(|| dir.to_path_buf());
            PathBuf::from(ask(
                "Where should the helper team live?",
                Some(&home.join("ensemble-team").display().to_string()),
            )?)
        }
        None => {
            return Err("setup: say where the helper team lives with --team <dir>.".into());
        }
    };
    let team = absolute(&team)?;
    let path = config::config_path(&team);
    if path.exists() {
        println!("{} exists; keeping it.", path.display());
    } else {
        let starter = setup::starter(opt(args, "--starter").unwrap_or("software"))?;
        let text = setup::starter_team(starter, &found)?;
        std::fs::create_dir_all(&team)
            .map_err(|e| format!("cannot create {}: {e}", team.display()))?;
        std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        println!(
            "Wrote {}: the {} starter. Edit it, or change it with `ensemble add-agent` and `remove-agent`.",
            path.display(),
            starter.name
        );
    }
    let env_file = args.env_file.as_deref().map(absolute).transpose()?;
    let hub = hub_for_registration(args)?;
    let reg = setup::registration(
        &manager,
        &setup::ensemble_command(),
        &team,
        hub.as_deref(),
        env_file.as_deref(),
        has(args, "--global"),
    )?;
    let registered = run_registration(&reg);
    if let Err(e) = &registered {
        println!("{e}");
    }
    println!();
    let config =
        config::load(&path, args.env_file.as_deref(), &SystemEnv).map_err(|e| e.to_string())?;
    let healthy = report_doctor(&config, &team, dir, args)?;
    println!();
    if registered.is_ok() && healthy {
        println!(
            "Done. In {manager}, try: \"have the helper team implement X\" (restart {manager} first if it was running)."
        );
        Ok(ExitCode::SUCCESS)
    } else {
        println!(
            "Setup is not finished: fix what is listed above, then run `ensemble doctor --team {}`.",
            team.display()
        );
        Ok(ExitCode::FAILURE)
    }
}

fn cmd_doctor(config: Config, dir: &Path, args: &Args) -> Result<ExitCode, String> {
    let team = absolute(&team_of(args, "doctor")?)?;
    Ok(if report_doctor(&config, &team, dir, args)? {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Print what is wrong, each with its fix, and say whether nothing was.
fn report_doctor(config: &Config, team: &Path, dir: &Path, args: &Args) -> Result<bool, String> {
    let mut findings = setup::structure(config, team, dir);
    let ensemble = ensemble_for(config.clone(), dir.to_path_buf(), args);
    for (name, result) in ensemble.health() {
        findings.push(setup::health_finding(&name, &result, config, team));
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
    let name = args
        .rest
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or("add-agent: needs the agent's NAME, such as coder")?;
    let adapter = opt(args, "--adapter")
        .ok_or("add-agent: needs --adapter claude-code, opencode, codex or direct")?;
    let team = team_of(args, "add-agent")?;
    let path = config::config_path(&team);
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
    let edited =
        config::edit::add_agent(&text, name, &block).map_err(|e| format!("add-agent: {e}"))?;
    config::parse(&edited, &path, &std::collections::BTreeMap::new()).map_err(|e| {
        format!("add-agent: the result would not load, so nothing was written: {e}")
    })?;
    std::fs::write(&path, edited).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!("added {name} to {}", path.display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_remove_agent(args: &Args) -> Result<ExitCode, String> {
    let name = args
        .rest
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or("remove-agent: needs the agent's NAME, such as coder")?;
    let team = team_of(args, "remove-agent")?;
    let path = config::config_path(&team);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("remove-agent: cannot read {}: {e}", path.display()))?;
    let edited =
        config::edit::remove_agent(&text, name).map_err(|e| format!("remove-agent: {e}"))?;
    config::parse(&edited, &path, &std::collections::BTreeMap::new()).map_err(|e| {
        format!("remove-agent: the result would not load, so nothing was written: {e}")
    })?;
    std::fs::write(&path, edited).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!("removed {name} from {}", path.display());
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

/// `ensemble team trust <url>#<sha>` and `ensemble team update <url>#<sha>`.
/// `org` is the command's old name.
fn cmd_team(args: &Args) -> Result<ExitCode, String> {
    if args.command == "org" {
        eprintln!("ensemble: `org` is now `team`; `org` stops working in the next release.");
    }
    let what = args
        .rest
        .first()
        .map(String::as_str)
        .ok_or("team: needs trust or update. Try --help.")?;
    let spec = args
        .rest
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .ok_or(format!(
            "team {what}: needs the helper team, such as github.com/acme/team#<commit>"
        ))?;
    let (url, sha, git_ref) = match remote::parse_source(spec)? {
        remote::Source::Remote { url, sha, git_ref } => (url, sha, git_ref),
        remote::Source::Local(_) => {
            return Err(format!(
                "team {what}: a directory needs no trust; this is for git repositories"
            ));
        }
    };
    match what {
        "trust" => {
            let sha = sha.ok_or(format!(
                "team trust: pin the commit: {spec}#<commit>. `ensemble connect` pins and trusts in one step."
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
                "\nThe new pin is {}.\nRead the changes above, then: ensemble team trust {}\nand register it again with ensemble connect --team {}",
                remote::pinned(&url, &new),
                remote::pinned(&url, &new),
                remote::pinned(&url, &new)
            );
            Ok(ExitCode::SUCCESS)
        }
        other => Err(format!("team: unknown '{other}'. Use trust or update.")),
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
    // An agent and a title, or a title and `--backlog`.
    let positional: Vec<&String> = args.rest.iter().filter(|a| !a.starts_with("--")).collect();
    let (agent, title): (String, &String) = match (has(args, "--backlog"), positional.as_slice()) {
        (false, [agent, title, ..]) => ((*agent).clone(), *title),
        (true, [title, ..]) => (String::new(), *title),
        _ => {
            return Err("submit: needs an agent and a title: `ensemble submit coder \"Add retry\" --text ...`, or a title and --backlog".into());
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
    if !agent.is_empty() && config.agent(&agent).is_none() {
        return Err(format!(
            "submit: no agent '{agent}'. Agents: {}.",
            config.agent_names().join(", ")
        ));
    }
    let gate = match opt(args, "--gate") {
        None => None,
        Some(spec) => {
            let (name, attempts) = spec.split_once(':').unwrap_or((spec, "1"));
            let attempts: u32 = attempts
                .parse()
                .ok()
                .filter(|n| *n >= 1)
                .ok_or("submit: --gate is name:attempts, with attempts at least 1")?;
            if config.gate(name).is_none() {
                return Err(format!("submit: no gate '{name}' under `gates`"));
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
        agent,
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
    let knowledge = knowledge_for(args, &dir)?;
    let agents: Vec<String> = config.agent_names().into_iter().map(String::from).collect();
    let inbox = inbox_for(args, agents)?;
    let ensemble = ensemble_for(config, dir, args);
    ensemble::harness::serve::run(
        &ensemble,
        mcp::Hub {
            knowledge: knowledge.as_ref().map(Knowledge::access),
            distill: knowledge.as_ref().and_then(Knowledge::local),
            inbox: inbox.as_ref(),
            node: None,
        },
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
    let (id, agent) = match args.rest.as_slice() {
        [id, agent, ..] => (id, agent),
        _ => {
            return Err(
                "assign: needs a task id and an agent: `ensemble assign 0003-fix coder`".into(),
            );
        }
    };
    let store = store_for(dir, args)?;
    if !store.exists(id) {
        return Err(format!(
            "assign: no task '{id}'. `ensemble tasks` lists them."
        ));
    }
    let to = config.agent(agent).map(|a| a.name.clone()).ok_or_else(|| {
        format!(
            "assign: no agent '{agent}'. Agents: {}.",
            config.agent_names().join(", ")
        )
    })?;
    if store.outcome(id).is_some() {
        return Err(format!("assign: {id} has ended; submit it again."));
    }
    store
        .assign(id, &to)
        .map_err(|e| format!("assign: cannot write: {e}"))?;
    println!("{id} is assigned to {to}.");
    Ok(ExitCode::SUCCESS)
}

/// `review`, `diff` and `merge`: a task's branch, for a person.
fn cmd_review(args: &Args, dir: &Path) -> Result<ExitCode, String> {
    let id = args
        .rest
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or_else(|| {
            format!(
                "{}: needs a task id. `ensemble tasks` lists them.",
                args.command
            )
        })?;
    let store = store_for(dir, args)?;
    let task =
        ensemble::review::find(&store, dir, id).map_err(|e| format!("{}: {e}", args.command))?;
    match args.command.as_str() {
        "review" => print!(
            "{}",
            ensemble::review::review(&task).map_err(|e| format!("review: {e}"))?
        ),
        "diff" => println!(
            "{}",
            ensemble::review::diff(&task).map_err(|e| format!("diff: {e}"))?
        ),
        _ => {
            let worktree = store
                .root()
                .join(ensemble::worktree::WORKTREES_DIR)
                .join(id);
            let said = ensemble::review::merge(
                &task,
                &worktree,
                ensemble::review::MergeOptions {
                    squash: has(args, "--squash"),
                    cleanup: has(args, "--cleanup"),
                },
            )
            .map_err(|e| format!("merge: {e}"))?;
            println!("{said}");
        }
    }
    Ok(ExitCode::SUCCESS)
}
