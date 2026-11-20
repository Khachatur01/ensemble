//! `ensemble-hub …`: a hub this computer runs: its settings, the server,
//! its accounts, and its knowledge vault. Whoever can read the hub's folder
//! administers it.

use crate::Ctx;
use crate::cli::{Command, KnowledgeCommand};
use crate::kept;
use ensemble_config::{self as config, SystemEnv};
use ensemble_core::state;
use ensemble_ops::cli::confirm;
use ensemble_ops::local::{self, STOP};
use std::process::ExitCode;

pub fn run(ctx: &Ctx, command: Command) -> Result<ExitCode, String> {
    match command {
        Command::Init {
            force,
            keep,
            stdout,
        } => init(force, keep, stdout),
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
/// its settings load, and its settings.
pub fn context(ctx: &Ctx) -> Result<crate::accounts::HubContext, String> {
    let dir = state::hub_config_dir()?;
    let env_file = ctx.hub_env()?;
    let at = dir.clone();
    let context = crate::accounts::HubContext::new(&dir, move || {
        kept::load(&at, env_file.as_deref()).map(|h| (h.accounts.roles, h.accounts.rules))
    });
    context.keys_from(ctx.env_file.clone());
    Ok(context)
}

/// The hub this computer runs, loaded.
fn load(ctx: &Ctx) -> Result<config::Hub, String> {
    kept::load(&state::hub_config_dir()?, ctx.hub_env()?.as_deref())
}

fn init(force: bool, keep: bool, stdout: bool) -> Result<ExitCode, String> {
    if stdout {
        print!("{}", config::hub::HUB_SAMPLE);
        return Ok(ExitCode::SUCCESS);
    }
    let dir = state::hub_config_dir()?;
    let sample = config::hub::HUB_SAMPLE;
    if !kept::settings(&dir).init(sample, force)? {
        if keep {
            return Ok(ExitCode::SUCCESS);
        }
        return Err(format!(
            "the hub in {} has its settings already. Pass --force to write the sample in their place.",
            dir.display()
        ));
    }
    println!(
        "wrote {} in {}: a sample hub, with every setting",
        kept::SETTINGS_NAME,
        kept::source(&dir).display()
    );
    let hub =
        config::parse_hub(sample, &kept::source(&dir), &SystemEnv).map_err(|e| e.to_string())?;
    ensemble_knowledge::Vault::new(&hub.connectors.knowledge.vault).init()?;
    println!(
        "made the knowledge vault, a git repository: {}",
        hub.connectors.knowledge.vault.display()
    );
    println!(
        "Name the hub as `owner`, choose the models that distill and the roles, then run `ensemble-hub check`, add the first admin with `ensemble-hub user add`, and `ensemble-hub serve`."
    );
    Ok(ExitCode::SUCCESS)
}

fn check(ctx: &Ctx) -> Result<ExitCode, String> {
    let hub = load(ctx)?;
    println!("{}: valid, for {}", kept::SETTINGS_NAME, hub.owner);
    let knowledge = &hub.connectors.knowledge;
    println!("knowledge vault: {}", knowledge.vault.display());
    if !ensemble_knowledge::Vault::new(&knowledge.vault).exists() {
        println!("  it is not a git repository yet; `ensemble-hub serve` makes it");
    }
    // Those of runners are reached only while the hub serves.
    let backends = hub.llm.backends(None);
    let candidates: Vec<(&config::LlmBackend, &str)> = knowledge
        .distill
        .fallback
        .candidates
        .iter()
        .filter_map(|c| backends.get(&c.backend).map(|b| (b, c.model.as_str())))
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
    match &knowledge.embed {
        None => println!("recall: by words"),
        Some(e) => {
            let answered = match backends.get(&e.backend) {
                Some(backend) => {
                    ensemble_backends::embed::embed(backend, &e.model, &["check".to_string()])
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                }
                None => Err(match hub.llm.runners.get(&e.backend) {
                    Some(r) => format!(
                        "it is runner {}'s, which the hub reaches only while it serves",
                        r.runner
                    ),
                    None => format!("no backend '{}'", e.backend),
                }),
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
/// change to its settings that cannot be used as it runs has it start again,
/// in this process.
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
        eprintln!("ensemble: starting again with the hub's settings as they are now");
    }
    eprintln!("ensemble: stopped");
    Ok(ExitCode::SUCCESS)
}

/// The hub, as its settings say now, until it stops or starts again.
fn serve_once(ctx: &Ctx, hub_dir: &std::path::Path) -> Result<(), String> {
    let dir = state::hub_config_dir()?;
    let env_file = ctx.hub_env()?;
    let hub = kept::load(&dir, env_file.as_deref())?;
    let listen = hub.http.listen;
    // The backends of runners the hub uses, served on loopback, each call
    // relayed to its runner.
    let calls: std::sync::Arc<crate::calls::Calls> = std::sync::Arc::default();
    let loopback = ensemble_core::runtime::shared()
        .block_on(ensemble_link::loopback::Loopback::start(
            ensemble_config::llm::routes(&hub.llm.runners),
            std::sync::Arc::new(crate::calls::HubOpener(calls.clone())),
        ))
        .map_err(|e| format!("serve: the relay's endpoint: {e}"))?;
    let relay = config::RelayAt {
        base_url: loopback.base_url.clone(),
        key: loopback.key.clone(),
    };
    let loopback: &'static ensemble_link::loopback::Loopback = Box::leak(Box::new(loopback));
    let memory = ensemble_knowledge::Memory::new(&hub, None).with_relay(&hub, relay);
    // A hub whose settings were changed to name another vault gets its vault
    // here.
    memory
        .vault()
        .init()
        .map_err(|e| format!("serve: the knowledge vault: {e}"))?;
    let users = dir.clone();
    let listener = std::net::TcpListener::bind(listen)
        .map_err(|e| format!("serve: cannot listen on {listen}: {e}"))?;
    // `ensemble-hub …` reaches it on its socket, open before it says it
    // listens, so a command right after finds it.
    let commands_at = state::hub_socket(hub_dir);
    let commands_listener = ensemble_core::runtime::shared()
        .block_on(async { local::listen(&commands_at) })
        .map_err(|e| format!("serve: {e}"))?;
    let say = |line: &str| eprintln!("ensemble: {line}");
    say(&format!(
        "hub {} listening on {}",
        hub.owner,
        listener.local_addr().unwrap_or(listen)
    ));
    // The server and what it borrows live until the process ends: each
    // connection is a task of its own.
    let hub: &'static config::Hub = Box::leak(Box::new(hub));
    let memory: &'static ensemble_knowledge::Memory = Box::leak(Box::new(memory));
    // Every task the hub makes keeps the notes related to it, as its account
    // may see them, for its agent to read as reference data.
    let notes = |user: &str, query: &str| {
        let found = memory.recall_as(user, query);
        found
            .starts_with(ensemble_knowledge::RECALL_HEADER)
            .then_some(found)
    };
    // An inbox for each tracker the settings name.
    let mut inboxes: Vec<&'static crate::inbox::ServerInbox> = Vec::new();
    for (name, tracker) in &hub.connectors.tasks.trackers {
        let inbox =
            crate::inbox::ServerInbox::new(name, tracker, &hub.accounts.roles, users.clone())
                .map_err(|e| format!("serve: {e}"))?
                .with_notes(notes);
        inboxes.push(Box::leak(Box::new(inbox)));
    }
    // What a task's agent reported when it was done is shared, so the vault
    // learns from the work as from what people share.
    let tasks = crate::board::Service::new(&users)
        .with_notes(notes)
        .on_done(|user, project, task| {
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
    let server = crate::server::Server::new(memory, &hub.accounts.roles, hub.accounts.rules, users)
        .with_tasks(tasks)
        .with_calls(calls.clone());
    // `runner.backends` lists what the connected runners share.
    server.operations().1.when_serving(move || {
        serde_json::Value::Array(
            calls
                .offered()
                .into_iter()
                .map(|o| {
                    serde_json::json!({
                        "runner": o.runner,
                        "backend": o.backend,
                        "works_for": if o.for_hub { "hub" } else { "owner" },
                        "max_calls": o.max_calls,
                        "models": o.models,
                    })
                })
                .collect(),
        )
    });
    let server: &'static crate::server::Server<'static> = Box::leak(Box::new(server));
    server.operations().1.keys_from(ctx.env_file.clone());
    // A change to its settings, or to a key they name: used at once where it
    // can be, else it starts again. Its .env is read again, made since it
    // started too.
    let given = ctx.env_file.clone();
    server.operations().1.when_changed(move || {
        let env_file = ensemble_ops::cli::env_file(given.as_deref(), &dir).or(env_file.clone());
        let now = match kept::load(&dir, env_file.as_deref()) {
            Ok(now) => now,
            Err(e) => return Some(format!("The running hub could not read it: {e}")),
        };
        if now.http != hub.http
            || now.connectors.knowledge.vault != hub.connectors.knowledge.vault
            || now.connectors.tasks != hub.connectors.tasks
        {
            local::restart();
            return Some(
                "The running hub starts again to use it; runners connect again by themselves."
                    .into(),
            );
        }
        if let Ok(accounts) = server.operations().1.accounts() {
            accounts.apply(now.accounts.roles.clone(), now.accounts.rules);
        }
        loopback.set_names(ensemble_config::llm::routes(&now.llm.runners));
        memory.apply(&now);
        Some("The running hub uses it now.".into())
    });
    std::thread::scope(|scope| {
        scope.spawn(|| memory.work(&STOP, &say));
        for inbox in &inboxes {
            scope.spawn(|| inbox.work(&STOP, &say));
        }
        ensemble_core::runtime::shared().block_on(async {
            let commands = commands(commands_listener, &commands_at, server);
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
