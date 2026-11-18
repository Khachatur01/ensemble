//! `ensemble node …`: this computer's node, which runs your helper teams and
//! reaches its hub; and what an agent CLI runs, `mcp` and `hook`.

use crate::cli::NodeCommand;
#[cfg(unix)]
use crate::on_signal;
use crate::{
    Ctx, STOP, absolute, ask, daemon, host_name, hostname, knowledge_for, node_file, node_hub,
    read_secret, run_registration, this_node,
};
use ensemble::config::{self, Config, SystemEnv};
use ensemble::knowledge::KnowledgeAccess;
use ensemble::router::Ensemble;
use ensemble::{setup, state, teams};
use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;

pub fn run(ctx: &Ctx, command: NodeCommand) -> Result<ExitCode, String> {
    match command {
        NodeCommand::Setup {
            client,
            name,
            global,
            yes,
        } => setup(ctx, client, name, global, yes),
        NodeCommand::Init { force, stdout } => init(force, stdout),
        NodeCommand::Serve => serve(ctx),
        NodeCommand::Reload => {
            let said = ensemble::runtime::block_on(daemon::reload(&state::node_socket(
                &state::node_dir()?,
            )))
            .map_err(|e| format!("node reload: {e}"))?;
            println!("{said}");
            Ok(ExitCode::SUCCESS)
        }
        NodeCommand::Status => status(ctx),
        NodeCommand::Doctor => Ok(if report_doctor(&ctx.teams()?, &ctx.dir)? {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }),
        NodeCommand::Connect {
            client,
            global,
            hooks,
        } => connect(ctx, &client, global, hooks),
        NodeCommand::Login { url, user, token } => login(&url, user.as_deref(), token.as_deref()),
        NodeCommand::Logout { url } => logout(&url),
        NodeCommand::Peers => peers(),
        NodeCommand::Config(c) => with_node_file(|file| super::config::run(file, c)),
        NodeCommand::Team(c) => super::team::run(ctx, c),
        NodeCommand::Agent(c) => super::agent::run(ctx, c),
    }
}

/// `ensemble node init`: write this computer's node file.
fn init(force: bool, stdout: bool) -> Result<ExitCode, String> {
    if stdout {
        print!("{}", config::node::NODE_SAMPLE);
        return Ok(ExitCode::SUCCESS);
    }
    let path = node_file()?;
    if path.exists() && !force {
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

/// `ensemble node status`: the node, whether it runs, its hub and its teams.
fn status(ctx: &Ctx) -> Result<ExitCode, String> {
    let node = this_node()?;
    let running =
        std::os::unix::net::UnixStream::connect(state::node_socket(&state::node_dir()?)).is_ok();
    println!(
        "node {} ({}): {}",
        node.name,
        node.access.as_str(),
        if running {
            "running"
        } else {
            "not running; start it with `ensemble node serve`"
        }
    );
    println!("  file: {}", node.source.display());
    match &node.hub {
        Some(url) => {
            let signed_in = ensemble::link::Tokens::load()?.get(url).is_some();
            println!(
                "  hub: {url}{}",
                if signed_in {
                    ", signed in"
                } else {
                    ", not signed in: `ensemble node login`"
                }
            );
        }
        None => println!("  hub: none, so no tasks and no knowledge"),
    }
    let listed = teams::list()?;
    let (ready, waiting): (Vec<_>, Vec<_>) = listed.iter().partition(|t| !t.pending);
    println!(
        "  teams: {}{}",
        if ready.is_empty() {
            "none".to_string()
        } else {
            ready
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        },
        if waiting.is_empty() {
            String::new()
        } else {
            format!(
                "; waiting for your approval: {}",
                waiting
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    );
    let _ = ctx;
    Ok(ExitCode::SUCCESS)
}

/// `ensemble node serve`: run this computer's node until stopped.
/// It serves `ensemble mcp` sessions from any folder with its teams, and runs
/// the tasks of its folders; with a hub, it also offers what it offers.
fn serve(ctx: &Ctx) -> Result<ExitCode, String> {
    let path = node_file()?;
    let node = config::load_node(&path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "node serve: no node file at {}; `ensemble node init` writes one",
                path.display()
            )
        })?;
    let (team, waiting) = teams::load_all(ctx.node_env()?.as_deref(), &SystemEnv)
        .map_err(|e| format!("node serve: {e}"))?;
    let dir = state::node_dir()?;
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
    for line in &waiting {
        say(line);
    }
    // With a hub: its tasks, and the link that offers what the node offers.
    let hub = match &node.hub {
        Some(_) => Some(node_hub("node serve")?),
        None => None,
    };
    let (url, token) = match &hub {
        Some((url, token)) => (Some(url.clone()), Some(token.clone())),
        None => (None, None),
    };
    let daemon = daemon::Daemon::new(
        node.clone(),
        hub.clone(),
        team.clone(),
        ctx.node_env()?,
        &dir,
        &say,
    );
    let worker = match (&url, &token) {
        (Some(url), Some(token)) if node.offers.runs.is_some() || node.offers.models.is_some() => {
            Some(
                ensemble::node::Worker::new(
                    url,
                    token,
                    node.clone(),
                    Some(team.clone()),
                    dir.join("runs"),
                )
                .map_err(|e| format!("node serve: {e}"))?,
            )
        }
        _ => None,
    };
    // What else holds the teams, given them again when they are reloaded.
    let reloaded = |config: &Config| -> Result<(), String> {
        if let Some(worker) = &worker {
            worker.reload(config)?;
        }
        Ok(())
    };
    let socket = state::node_socket(&dir);
    let refused = std::sync::Mutex::new(None);
    let served = {
        let stop = &STOP;
        let link = async {
            if let Some(worker) = &worker
                && let Err(e) = worker.work(stop, &say).await
            {
                *refused.lock().unwrap() = Some(e);
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        };
        let (served, ()) = ensemble::runtime::shared()
            .block_on(async { tokio::join!(daemon.serve(&socket, &reloaded), link) });
        STOP.store(true, std::sync::atomic::Ordering::Relaxed);
        served
    };
    served.map_err(|e| format!("node serve: {e}"))?;
    if let Some(e) = refused.into_inner().unwrap() {
        return Err(format!("node serve: the hub refused this node: {e}"));
    }
    say("stopped");
    Ok(ExitCode::SUCCESS)
}

/// `ensemble mcp`: a session of this computer's running node, in `dir`.
pub fn mcp(ctx: &Ctx) -> Result<ExitCode, String> {
    let path = node_file()?;
    let node = config::load_node(&path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "mcp: no node file at {}. `ensemble node setup` writes one, or `ensemble node init`; then start the node with `ensemble node serve`",
                path.display()
            )
        })?;
    let socket = state::node_socket(&state::node_dir()?);
    ensemble::runtime::shared()
        .block_on(daemon::bridge(&socket, &ctx.dir, &node.name))
        .map_err(|e| format!("mcp: {e}"))?;
    Ok(ExitCode::SUCCESS)
}

/// `ensemble hook`: what a CLI's session hook prints, the vault's notes
/// about the prompt or the project, each once per session. It never fails the
/// CLI's turn: a problem is said on stderr, and nothing is printed.
pub fn hook() -> Result<ExitCode, String> {
    if let Err(e) = notes() {
        eprintln!("ensemble: hook: {e}");
    }
    Ok(ExitCode::SUCCESS)
}

fn notes() -> Result<(), String> {
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
    // A node with no hub has no notes to give: nothing to say.
    if this_node()?.hub.is_none() {
        return Ok(());
    }
    let knowledge = knowledge_for(&dir, "hook")?;
    let answer = match ensemble::runtime::block_on(knowledge.call(
        ensemble::knowledge::RECALL,
        &serde_json::json!({ "query": topic }),
    )) {
        Some(answer) => answer?,
        None => return Ok(()),
    };
    let answer = ensemble::knowledge::recalled_within(&answer, MAX_PRINTED);
    if let Some(notes) = fresh_notes(&answer, &mut given.notes) {
        print!("{notes}");
    }
    given.save()
}

/// `ensemble node login URL`: check a hub's token, made here from your
/// account or elsewhere, and keep it.
fn login(url: &str, user: Option<&str>, token: Option<&str>) -> Result<ExitCode, String> {
    if !url.starts_with("ws://") && !url.starts_with("wss://") {
        return Err(format!(
            "node login: '{url}' is not a hub's URL, such as wss://hub.example.com"
        ));
    }
    let token = match (user, token) {
        (Some(user), None) => token_from_account(url, user)?,
        (None, Some(t)) => t.to_string(),
        _ => {
            return Err(
                "login: give either --user NAME (your password is asked) or --token TOKEN".into(),
            );
        }
    };
    let hello =
        ensemble::runtime::block_on(ensemble::link::RemoteMemory::new(url, &token, None).hello())
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
    // The node names the hub it uses.
    if !node_file()?.exists() {
        println!(
            "There is no node yet: `ensemble node setup` writes one; then name this hub in it with `ensemble node config set hub {url}`."
        );
        return Ok(ExitCode::SUCCESS);
    }
    with_node_file(|file| {
        let named = ensemble::config::settings::get(
            &std::fs::read_to_string(&file.path).unwrap_or_default(),
            "hub",
        )?;
        if named.as_deref() == Some(url) {
            return Ok(());
        }
        match file.set("hub", url) {
            Ok(()) => {
                match named {
                    Some(before) => println!("The node names this hub now, in place of {before}."),
                    None => println!("The node names this hub now."),
                }
                (file.changed)();
            }
            Err(e) => println!("The node's hub was not changed: {e}"),
        }
        Ok(())
    })?;
    Ok(ExitCode::SUCCESS)
}

/// node.yaml, changed by `ensemble node config`, `node login` and `node
/// logout`: checked as a node before it is written.
fn with_node_file<T>(
    work: impl FnOnce(&super::config::File<'_>) -> Result<T, String>,
) -> Result<T, String> {
    let path = node_file()?;
    let check = |text: &str| {
        config::parse_node(text, &path)
            .map(|_| ())
            .map_err(|e| e.to_string())
    };
    let changed = || {
        let running = state::node_dir().is_ok_and(|dir| {
            std::os::unix::net::UnixStream::connect(state::node_socket(&dir)).is_ok()
        });
        if running {
            println!(
                "The running node read node.yaml when it started: restart it to use the change (`systemctl --user restart ensemble-node` where a service runs it)."
            );
        }
    };
    work(&super::config::File {
        path: path.clone(),
        made_by: "`ensemble node setup`",
        check: &check,
        changed: &changed,
    })
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

/// `ensemble node logout URL`: forget a hub's token.
fn logout(url: &str) -> Result<ExitCode, String> {
    let mut tokens = ensemble::link::Tokens::load()?;
    if tokens.remove(url) {
        tokens.save()?;
        println!("signed out of {url}");
    } else {
        println!("no token kept for {url}");
    }
    // A node that names it would reach no hub: it names none now.
    if node_file()?.exists() {
        with_node_file(|file| {
            let text = std::fs::read_to_string(&file.path).unwrap_or_default();
            if ensemble::config::settings::get(&text, "hub")?.as_deref() == Some(url) {
                match file.unset("hub") {
                    Ok(()) => {
                        println!("The node names no hub now.");
                        (file.changed)();
                    }
                    Err(e) => println!("The node still names it: {e}"),
                }
            }
            Ok(())
        })?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `ensemble node peers`: the nodes of your node's hub you may use now.
fn peers() -> Result<ExitCode, String> {
    let (url, token) = node_hub("node peers")?;
    let token = token.as_str();
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

/// `ensemble node connect --client C`: register Ensemble with the agent CLI you
/// talk to, so that its sessions reach this computer's node; with `--hooks`,
/// the session hooks that recall from the node's hub too.
fn connect(ctx: &Ctx, client: &str, global: bool, hooks: bool) -> Result<ExitCode, String> {
    let reg = setup::registration(client, &setup::ensemble_command(), global)?;
    // The hooks are checked before anything is registered, so that a request
    // that cannot be met changes nothing.
    let hooks = if hooks {
        // The hooks recall from the hub the node names; without one they say nothing.
        if this_node().is_ok_and(|n| n.hub.is_none()) {
            println!(
                "note: this computer's node names no hub yet, so the hooks give no notes until it does"
            );
        }
        let env_file = ctx.env_file.as_deref().map(absolute).transpose()?;
        let home =
            setup::home().ok_or("connect: cannot find your home directory: HOME is not set")?;
        let path = ensemble::hooks::settings_path(client, global, &home, &ctx.dir)
            .map_err(|e| format!("connect: --hooks: {e}"))?;
        let command =
            ensemble::hooks::hook_command(&setup::ensemble_command(), env_file.as_deref());
        Some((path, command))
    } else {
        None
    };
    run_registration(&reg)?;
    if let Some((path, command)) = hooks {
        ensemble::hooks::install_into(&path, &command)?;
        println!("Hooks written to {}: `{command}`", path.display());
        if client == config::CODEX_AGENT {
            println!("Codex runs them once you trust them: open /hooks in Codex.");
        }
    }
    let node = node_file()?;
    if !node.exists() {
        println!(
            "There is no node yet: `ensemble node setup` writes {}, or `ensemble node init`.",
            node.display()
        );
    }
    println!(
        "Registered. Restart {client}: its sessions reach your node, which `ensemble node serve` runs."
    );
    Ok(ExitCode::SUCCESS)
}

/// `ensemble node setup`: find the agent CLIs, write this computer's node if there
/// is none, register Ensemble with the client, and check the node's teams.
fn setup(
    ctx: &Ctx,
    client: Option<String>,
    name: Option<String>,
    global: bool,
    yes: bool,
) -> Result<ExitCode, String> {
    let interactive = std::io::stdin().is_terminal() && !yes;
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
    let client = match client {
        Some(c) => c,
        None => {
            let there: Vec<&str> = [
                (found.claude, config::CLAUDE_CODE_AGENT),
                (found.opencode, config::OPENCODE_AGENT),
                (found.codex, config::CODEX_AGENT),
            ]
            .iter()
            .filter(|(yes, _)| *yes)
            .map(|(_, name)| *name)
            .collect();
            match there.as_slice() {
                [] => {
                    return Err("setup: no agent CLI found on PATH. Install `claude` (Claude Code), `opencode` or `codex`, then run `ensemble node setup` again.".into());
                }
                [only] => only.to_string(),
                several if interactive => ask(
                    &format!("Which agent do you talk to? ({})", several.join(", ")),
                    Some(several[0]),
                )?,
                several => {
                    return Err(format!(
                        "setup: {} are installed; say which one you talk to with --client.",
                        several.join(", ")
                    ));
                }
            }
        }
    };
    let path = node_file()?;
    if path.exists() {
        println!("{} exists; keeping it.", path.display());
    } else {
        let name = match name {
            Some(name) => name,
            None if interactive => ask(
                "What is this computer's node called? A hub knows it as <you>/<name>.",
                host_name().as_deref(),
            )?,
            None => {
                return Err("setup: name this computer's node with --name <name>.".into());
            }
        };
        let text = setup::node_file(&name)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        println!("Wrote {}: the node '{name}'.", path.display());
    }
    let reg = setup::registration(&client, &setup::ensemble_command(), global)?;
    let registered = run_registration(&reg);
    if let Err(e) = &registered {
        println!("{e}");
    }
    println!();
    config::load_node(&path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("setup: {} went away", path.display()))?;
    let (team, waiting) = teams::load_all(ctx.node_env()?.as_deref(), &SystemEnv)?;
    for line in &waiting {
        println!("{line}");
    }
    let healthy = if team.agents.is_empty() {
        println!(
            "The node has no helper team yet. Make one with `ensemble node team new <name>`, pull one with `ensemble node team pull <url>`, or ask your agent to make one once the node runs."
        );
        true
    } else {
        report_doctor(&team, &ctx.dir)?
    };
    println!();
    if registered.is_ok() && healthy {
        println!(
            "Done. Start your node with `ensemble node serve` and keep it running; then in {client}, try: \"have the helper team implement X\" (restart {client} first if it was running)."
        );
        Ok(ExitCode::SUCCESS)
    } else {
        println!(
            "Setup is not finished: fix what is listed above, then run `ensemble node setup` again."
        );
        Ok(ExitCode::FAILURE)
    }
}

/// Print what is wrong, each with its fix, and say whether nothing was.
fn report_doctor(config: &Config, dir: &Path) -> Result<bool, String> {
    let mut findings = setup::structure(config, dir);
    let ensemble = Ensemble::new(config.clone(), Some(dir.to_path_buf()));
    for (name, result) in ensemble.health() {
        findings.push(setup::health_finding(&name, &result, config));
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
