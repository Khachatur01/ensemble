//! `ensemble hub …`: a hub this computer runs: its settings, the server,
//! its accounts, and its knowledge vault. Whoever can read the hub's folder
//! administers it.

use crate::cli::{HubCommand, HubKnowledgeCommand, UserCommand};
#[cfg(unix)]
use crate::on_signal;
use crate::{Ctx, STOP, confirm, read_secret};
use ensemble::config::{self, SystemEnv};
use ensemble::state;
use std::process::ExitCode;

pub fn run(ctx: &Ctx, command: HubCommand) -> Result<ExitCode, String> {
    match command {
        HubCommand::Init { force, stdout } => init(force, stdout),
        HubCommand::Check => check(ctx),
        HubCommand::Serve => serve(ctx),
        HubCommand::User(c) => user(ctx, c),
        HubCommand::Knowledge(c) => knowledge(ctx, c),
        HubCommand::Config(c) => {
            let path = config::hub_path(&state::hub_config_dir()?);
            let check = |text: &str| {
                config::parse_hub(text, &path, &SystemEnv)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            };
            let changed = || {
                println!(
                    "A running hub read hub.yaml when it started: restart it to use the change (`sudo systemctl restart ensemble-hub` with the NixOS module)."
                )
            };
            super::config::run(
                &super::config::File {
                    path: path.clone(),
                    made_by: "`ensemble hub init`",
                    check: &check,
                    changed: &changed,
                },
                c,
            )
        }
    }
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
    ensemble::knowledge::Vault::new(&hub.knowledge.vault).init()?;
    println!(
        "made the knowledge vault, a git repository: {}",
        hub.knowledge.vault.display()
    );
    println!(
        "Name the hub as `owner`, choose the models that distill and the roles, then run `ensemble hub check`, add the first admin with `ensemble hub user add`, and `ensemble hub serve`."
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
    if !ensemble::knowledge::Vault::new(&knowledge.vault).exists() {
        println!("  it is not a git repository yet; `ensemble hub serve` makes it");
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

/// `ensemble hub serve`: the hub as a server for a team, until stopped.
fn serve(ctx: &Ctx) -> Result<ExitCode, String> {
    let dir = state::hub_config_dir()?;
    let hub = config::load_hub(
        &config::hub_path(&dir),
        ctx.hub_env()?.as_deref(),
        &SystemEnv,
    )
    .map_err(|e| e.to_string())?;
    let server = hub.server;
    let memory = ensemble::knowledge::Memory::new(&hub, None);
    // A hub written by hand, or by a NixOS module, gets its vault here.
    memory
        .vault()
        .init()
        .map_err(|e| format!("hub serve: the knowledge vault: {e}"))?;
    let users = dir.clone();
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
    let inbox: Option<&'static ensemble::inbox::ServerInbox> = match &hub.tasks {
        Some(tasks) => Some(Box::leak(Box::new(
            ensemble::inbox::ServerInbox::new(tasks, &hub.roles, users.clone())
                .map_err(|e| format!("hub serve: {e}"))?,
        ))),
        None => None,
    };
    let tasks: &'static ensemble::board::Service =
        Box::leak(Box::new(ensemble::board::Service::new(&users)));
    let server = ensemble::link::Server::new(memory, &hub.roles, server, users).with_tasks(tasks);
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

/// `ensemble hub user …`: the hub's accounts, kept in its database.
fn user(ctx: &Ctx, command: UserCommand) -> Result<ExitCode, String> {
    let hub = load(ctx)?;
    let mut users = ensemble::link::Users::load(&state::hub_config_dir()?)?;
    let show_token = |name: &str, token: &str| {
        println!("{token}");
        eprintln!(
            "This is {name}'s token, shown once. On their computer: ensemble node login <hub-url> --token <token>"
        );
    };
    match command {
        UserCommand::Add { name, role } => {
            let (name, role) = (name.as_str(), role.as_str());
            let token = users.add(name, role, &hub.roles)?;
            users.save()?;
            show_token(name, &token);
        }
        UserCommand::Token { name } => {
            let name = name.as_str();
            let token = users.token(name)?;
            users.save()?;
            show_token(name, &token);
        }
        UserCommand::Tracker { name, tracker_name } => {
            let (name, tracker) = (name.as_str(), tracker_name.as_deref());
            users.set_tracker_user(name, tracker)?;
            users.save()?;
            match tracker {
                Some(t) => println!("{name} is {t} in the tracker"),
                None => println!("{name} is {name} in the tracker, the account's own name"),
            }
        }
        UserCommand::Password { name } => {
            let name = name.as_str();
            if users.get(name).is_none() {
                return Err(format!("hub user password: no user called {name}"));
            }
            let password = read_secret(&format!("New password for {name}: "))?;
            users.set_password(name, &password)?;
            users.save()?;
            println!("set {name}'s password; their sessions were ended");
        }
        UserCommand::Approve { name, role } => {
            users.approve(&name, &role, &hub.roles)?;
            users.save()?;
            println!("approved {name} as {role}; their token works now");
        }
        UserCommand::Role { name, role } => {
            users.set_role(&name, &role, &hub.roles)?;
            users.save()?;
            println!("{name} is {role} now");
        }
        UserCommand::Revoke { name } => {
            let name = name.as_str();
            let n = users.revoke(name)?;
            users.save()?;
            println!(
                "revoked {n} token(s) of {name}; their nodes are signed out at their next request"
            );
        }
        UserCommand::Remove { name } => {
            let name = name.as_str();
            users.remove(name)?;
            users.save()?;
            println!("removed {name}");
        }
        UserCommand::List => {
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
    }
    Ok(ExitCode::SUCCESS)
}

/// `ensemble hub knowledge …`: the vault of the hub this computer runs.
fn knowledge(ctx: &Ctx, command: HubKnowledgeCommand) -> Result<ExitCode, String> {
    let memory = ensemble::knowledge::Memory::new(&load(ctx)?, None);
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
    match command {
        HubKnowledgeCommand::Status => {
            println!("vault: {}", vault.root().display());
            if memory.rebuild_asked() {
                println!("a rebuild was asked: the running hub starts it in a few seconds");
            }
            println!("shares: {}", vault.shares().len());
            println!(
                "pending: {}",
                ensemble::knowledge::pipeline::pending(&vault).len()
            );
            println!("notes: {}", vault.notes().len());
            println!("in review/: {}", vault.review_count());
            Ok(ExitCode::SUCCESS)
        }
        HubKnowledgeCommand::Distill => Ok(report(&memory.distill()?)),
        HubKnowledgeCommand::Rebuild { yes } => {
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
                    "The running hub (process {pid}) rebuilds the notes now, one share at a time; `ensemble hub knowledge status` shows how far it is."
                );
                return Ok(ExitCode::SUCCESS);
            }
            Ok(report(&memory.rebuild()?))
        }
    }
}
