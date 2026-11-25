//! `ensemble-hub …`: a hub this computer runs: its settings, the server,
//! its accounts, and its knowledge vault. Whoever can read the hub's folder
//! administers it. The hub knows nothing of a runner on the same computer:
//! a runner reaches it as any other does, over the link.

use crate::Ctx;
use crate::cli::{Command, KnowledgeCommand};
use crate::kept;
use ensemble_config::kept::{Change, Keep};
use ensemble_config::{self as config, HubSettings, Permission};
use ensemble_core::state::{self, HubDir};
use ensemble_ops::cli::{ask, confirm};
use ensemble_ops::local::{self, STOP};
use serde_json::json;
use std::io::IsTerminal;
use std::net::SocketAddr;

use std::process::ExitCode;

pub fn run(ctx: &Ctx, command: Command) -> Result<ExitCode, String> {
    match command {
        Command::Init { force, keep, owner } => init(ctx, force, keep, owner),
        Command::Setup { owner, admin, yes } => setup(ctx, owner, admin, yes),
        Command::Check => check(ctx),
        Command::Serve {
            http_host,
            http_port,
        } => serve(ctx, SocketAddr::new(http_host, http_port)),
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
    let at = ctx.dir.clone();
    Ok(crate::accounts::HubContext::new(&ctx.dir, move || {
        kept::load(&at).map(|h| (h.accounts.roles, h.accounts.rules))
    }))
}

/// The hub this computer runs, loaded.
fn load(ctx: &Ctx) -> Result<config::Hub, String> {
    kept::load(&ctx.dir)
}

/// Who runs this computer's hub, as its owner is named when nobody says.
fn this_user() -> Option<String> {
    std::env::var("USER")
        .ok()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
}

/// `ensemble-hub init`: the settings a new hub starts with, and its vault:
/// `owner`'s, on this computer alone, closed to sign-ups, with no backend or
/// model yet. Not when it has settings, unless `force`; with `keep`, that is
/// no error: what the NixOS module runs before the hub starts.
fn init(ctx: &Ctx, force: bool, keep: bool, owner: Option<String>) -> Result<ExitCode, String> {
    let dir = &ctx.dir;
    if !force && kept::settings(dir).read()?.is_some() {
        if keep {
            return Ok(ExitCode::SUCCESS);
        }
        return Err(format!(
            "the hub in {} has its settings already; `ensemble-hub settings show` shows them. --force makes them anew.",
            dir.path().display()
        ));
    }
    let owner = owner
        .or_else(this_user)
        .ok_or("init: name the hub with --owner <name>")?;
    start(dir, &owner)?;
    println!(
        "Next: `ensemble-hub setup` adds the first admin, or `ensemble-hub user add <name> --role admin`."
    );
    Ok(ExitCode::SUCCESS)
}

/// Keep the settings a new hub of `owner`'s starts with in `dir`, and make its
/// vault.
fn start(dir: &HubDir, owner: &str) -> Result<(), String> {
    let settings = HubSettings::starting(owner.trim());
    let hub = settings
        .resolve(dir, &config::SystemEnv)
        .map_err(|e| e.to_string())?;
    kept::settings(dir).write(&settings, &Change::by_this_computer("init"))?;
    println!(
        "Made the hub {}: closed to sign-ups, with no provider of models yet. Its settings are kept in {}.",
        hub.owner,
        dir.database().display()
    );
    ensemble_knowledge::Vault::new(&hub.connectors.knowledge.vault).init()?;
    println!(
        "Made the knowledge vault, a git repository: {}",
        hub.connectors.knowledge.vault.display()
    );
    Ok(())
}

/// `ensemble-hub setup`: the hub's settings when it has none, and its first
/// admin, with their token; then what is next.
fn setup(
    ctx: &Ctx,
    owner: Option<String>,
    admin: Option<String>,
    yes: bool,
) -> Result<ExitCode, String> {
    let interactive = std::io::stdin().is_terminal() && !yes;
    let me = this_user();
    let dir = &ctx.dir;
    let settings = match kept::settings(dir).read()? {
        Some(settings) => {
            println!(
                "The hub {} has its settings already; keeping them.",
                settings.owner
            );
            settings
        }
        None => {
            let owner = match owner {
                Some(owner) => owner,
                None if interactive => ask("What is the hub called?", me.as_deref())?,
                None => me
                    .clone()
                    .ok_or("setup: name the hub with --owner <name>")?,
            };
            start(dir, &owner)?;
            kept::settings(dir)
                .read()?
                .ok_or("setup: the settings were not kept")?
        }
    };
    let hub = crate::accounts::operations();
    let route = crate::route(ctx)?;
    let call = |name: &str, input| route.call(&hub, name, input).map_err(|e| e.to_string());
    let admins: Vec<&String> = settings
        .roles
        .iter()
        .filter(|(_, can)| can.contains(&Permission::Admin))
        .map(|(role, _)| role)
        .collect();
    let users = call("user.list", json!({}))?;
    let admin_now = users["users"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|u| u["status"] == "active" && admins.iter().any(|r| u["role"] == r.as_str()));
    match admin_now {
        Some(u) => {
            println!(
                "{} is an admin already.",
                u["name"].as_str().unwrap_or("someone")
            );
        }
        None => {
            let role = admins
                .first()
                .ok_or("setup: no role of the hub may administer it; `ensemble-hub role set admin admin` makes one")?;
            let name = match admin {
                Some(name) => name,
                None if interactive => ask("Who is its first admin?", me.as_deref())?,
                None => me
                    .clone()
                    .ok_or("setup: name the first admin with --admin <name>")?,
            };
            let added = call("user.add", json!({ "name": name, "role": role }))?;
            println!(
                "\nAdded {name} as {role}. Their token, shown once:\n\n  {}\n",
                added["token"].as_str().unwrap_or("")
            );
            if let Some(note) = added["note"].as_str() {
                println!("{note}\n");
            }
        }
    }
    println!(
        "Next: give it a model server with `ensemble-hub provider add <name> --type openai-compatible-api --url <url> --max-concurrent <n>`, add the models you want with `ensemble-hub model add <name>:<model>`, choose those that distill shares with `ensemble-hub knowledge models <name>:<model>`, and watch a tracker with `ensemble-hub tracker add`. `ensemble-hub settings show` shows every setting."
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
    // The models of runners are reached only while the hub serves.
    let backends = hub.llm.backends(None);
    let candidates: Vec<(&config::LlmBackend, &str)> = knowledge
        .distill
        .fallback
        .candidates
        .iter()
        .filter_map(|c| backends.get(&c.provider).map(|b| (b, c.model.as_str())))
        .collect();
    let chosen = !knowledge.distill.fallback.candidates.is_empty();
    let distills = match ensemble_backends::resolve::Resolver::default().choose(&candidates) {
        _ if !chosen => {
            println!(
                "distill: no model is chosen yet, so shares wait; `ensemble-hub knowledge models <provider>:<model>` chooses them"
            );
            true
        }
        Ok(i) => {
            let c = &knowledge.distill.fallback.candidates[i];
            println!(
                "distill: shares would be distilled by {}:{}",
                c.provider, c.model
            );
            true
        }
        Err(refusals) => {
            println!("distill: no model can be used now:");
            for r in refusals {
                println!("  {}:{}: {}", r.backend, r.model, r.why);
            }
            false
        }
    };
    // Recall works without its embedding model, by words alone: said, but
    // no failure.
    match &knowledge.embed {
        None => println!("recall: by words"),
        Some(e) => {
            let answered = match backends.get(&e.provider) {
                Some(backend) => {
                    ensemble_backends::embed::embed(backend, &e.model, &["check".to_string()])
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                }
                None => Err(match hub.llm.relayed.get(&e.provider) {
                    Some(runner) => format!(
                        "it is runner {runner}'s, which the hub reaches only while it serves"
                    ),
                    None => format!("no provider '{}'", e.provider),
                }),
            };
            match answered {
                Ok(()) => println!(
                    "recall: by words and by meaning, with {}:{}",
                    e.provider, e.model
                ),
                Err(why) => println!(
                    "recall: by words alone for now: {}:{} cannot embed: {why}",
                    e.provider, e.model
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

/// `ensemble-hub serve`: the hub as a server for a team, until stopped; a
/// new one, as `init` makes it, when its folder has none, so that what
/// installs it only runs it. A change to its settings that cannot be used as
/// it runs has it start again, in this process.
fn serve(ctx: &Ctx, listen: SocketAddr) -> Result<ExitCode, String> {
    // One hub per folder; the lock also tells `ensemble-hub …` it runs.
    let _lock = ensemble_core::lock::acquire(ctx.dir.path())
        .map_err(|pid| format!("serve: process {pid} already runs the hub"))?;
    if kept::settings(&ctx.dir).read()?.is_none() {
        let owner = this_user()
            .ok_or("serve: name the hub first with `ensemble-hub init --owner <name>`")?;
        start(&ctx.dir, &owner)?;
        println!("Next: `ensemble-hub setup` adds its first admin.");
    }
    local::stop_on_signals();
    loop {
        serve_once(ctx, listen)?;
        if !local::restarting() {
            break;
        }
        eprintln!("ensemble: starting again with the hub's settings as they are now");
    }
    eprintln!("ensemble: stopped");
    Ok(ExitCode::SUCCESS)
}

/// The hub, as its settings say now, listening on `listen`, until it stops
/// or starts again.
fn serve_once(ctx: &Ctx, listen: SocketAddr) -> Result<(), String> {
    let dir = ctx.dir.clone();
    let hub = kept::load(&dir)?;
    // The backends of runners the hub uses, served on loopback, each call
    // relayed to its runner.
    let calls: std::sync::Arc<crate::calls::Calls> = std::sync::Arc::default();
    let loopback = ensemble_core::runtime::shared()
        .block_on(ensemble_link::loopback::Loopback::start(
            hub.llm.routes(),
            std::sync::Arc::new(crate::calls::HubOpener(calls.clone())),
        ))
        .map_err(|e| format!("serve: the relay's endpoint: {e}"))?;
    let relay = config::RelayAt {
        base_url: loopback.base_url.clone(),
        key: loopback.key.clone(),
    };
    let loopback: &'static ensemble_link::loopback::Loopback = Box::leak(Box::new(loopback));
    let memory = ensemble_knowledge::Memory::new(&hub, None).with_relay(&hub, relay);
    // A hub made without `init` gets its vault here.
    memory
        .vault()
        .init()
        .map_err(|e| format!("serve: the knowledge vault: {e}"))?;
    let listener = std::net::TcpListener::bind(listen)
        .map_err(|e| format!("serve: cannot listen on {listen}: {e}"))?;
    // `ensemble-hub …` reaches it on its socket, open before it says it
    // listens, so a command right after finds it.
    let commands_at = state::hub_socket(dir.path());
    let commands_listener = ensemble_core::runtime::shared()
        .block_on(async { local::listen(&commands_at) })
        .map_err(|e| format!("serve: {e}"))?;
    let say = |line: &str| eprintln!("ensemble: {line}");
    let listen = listener.local_addr().unwrap_or(listen);
    say(&format!("hub {} listening on {listen}", hub.owner));
    // The server and what it borrows live until the process ends: each
    // connection is a task of its own.
    let hub: &'static config::Hub = Box::leak(Box::new(hub));
    let memory: &'static ensemble_knowledge::Memory = Box::leak(Box::new(memory));
    // Every task the hub makes keeps the notes related to it, as its account
    // may see them, for its run to read as reference data.
    let notes = |user: &str, query: &str| {
        let found = memory.recall_as(user, query);
        found
            .starts_with(ensemble_knowledge::RECALL_HEADER)
            .then_some(found)
    };
    // An inbox for each tracker the settings name.
    let mut inboxes: Vec<&'static crate::inbox::ServerInbox> = Vec::new();
    for (name, tracker) in &hub.connectors.tasks.trackers {
        let inbox = crate::inbox::ServerInbox::new(name, tracker, &hub.accounts.roles, &dir)
            .map_err(|e| format!("serve: {e}"))?
            .with_notes(notes);
        inboxes.push(Box::leak(Box::new(inbox)));
    }
    // What a task's run reported when it was done is shared, so the vault
    // learns from the work as from what people share.
    let tasks = crate::board::Service::new(&dir.tasks())
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
    let server = crate::server::Server::new(memory, &hub.accounts.roles, hub.accounts.rules, &dir)
        .with_tasks(tasks)
        .with_calls(calls.clone());
    server.operations().1.listening(listen);
    // `runner.models` lists what the connected runners share.
    server.operations().1.when_serving(move || {
        serde_json::Value::Array(
            calls
                .offered()
                .into_iter()
                .map(|o| {
                    serde_json::json!({
                        "runner": o.runner,
                        "provider": o.provider,
                        "max_concurrent": o.max_calls,
                        "models": o.models,
                    })
                })
                .collect(),
        )
    });
    let server: &'static crate::server::Server<'static> = Box::leak(Box::new(server));
    server.answer_operations();
    // The task picker chooses the worker of each task of the runners
    // connected now.
    let dispatcher = crate::dispatch::Dispatcher::new(&dir.tasks(), move || server.routes.now());
    dispatcher.apply(hub.task_picker.as_ref(), &hub.llm.direct);
    let dispatcher: &'static crate::dispatch::Dispatcher = Box::leak(Box::new(dispatcher));
    // A change to its settings, or to a key they name: used at once where it
    // can be, else it starts again.
    server.operations().1.when_changed(move || {
        let now = match kept::load(&dir) {
            Ok(now) => now,
            Err(e) => return Some(format!("The running hub could not read it: {e}")),
        };
        if now.connectors.tasks != hub.connectors.tasks {
            local::restart();
            return Some(
                "The running hub starts again to use it; runners connect again by themselves."
                    .into(),
            );
        }
        if let Ok(accounts) = server.operations().1.accounts() {
            accounts.apply(now.accounts.roles.clone(), now.accounts.rules);
        }
        loopback.set_names(now.llm.routes());
        memory.apply(&now);
        dispatcher.apply(now.task_picker.as_ref(), &now.llm.direct);
        Some("The running hub uses it now.".into())
    });
    std::thread::scope(|scope| {
        // What its providers serve now: a model new to one is kept off.
        scope.spawn(|| {
            let said = ensemble_backends::ops::refresh_all(server.operations().1);
            say(&format!("the providers: {}", said.replace('\n', "; ")));
        });
        scope.spawn(|| memory.work(&STOP, &say));
        scope.spawn(|| dispatcher.work(&STOP, &say));
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
