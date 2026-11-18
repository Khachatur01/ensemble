//! `ensemble-runner …`: this computer's runner, which runs the tasks of your
//! hub with your helper teams, in the git repositories of its workspace.

use crate::cli::Command;
use crate::{Ctx, daemon, host_name, runner_file, runner_hub, this_runner};
use crate::{setup, teams};
use ensemble_agents::router::Ensemble;
use ensemble_config::settings_file::{self, SettingsFile, WithSettings};
use ensemble_config::{self as config, Config, SystemEnv};
use ensemble_core::state;
use ensemble_ops::cli::ask;
use ensemble_ops::local;
use ensemble_ops::{Access, Caller, Code, Error, Field, Registry, Spec, operation};
use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;

pub fn run(ctx: &Ctx, command: Command) -> Result<ExitCode, String> {
    match command {
        Command::Setup {
            name,
            workspace,
            yes,
        } => setup(ctx, name, workspace, yes),
        Command::Init { force, stdout } => init(force, stdout),
        Command::Serve => serve(ctx),
        Command::Reload => {
            let said = ensemble_core::runtime::block_on(daemon::reload(&state::runner_socket(
                &state::runner_dir()?,
            )))
            .map_err(|e| format!("reload: {e}"))?;
            println!("{said}");
            Ok(ExitCode::SUCCESS)
        }
        Command::Status => status(),
        Command::Doctor => {
            let workspace = this_runner().ok().map(|r| r.workspace);
            Ok(
                if report_doctor(&ctx.teams()?, workspace.as_deref(), &ctx.dir)? {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                },
            )
        }
        Command::Peers => peers(),
        Command::Team(c) => super::team::run(ctx, c),
        Command::Agent(c) => super::agent::run(ctx, c),
        Command::Completions { shell } => completions(shell),
    }
}

/// `ensemble-runner completions`: the script that completes its commands in
/// `shell`.
fn completions(shell: clap_complete::Shell) -> Result<ExitCode, String> {
    use std::io::Write;
    let mut script = Vec::new();
    clap_complete::generate(
        shell,
        &mut crate::cli::command(&operations()),
        "ensemble-runner",
        &mut script,
    );
    // Piped into `head` or the like, the rest is not wanted: no error.
    let _ = std::io::stdout().write_all(&script);
    Ok(ExitCode::SUCCESS)
}

/// `ensemble-runner init`: write this computer's runner file.
fn init(force: bool, stdout: bool) -> Result<ExitCode, String> {
    if stdout {
        print!("{}", config::runner::RUNNER_SAMPLE);
        return Ok(ExitCode::SUCCESS);
    }
    let path = runner_file()?;
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
    std::fs::write(&path, config::runner::RUNNER_SAMPLE)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!(
        "wrote {}: name the runner, say whom it works for, where it works and what it offers",
        path.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// `ensemble-runner status`: the runner, whether it runs, its workspace, its
/// hub and its teams.
fn status() -> Result<ExitCode, String> {
    let runner = this_runner()?;
    println!(
        "runner {} ({}): {}",
        runner.name,
        runner.access.as_str(),
        if runner_running() {
            "running"
        } else {
            "not running; start it with `ensemble-runner serve`"
        }
    );
    println!("  file: {}", runner.source.display());
    let projects: Vec<String> = daemon::repositories(&runner.workspace)
        .iter()
        .filter_map(|r| r.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    println!(
        "  workspace: {}, {}",
        runner.workspace.display(),
        if projects.is_empty() {
            "with no git repository yet".to_string()
        } else {
            format!("projects: {}", projects.join(", "))
        }
    );
    match &runner.hub {
        Some(url) => {
            // The hub says whether the token kept for it still works.
            let said = match ensemble_link::Tokens::load()?.get(url) {
                None => format!("not signed in: `ensemble-runner login {url}`"),
                Some(token) => {
                    match ensemble_core::runtime::block_on(
                        ensemble_link::Link::new(url, token).hello(),
                    ) {
                        Ok(hello) => format!(
                            "signed in as {} ({}: {})",
                            hello.user,
                            hello.role,
                            hello.permissions.join(", ")
                        ),
                        Err(e) => format!("the hub cannot be used: {e}"),
                    }
                }
            };
            println!("  hub: {url}, {said}");
        }
        None => println!("  hub: none, so no tasks"),
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
    Ok(ExitCode::SUCCESS)
}

/// `ensemble-runner serve`: run this computer's runner until stopped. It runs
/// the tasks of the repositories in its workspace with its teams; with a hub,
/// it also offers what it offers. A change to runner.yaml has it start again,
/// in this process: running tasks start again from where their work was last
/// pushed.
fn serve(ctx: &Ctx) -> Result<ExitCode, String> {
    let dir = state::runner_dir()?;
    let _lock = ensemble_core::lock::acquire(&dir).map_err(|pid| {
        let name = this_runner().map(|r| r.name).unwrap_or_default();
        format!("serve: process {pid} already runs the runner '{name}'")
    })?;
    local::stop_on_signals();
    loop {
        serve_once(ctx, &dir)?;
        if !local::restarting() {
            break;
        }
        eprintln!("ensemble: starting again with runner.yaml as it is now");
    }
    eprintln!("ensemble: stopped");
    Ok(ExitCode::SUCCESS)
}

/// The runner, as runner.yaml says now, until it stops or starts again.
fn serve_once(ctx: &Ctx, dir: &Path) -> Result<(), String> {
    let path = runner_file()?;
    let runner = config::load_runner(&path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            format!(
                "serve: no runner file at {}; `ensemble-runner init` writes one",
                path.display()
            )
        })?;
    let (team, waiting) = teams::load_all(ctx.runner_env()?.as_deref(), &SystemEnv)
        .map_err(|e| format!("serve: {e}"))?;
    let say = |line: &str| eprintln!("ensemble: {line}");
    for line in &waiting {
        say(line);
    }
    // With a hub: its tasks, and the link that offers what the runner offers.
    // One token for all of it, replaced when `ensemble-runner login` signs in
    // again.
    let hub = match &runner.hub {
        Some(_) => {
            let (url, token) = runner_hub("serve")?;
            Some((url, ensemble_link::Token::new(&token)))
        }
        None => None,
    };
    // What `ensemble-runner …` asks of it while it runs.
    let operations = operations();
    let daemon = daemon::Daemon::new(
        runner.clone(),
        hub.clone(),
        team.clone(),
        ctx.runner_env()?,
        &operations,
        &say,
    )
    .map_err(|e| format!("serve: {e}"))?;
    let worker = match &hub {
        Some((url, token)) if runner.offers.runs.is_some() || runner.offers.models.is_some() => {
            Some(
                crate::worker::Worker::new(
                    url,
                    token.clone(),
                    runner.clone(),
                    Some(team.clone()),
                    dir.join("runs"),
                )
                .map_err(|e| format!("serve: {e}"))?,
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
    let socket = state::runner_socket(dir);
    let refused = std::sync::Mutex::new(None);
    let served = {
        let stop = &local::STOP;
        let link = async {
            if let Some(worker) = &worker
                && let Err(e) = worker.work(stop, &say).await
            {
                *refused.lock().unwrap() = Some(e);
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        };
        let (served, ()) = ensemble_core::runtime::shared()
            .block_on(async { tokio::join!(daemon.serve(&socket, &reloaded), link) });
        local::STOP.store(true, std::sync::atomic::Ordering::Relaxed);
        served
    };
    served.map_err(|e| format!("serve: {e}"))?;
    if let Some(e) = refused.into_inner().unwrap() {
        return Err(format!("serve: the hub refused this runner: {e}"));
    }
    Ok(())
}

/// What the runner's operations are given: its settings file, runner.yaml.
pub struct RunnerContext {
    settings: SettingsFile,
    /// In the running runner: read its hub's token again, and say what it did.
    relink: Option<Box<Relink>>,
}

type Relink = dyn Fn() -> Result<String, String> + Send + Sync;

/// runner.yaml, checked as a runner before it is written.
fn runner_settings() -> Result<SettingsFile, String> {
    let path = runner_file()?;
    let checked = path.clone();
    Ok(SettingsFile::new(
        path,
        "`ensemble-runner setup`",
        move |text| {
            config::parse_runner(text, &checked)
                .map(|_| ())
                .map_err(|e| e.to_string())
        },
    ))
}

impl WithSettings for RunnerContext {
    fn settings_file(&self) -> Result<&SettingsFile, Error> {
        Ok(&self.settings)
    }
}

impl RunnerContext {
    /// This computer's runner, which does not run: runner.yaml, checked as a
    /// runner before it is written.
    pub fn new() -> Result<Self, String> {
        Ok(RunnerContext {
            settings: runner_settings()?,
            relink: None,
        })
    }

    /// The runner, in the process that runs it, which reads its hub's token
    /// again with `relink`.
    pub fn running(
        relink: impl Fn() -> Result<String, String> + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let settings = runner_settings()?.on_change(|| {
            local::restart();
            Some("The running runner starts again with it; running tasks go on from their pushed work.".into())
        });
        Ok(RunnerContext {
            settings,
            relink: Some(Box::new(relink)),
        })
    }

    /// Its settings file alone, for what only a command line does with it.
    pub fn into_settings(self) -> SettingsFile {
        self.settings
    }
}

#[derive(serde::Deserialize)]
struct Login {
    url: String,
    user: Option<String>,
    token: Option<String>,
    password: Option<String>,
}

#[derive(serde::Deserialize)]
struct Join {
    url: String,
    code: String,
}

#[derive(serde::Deserialize)]
struct Logout {
    url: String,
}

const URL: Field = Field::text("url", "The hub's address, such as wss://hub.example.com.")
    .required()
    .positional();
const LOGIN: &[Field] = &[
    URL,
    Field::text(
        "user",
        "Your account on the hub: your password is asked, or read from stdin.",
    ),
    Field::text("token", "A token made elsewhere, in place of your account."),
    Field::secret("password", "Your password on the hub"),
];
const JOIN: &[Field] = &[
    URL,
    Field::text(
        "code",
        "A join code from `ensemble-hub user join-code` or your account.",
    )
    .required()
    .positional(),
];
const LOGOUT: &[Field] = &[URL];

/// Every operation of the runner.
pub fn operations() -> Registry<RunnerContext> {
    let mut all = settings_file::operations::<RunnerContext>(Access::Local);
    all.push(operation(
        Spec {
            name: "login",
            summary: "Sign in to a hub (ws:// or wss://) and keep its token for the runner.",
            access: Access::Local,
            input: LOGIN,
        },
        |c: &RunnerContext, _: &Caller, input: Login| login(c, input),
    ));
    all.push(operation(
        Spec {
            name: "join",
            summary: "Join a hub with a code made for your account, and keep its token for the runner.",
            access: Access::Local,
            input: JOIN,
        },
        |c: &RunnerContext, _: &Caller, input: Join| {
            let url = hub_url(&input.url)?;
            let params = serde_json::json!({ "code": input.code.trim(), "runner": runner_name() });
            let token = sign_in(url, ensemble_link::protocol::JOIN, params)?;
            keep(c, url, &token)
        },
    ));
    all.push(operation(
        Spec {
            name: "logout",
            summary: "Forget a hub's token.",
            access: Access::Local,
            input: LOGOUT,
        },
        |c: &RunnerContext, _: &Caller, input: Logout| logout(c, &input.url),
    ));
    Registry::new(all)
}

/// `login`: sign in to a hub over the link with your account, or take a token
/// made elsewhere, and keep it.
fn login(c: &RunnerContext, input: Login) -> Result<serde_json::Value, Error> {
    let url = hub_url(&input.url)?;
    let token = match (&input.user, &input.token) {
        (Some(user), None) => {
            let password = input.password.as_deref().ok_or_else(|| {
                Error::invalid(format!("a password is needed to sign in as {user}")).on("password")
            })?;
            let params = serde_json::json!({
                "name": user, "password": password, "runner": runner_name(),
            });
            sign_in(url, ensemble_link::protocol::LOGIN, params)?
        }
        (None, Some(t)) => t.clone(),
        _ => {
            return Err(
                Error::invalid("give either `user` (your password is asked) or `token`").on("user"),
            );
        }
    };
    keep(c, url, &token)
}

/// `url`, when it is a hub's: a WebSocket's.
fn hub_url(url: &str) -> Result<&str, Error> {
    if url.starts_with("ws://") || url.starts_with("wss://") {
        Ok(url)
    } else {
        Err(Error::invalid(format!(
            "'{url}' is not a hub's URL, such as wss://hub.example.com"
        ))
        .on("url"))
    }
}

/// Sign in to the hub at `url` over a link without a token, asking `method`
/// with `params`: the access token it makes for this computer.
fn sign_in(url: &str, method: &str, params: serde_json::Value) -> Result<String, Error> {
    ensemble_core::runtime::block_on(ensemble_link::client::sign_in(url, method, params)).map_err(
        |failure| match failure {
            ensemble_link::client::Failure::Hub(e) => Error::new(Code::Unauthorized, e),
            ensemble_link::client::Failure::Broken(e) => Error::new(Code::Unavailable, e).on("url"),
        },
    )
}

/// The name the hub gives this computer's token: the runner's, or else the
/// computer's.
fn runner_name() -> String {
    this_runner()
        .map(|r| r.name)
        .ok()
        .or_else(host_name)
        .unwrap_or_else(|| "this computer".into())
}

/// Check the token of the hub at `url`, keep it, and have the runner name
/// the hub.
fn keep(c: &RunnerContext, url: &str, token: &str) -> Result<serde_json::Value, Error> {
    let hello = ensemble_core::runtime::block_on(ensemble_link::Link::new(url, token).hello())
        .map_err(|e| Error::new(Code::Unauthorized, e).on("token"))?;
    let mut tokens = ensemble_link::Tokens::load()?;
    tokens.set(url, token);
    tokens.save()?;
    let mut said = vec![format!(
        "signed in to {url} as {} ({}: {})",
        hello.user,
        hello.role,
        hello.permissions.join(", ")
    )];
    // The runner names the hub it uses.
    let file = c.settings_file()?;
    if !file.path.exists() {
        said.push(format!(
            "There is no runner yet: `ensemble-runner setup` writes one; then name this hub in it with `ensemble-runner config set hub {url}`."
        ));
    } else {
        let named = ensemble_config::settings::get(&file.read()?, "hub").map_err(Error::invalid)?;
        if named.as_deref() == Some(url) {
            // The running runner takes the new token as it is.
            if let Some(relink) = &c.relink {
                said.push(match relink() {
                    Ok(relinked) => format!("The running runner: {relinked}."),
                    Err(e) => e,
                });
            }
        } else {
            match file.set("hub", url) {
                Ok(note) => {
                    said.push(match named {
                        Some(before) => {
                            format!("The runner names this hub now, in place of {before}.")
                        }
                        None => "The runner names this hub now.".into(),
                    });
                    said.extend(note);
                }
                Err(e) => said.push(format!("The runner's hub was not changed: {e}")),
            }
        }
    }
    Ok(serde_json::json!({
        "said": said.join("\n"),
        "user": hello.user,
        "role": hello.role,
        "permissions": hello.permissions,
    }))
}

/// `logout`: forget a hub's token, and have the runner name no hub when it
/// named that one.
fn logout(c: &RunnerContext, url: &str) -> Result<serde_json::Value, Error> {
    let mut tokens = ensemble_link::Tokens::load()?;
    let mut said = Vec::new();
    if tokens.remove(url) {
        tokens.save()?;
        said.push(format!("signed out of {url}"));
    } else {
        said.push(format!("no token kept for {url}"));
    }
    // A runner that names it would reach no hub: it names none now.
    let file = c.settings_file()?;
    if file.path.exists()
        && ensemble_config::settings::get(&file.read()?, "hub")
            .map_err(Error::invalid)?
            .as_deref()
            == Some(url)
    {
        match file.unset("hub") {
            Ok(note) => {
                said.push("The runner names no hub now.".into());
                said.extend(note);
            }
            Err(e) => said.push(format!("The runner still names it: {e}")),
        }
    }
    Ok(serde_json::json!({ "said": said.join("\n") }))
}

/// Whether this computer's runner runs: it answers on its socket.
fn runner_running() -> bool {
    state::runner_dir().is_ok_and(|dir| {
        std::os::unix::net::UnixStream::connect(state::runner_socket(&dir)).is_ok()
    })
}

/// `ensemble-runner peers`: the runners of your runner's hub you may use now.
fn peers() -> Result<ExitCode, String> {
    let (url, token) = runner_hub("peers")?;
    let token = token.as_str();
    let listed = ensemble_core::runtime::block_on(async {
        let mut link = ensemble_link::client::Connection::open(&url, token).await?;
        let listed = link
            .call(ensemble_link::protocol::NODES, serde_json::json!({}))
            .await
            .map_err(|f| f.message());
        link.close().await;
        listed
    })?;
    let nodes = listed["nodes"].as_array().cloned().unwrap_or_default();
    if nodes.is_empty() {
        println!("No runner you may use is connected to {url}.");
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

/// `ensemble-runner setup`: find the agent CLIs, write this computer's runner
/// if there is none, and check the runner's teams.
fn setup(
    ctx: &Ctx,
    name: Option<String>,
    workspace: Option<String>,
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
    if !(found.claude || found.opencode || found.codex) {
        println!(
            "No agent CLI is on PATH: agents of type claude-code, opencode or codex need theirs installed (`claude`, `opencode`, `codex`); builtin and direct agents need none."
        );
    }
    let path = runner_file()?;
    if path.exists() {
        println!("{} exists; keeping it.", path.display());
    } else {
        let name = match name {
            Some(name) => name,
            None if interactive => ask(
                "What is this computer's runner called? A hub knows it as <you>/<name>.",
                host_name().as_deref(),
            )?,
            None => {
                return Err("setup: name this computer's runner with --name <name>.".into());
            }
        };
        let workspace = match workspace {
            Some(workspace) => workspace,
            None if interactive => ask(
                "Which folder does it work in? Each git repository directly inside it is a project.",
                Some(setup::WORKSPACE),
            )?,
            None => setup::WORKSPACE.to_string(),
        };
        let text = setup::runner_file(&name, &workspace)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        println!(
            "Wrote {}: the runner '{name}', working in {workspace}.",
            path.display()
        );
    }
    println!();
    let runner = config::load_runner(&path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("setup: {} went away", path.display()))?;
    let (team, waiting) = teams::load_all(ctx.runner_env()?.as_deref(), &SystemEnv)?;
    for line in &waiting {
        println!("{line}");
    }
    let healthy = if team.agents.is_empty() {
        println!(
            "The runner has no helper team yet. Make one with `ensemble-runner team new <name>`, or pull one with `ensemble-runner team pull <url>`."
        );
        true
    } else {
        report_doctor(&team, Some(&runner.workspace), &ctx.dir)?
    };
    println!();
    if healthy {
        println!(
            "Done. Clone the repositories its tasks are for into {}, sign in to your hub with `ensemble-runner login <url> --user <name>` or `ensemble-runner join <url> <code>`, and start the runner with `ensemble-runner serve`; keep it running.",
            runner.workspace.display()
        );
        Ok(ExitCode::SUCCESS)
    } else {
        println!(
            "Setup is not finished: fix what is listed above, then run `ensemble-runner setup` again."
        );
        Ok(ExitCode::FAILURE)
    }
}

/// Print what is wrong, each with its fix, and say whether nothing was. The
/// agent CLIs are started in `dir`.
fn report_doctor(config: &Config, workspace: Option<&Path>, dir: &Path) -> Result<bool, String> {
    let mut findings = setup::structure(config, workspace);
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
