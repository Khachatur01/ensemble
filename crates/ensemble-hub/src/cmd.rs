//! `ensemble-hub …`: a hub this computer runs: its settings, the server,
//! its accounts, and its knowledge vault. Whoever can read the hub's folder
//! administers it.

use crate::Ctx;
use crate::cli::{Command, KnowledgeCommand};
use ensemble_config::{self as config, SystemEnv};
use ensemble_core::state;
use ensemble_ops::cli::confirm;
use ensemble_ops::local::{self, STOP};
use std::process::ExitCode;

pub fn run(ctx: &Ctx, command: Command) -> Result<ExitCode, String> {
    match command {
        Command::Init { force, stdout } => init(force, stdout),
        Command::Check => check(ctx),
        Command::Serve => serve(ctx),
        Command::Knowledge(c) => knowledge(ctx, c),
        Command::Completions { shell } => completions(shell),
    }
}

/// `ensemble-hub completions`: the script that completes its commands in `shell`.
fn completions(shell: clap_complete::Shell) -> Result<ExitCode, String> {
    use std::io::Write;
    let mut script = Vec::new();
    clap_complete::generate(
        shell,
        &mut crate::cli::command(&crate::accounts::operations()),
        "ensemble-hub",
        &mut script,
    );
    // Piped into `head` or the like, the rest is not wanted: no error.
    let _ = std::io::stdout().write_all(&script);
    Ok(ExitCode::SUCCESS)
}

/// What the hub's operations are given, on this computer: its accounts, when
/// hub.yaml loads, and hub.yaml.
pub fn context(ctx: &Ctx) -> Result<crate::accounts::HubContext, String> {
    let dir = state::hub_config_dir()?;
    let env_file = ctx.hub_env()?;
    let path = config::hub_path(&dir);
    Ok(crate::accounts::HubContext::new(&dir, move || {
        config::load_hub(&path, env_file.as_deref(), &SystemEnv)
            .map(|h| (h.roles, h.server))
            .map_err(|e| e.to_string())
    }))
}

/// The hub this computer runs, loaded.
fn load(ctx: &Ctx) -> Result<config::Hub, String> {
    config::load_hub(
        &config::hub_path(&state::hub_config_dir()?),
        ctx.hub_env()?.as_deref(),
        &SystemEnv,
    )
    .map_err(|e| e.to_string())
}

fn init(force: bool, stdout: bool) -> Result<ExitCode, String> {
    if stdout {
        print!("{}", config::hub::HUB_SAMPLE);
        return Ok(ExitCode::SUCCESS);
    }
    let dir = state::hub_config_dir()?;
    let path = config::hub_path(&dir);
    if path.exists() && !force {
        return Err(format!(
            "{} already exists. Pass --force to overwrite it.",
            path.display()
        ));
    }
    let (sample, what) = (config::hub::HUB_SAMPLE, "a sample hub");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    std::fs::write(&path, sample).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!("wrote {}: {what}, with every setting", path.display());
    let hub = config::parse_hub(sample, &path, &SystemEnv).map_err(|e| e.to_string())?;
    ensemble_knowledge::Vault::new(&hub.knowledge.vault).init()?;
    println!(
        "made the knowledge vault, a git repository: {}",
        hub.knowledge.vault.display()
    );
    println!(
        "Name the hub as `owner`, choose the models that distill and the roles, then run `ensemble-hub check`, add the first admin with `ensemble-hub user add`, and `ensemble-hub serve`."
    );
    Ok(ExitCode::SUCCESS)
}

fn check(ctx: &Ctx) -> Result<ExitCode, String> {
    let dir = state::hub_config_dir()?;
    let hub = config::load_hub(
        &config::hub_path(&dir),
        ctx.hub_env()?.as_deref(),
        &SystemEnv,
    )
    .map_err(|e| e.to_string())?;
    println!("{}: valid, for {}", hub.source.display(), hub.owner);
    let knowledge = &hub.knowledge;
    println!("knowledge vault: {}", knowledge.vault.display());
    if !ensemble_knowledge::Vault::new(&knowledge.vault).exists() {
        println!("  it is not a git repository yet; `ensemble-hub serve` makes it");
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
    let distills = match ensemble_backends::resolve::Resolver::default().choose(&candidates) {
        Ok(i) => {
            let c = &knowledge.distill.fallback.candidates[i];
            println!(
                "distill: shares would be distilled by {} on {}",
                c.model, c.backend
            );
            true
        }
        Err(refusals) => {
            println!("distill: no model can be used now:");
            for r in refusals {
                println!("  {} / {}: {}", r.backend, r.model, r.why);
            }
            false
        }
    };
    // Recall works without its embedding model, by words alone: said, but
    // no failure.
    match &knowledge.recall.embed {
        None => println!("recall: by words"),
        Some(e) => {
            let answered = match hub.llm_backends.get(&e.backend) {
                Some(backend) => {
                    ensemble_backends::embed::embed(backend, &e.model, &["check".to_string()])
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                }
                None => Err(format!("no backend '{}'", e.backend)),
            };
            match answered {
                Ok(()) => println!(
                    "recall: by words and by meaning, with {} on {}",
                    e.model, e.backend
                ),
                Err(why) => println!(
                    "recall: by words alone for now: {} on {} cannot embed: {why}",
                    e.model, e.backend
                ),
            }
        }
    }
    Ok(if distills {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// `ensemble-hub serve`: the hub as a server for a team, until stopped. A
/// change to hub.yaml that cannot be used as it runs has it start again, in
/// this process.
fn serve(ctx: &Ctx) -> Result<ExitCode, String> {
    // One hub per computer; the lock also tells `ensemble-hub …` it runs.
    let hub_dir = state::hub_dir()?;
    let _lock = ensemble_core::lock::acquire(&hub_dir)
        .map_err(|pid| format!("serve: process {pid} already runs the hub"))?;
    local::stop_on_signals();
    loop {
        serve_once(ctx, &hub_dir)?;
        if !local::restarting() {
            break;
        }
        eprintln!("ensemble: starting again with hub.yaml as it is now");
    }
    eprintln!("ensemble: stopped");
    Ok(ExitCode::SUCCESS)
}

/// The hub, as hub.yaml says now, until it stops or starts again.
fn serve_once(ctx: &Ctx, hub_dir: &std::path::Path) -> Result<(), String> {
    let dir = state::hub_config_dir()?;
    let env_file = ctx.hub_env()?;
    let path = config::hub_path(&dir);
    let hub =
        config::load_hub(&path, env_file.as_deref(), &SystemEnv).map_err(|e| e.to_string())?;
    let server = hub.server;
    let memory = ensemble_knowledge::Memory::new(&hub, None);
    // A hub written by hand, or by a NixOS module, gets its vault here.
    memory
        .vault()
        .init()
        .map_err(|e| format!("serve: the knowledge vault: {e}"))?;
    let users = dir.clone();
    let listener = std::net::TcpListener::bind(server.listen)
        .map_err(|e| format!("serve: cannot listen on {}: {e}", server.listen))?;
    // `ensemble-hub …` reaches it on its socket, open before it says it
    // listens, so a command right after finds it.
    let commands_at = hub.interfaces.cli.then(|| state::hub_socket(hub_dir));
    let commands_listener = match &commands_at {
        Some(socket) => Some(
            ensemble_core::runtime::shared()
                .block_on(async { local::listen(socket) })
                .map_err(|e| format!("serve: {e}"))?,
        ),
        None => None,
    };
    let say = |line: &str| eprintln!("ensemble: {line}");
    say(&format!(
        "hub {} listening on {}",
        hub.owner,
        listener.local_addr().unwrap_or(server.listen)
    ));
    // The server and what it borrows live until the process ends: each
    // connection is a task of its own.
    let hub: &'static config::Hub = Box::leak(Box::new(hub));
    let memory: &'static ensemble_knowledge::Memory = Box::leak(Box::new(memory));
    let inbox: Option<&'static crate::inbox::ServerInbox> = match &hub.tasks {
        Some(tasks) => Some(Box::leak(Box::new(
            crate::inbox::ServerInbox::new(tasks, &hub.roles, users.clone())
                .map_err(|e| format!("serve: {e}"))?,
        ))),
        None => None,
    };
    // What a task's agent reported when it was done is shared, so the vault
    // learns from the work as from what people share.
    let tasks = crate::board::Service::new(&users).on_done(|user, project, task| {
        let Some((title, text)) = crate::board::learned(project, task) else {
            return;
        };
        let id = &task.task.ticket.id;
        // Private, as the task was: its account alone recalls what came of it.
        let private = ensemble_knowledge::Visibility::Private;
        match memory.share_as(user, Some("task"), Some(project), private, &title, &text) {
            Ok(share) => eprintln!(
                "ensemble: knowledge: task {id} of {user} is share {}",
                share.id
            ),
            Err(e) => eprintln!("ensemble: knowledge: task {id} of {user} was not shared: {e}"),
        }
    });
    let tasks: &'static crate::board::Service = Box::leak(Box::new(tasks));
    let mut server =
        crate::server::Server::new(memory, &hub.roles, server, users).with_tasks(tasks);
    if let Some(mcp) = &hub.interfaces.mcp {
        server = server.with_mcp();
        say(&format!("MCP for client agents at {}", mcp.endpoint()));
    }
    let server: &'static crate::server::Server<'static> = Box::leak(Box::new(server));
    // A change to hub.yaml: used at once where it can be, else it starts again.
    server.operations().1.when_changed(move || {
        let now = match config::load_hub(&path, env_file.as_deref(), &SystemEnv) {
            Ok(now) => now,
            Err(e) => return Some(format!("The running hub could not read it: {e}")),
        };
        if now.server.listen != hub.server.listen
            || now.knowledge.vault != hub.knowledge.vault
            || now.tasks != hub.tasks
            || now.interfaces != hub.interfaces
        {
            local::restart();
            return Some(
                "The running hub starts again to use it; runners connect again by themselves."
                    .into(),
            );
        }
        if let Ok(accounts) = server.operations().1.accounts() {
            accounts.apply(now.roles.clone(), now.server);
        }
        memory.apply(&now);
        Some("The running hub uses it now.".into())
    });
    std::thread::scope(|scope| {
        scope.spawn(|| memory.work(&STOP, &say));
        if let Some(inbox) = inbox {
            scope.spawn(|| inbox.work(&STOP, &say));
        }
        ensemble_core::runtime::shared().block_on(async {
            let commands = async {
                if let (Some(listener), Some(socket)) = (commands_listener, &commands_at) {
                    commands(listener, socket, server).await;
                }
            };
            tokio::join!(
                server.run(listener, &STOP, &|line| eprintln!("ensemble: {line}")),
                commands
            );
        });
    });
    Ok(())
}

/// Run the operations `ensemble-hub …` asks for on `listener`, at `socket`,
/// until [`STOP`] is set: the one who runs the hub asks, on its computer.
async fn commands(
    listener: tokio::net::UnixListener,
    socket: &std::path::Path,
    server: &'static crate::server::Server<'static>,
) {
    while !STOP.load(std::sync::atomic::Ordering::Relaxed) {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            () = tokio::time::sleep(std::time::Duration::from_millis(200)) => continue,
        };
        let Ok((stream, _)) = accepted else { continue };
        tokio::spawn(async move {
            let (registry, context) = server.operations();
            let _ = local::reply(stream, |asked| {
                local::answer(registry, context, asked).unwrap_or_else(|| {
                    serde_json::json!({ "error": ensemble_ops::Error::invalid("`op` must name an operation") })
                })
            })
            .await;
        });
    }
    let _ = std::fs::remove_file(socket);
}

/// `ensemble-hub knowledge …`: the vault of the hub this computer runs.
fn knowledge(ctx: &Ctx, command: KnowledgeCommand) -> Result<ExitCode, String> {
    let memory = ensemble_knowledge::Memory::new(&load(ctx)?, None);
    let vault = memory.vault();
    let report = |run: &ensemble_knowledge::Run| {
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
    match command {
        KnowledgeCommand::Status => {
            println!("vault: {}", vault.root().display());
            if memory.rebuild_asked() {
                println!("a rebuild was asked: the running hub starts it in a few seconds");
            }
            println!("shares: {}", vault.shares().len());
            println!(
                "pending: {}",
                ensemble_knowledge::pipeline::pending(&vault).len()
            );
            println!("notes: {}", vault.notes().len());
            println!("in review/: {}", vault.review_count());
            Ok(ExitCode::SUCCESS)
        }
        KnowledgeCommand::Distill => Ok(report(&memory.distill()?)),
        KnowledgeCommand::Rebuild { yes } => {
            if !confirm(
                &format!(
                    "Delete every note and review item in {} and make them again from the {} shares?",
                    vault.root().display(),
                    vault.shares().len()
                ),
                yes,
            )? {
                return Ok(ExitCode::FAILURE);
            }
            // A running hub holds the vault: it rebuilds them itself.
            if let Some(pid) = memory.ask_rebuild()? {
                println!(
                    "The running hub (process {pid}) rebuilds the notes now, one share at a time; `ensemble-hub knowledge status` shows how far it is."
                );
                return Ok(ExitCode::SUCCESS);
            }
            Ok(report(&memory.rebuild()?))
        }
    }
}
