//! `ensemble-runner …`: this computer's runner, which runs the tasks of your
//! hub with your helper teams, in the git repositories of its workspace.

use crate::cli::Command;
use crate::{Ctx, daemon, host_name, runner_hub, this_runner};
use crate::{kept, setup, teams};
use ensemble_agents::router::Ensemble;
use ensemble_backends::ops::{Backends, KeptBackends, Side, WithBackends};
use ensemble_config::kept::{Change, Keep, Kept, WithKept};
use ensemble_config::{self as config, Config, RunnerSettings, WorksFor};
use ensemble_core::state::{self, RunnerDir};
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
        Command::Init { force, keep } => init(force, keep),
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

/// `ensemble-runner init`: make this computer's runner as `setup` does
/// without asking: named for this computer, working for its owner in
/// ~/ensemble, offering nothing. Not when it has one, unless `force`; with
/// `keep`, that is no error: what the NixOS module runs before each start.
fn init(force: bool, keep: bool) -> Result<ExitCode, String> {
    let dir = kept::dir()?;
    let store = kept::settings(&dir);
    if !force && store.read()?.is_some() {
        if keep {
            return Ok(ExitCode::SUCCESS);
        }
        return Err(format!(
            "this computer has a runner already, kept in {}; `ensemble-runner settings show` shows it. --force makes it anew.",
            dir.database().display()
        ));
    }
    let name = host_name().unwrap_or_else(|| "runner".into());
    let settings = setup::starting(&name, setup::WORKSPACE)?;
    store.write(&settings, &Change::by_this_computer("init"))?;
    println!(
        "Made the runner '{name}', working for you in {}, kept in {}. `ensemble-runner login` signs it in to a hub, `ensemble-runner backend add` gives it LLM backends, and `ensemble-runner settings show` shows every setting.",
        setup::WORKSPACE,
        dir.database().display()
    );
    Ok(ExitCode::SUCCESS)
}

/// `ensemble-runner status`: the runner, whether it runs, its workspace, its
/// hub and its teams.
fn status() -> Result<ExitCode, String> {
    let runner = this_runner()?;
    println!(
        "runner {} (works for {}): {}",
        runner.name,
        runner.works_for.as_str(),
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
            let said = match crate::kept::Tokens::load()?.get(url) {
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

/// `ensemble-runner serve`: run this computer's runner until stopped. With a
/// hub, it runs the tasks the hub keeps for it with its teams, in the
/// repositories of its workspace or those it clones there, and tells the hub
/// it is connected, with its agents and what it offers. A change to
/// its settings has it start again, in this process: running tasks start again
/// from where their work was last pushed.
fn serve(ctx: &Ctx) -> Result<ExitCode, String> {
    // A new runner, as `init` makes it, when there is none: what installs
    // it only runs it.
    init(false, true)?;
    local::stop_on_signals();
    if !signed_in(ctx)? {
        eprintln!("ensemble: stopped");
        return Ok(ExitCode::SUCCESS);
    }
    let dir = state::runner_dir()?;
    let _lock = ensemble_core::lock::acquire(&dir).map_err(|pid| {
        let name = this_runner().map(|r| r.name).unwrap_or_default();
        format!("serve: process {pid} already runs the runner '{name}'")
    })?;
    loop {
        serve_once(ctx, &dir)?;
        if !local::restarting() {
            break;
        }
        eprintln!("ensemble: starting again with its settings as they are now");
    }
    eprintln!("ensemble: stopped");
    Ok(ExitCode::SUCCESS)
}

/// Wait until the hub its settings name has a token, as a runner that starts
/// before anyone signed it in does, such as a service at boot: true once it
/// has, or when it names no hub; false when it is stopped first. It holds
/// nothing meanwhile, so `ensemble-runner login` and `join` run as they would
/// with no runner, and it goes on from the token they keep.
fn signed_in(ctx: &Ctx) -> Result<bool, String> {
    const LOOK_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
    let mut said = false;
    loop {
        let runner = ctx.runner().map_err(|e| format!("serve: {e}"))?;
        let Some(url) = runner.hub else {
            return Ok(true);
        };
        if crate::kept::Tokens::load()?.get(&url).is_some() {
            return Ok(true);
        }
        if !said {
            eprintln!(
                "ensemble: no token for the hub {url} yet: waiting for `ensemble-runner login {url} --user <name>` or `ensemble-runner join {url} <code>`"
            );
            said = true;
        }
        let since = std::time::Instant::now();
        while since.elapsed() < LOOK_EVERY {
            if local::STOP.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(false);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

/// The runner, as its settings say now, until it stops or starts again.
fn serve_once(ctx: &Ctx, dir: &Path) -> Result<(), String> {
    let runner = ctx.runner().map_err(|e| format!("serve: {e}"))?;
    let say = |line: &str| eprintln!("ensemble: {line}");
    // With a hub: its tasks, and the link that says the runner is there.
    // One token for all of it, replaced when `ensemble-runner login` signs in
    // again.
    let hub = match &runner.hub {
        Some(_) => {
            let (url, token) = runner_hub("serve")?;
            Some((url, ensemble_link::Token::new(&token)))
        }
        None => None,
    };
    // The relay, when it shares backends or reaches others' through the hub,
    // and where its agents reach those.
    let relay = hub
        .as_ref()
        .filter(|_| !runner.llm.shares.is_empty() || !runner.llm.hub.is_empty())
        .map(|(url, token)| {
            std::sync::Arc::new(crate::relay::Relay::new(url, token.clone(), &runner))
        });
    let loopback = match relay.as_ref().filter(|_| !runner.llm.hub.is_empty()) {
        Some(relay) => Some(
            ensemble_core::runtime::shared()
                .block_on(ensemble_link::loopback::Loopback::start(
                    config::llm::routes(&runner.llm.hub),
                    std::sync::Arc::new(crate::relay::RelayOpener(relay.clone())),
                ))
                .map_err(|e| format!("serve: the relay's endpoint: {e}"))?,
        ),
        None => None,
    };
    let relay_at = loopback.as_ref().map(|l| config::RelayAt {
        base_url: l.base_url.clone(),
        key: l.key.clone(),
    });
    let (team, waiting) =
        teams::load_all(&runner.llm, relay_at.as_ref()).map_err(|e| format!("serve: {e}"))?;
    for line in &waiting {
        say(line);
    }
    // What `ensemble-runner …` asks of it while it runs.
    let operations = operations();
    let daemon = daemon::Daemon::new(
        runner.clone(),
        hub.clone(),
        team.clone(),
        relay_at,
        &operations,
        &say,
    )
    .map_err(|e| format!("serve: {e}"))?;
    let worker = hub.as_ref().map(|(url, token)| {
        crate::worker::Worker::new(url, token.clone(), runner.clone(), team.clone())
    });
    // What else holds the teams, given them again when they are reloaded.
    let reloaded = |config: &Config| -> Result<(), String> {
        if let Some(worker) = &worker {
            worker.reload(config);
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
        let relaying = async {
            if let Some(relay) = &relay
                && let Err(e) = relay.work(stop, &say).await
            {
                *refused.lock().unwrap() = Some(e);
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        };
        let (served, (), ()) = ensemble_core::runtime::shared()
            .block_on(async { tokio::join!(daemon.serve(&socket, &reloaded), link, relaying) });
        local::STOP.store(true, std::sync::atomic::Ordering::Relaxed);
        served
    };
    served.map_err(|e| format!("serve: {e}"))?;
    if let Some(e) = refused.into_inner().unwrap() {
        return Err(format!("serve: the hub refused this runner: {e}"));
    }
    Ok(())
}

/// What the runner's operations are given: its settings, kept in its
/// database.
pub struct RunnerContext {
    settings: Kept<RunnerSettings>,
    /// Its folder.
    dir: RunnerDir,
    /// In the running runner: read its hub's token again, and say what it did.
    relink: Option<Box<Relink>>,
}

type Relink = dyn Fn() -> Result<String, String> + Send + Sync;

/// The runner's settings, in its database, checked as a runner before they are
/// written.
fn runner_settings() -> Result<Kept<RunnerSettings>, String> {
    let dir = kept::dir()?;
    let checked = dir.clone();
    Ok(Kept::new(
        kept::settings(&dir),
        kept::SETTINGS_NAME,
        "`ensemble-runner setup`",
        move |s: &RunnerSettings| kept::resolve(s, &checked).map(|_| ()),
    ))
}

impl WithKept<RunnerSettings> for RunnerContext {
    fn kept(&self) -> Result<&Kept<RunnerSettings>, Error> {
        Ok(&self.settings)
    }
}

/// Its keys are kept sealed in its database ([`crate::kept`]).
impl WithBackends for RunnerContext {
    fn can_keep_keys(&self) -> Result<(), Error> {
        kept::keys(&self.dir).ready().map_err(Error::failed)
    }

    fn keep_key(&self, name: &str, key: Option<&str>) -> Result<(), Error> {
        kept::keys(&self.dir).keep(name, key).map_err(Error::failed)
    }

    fn variable(&self, name: &str) -> Option<String> {
        use ensemble_config::Env;
        kept::variables(&self.dir).ok()?.get(name)
    }

    fn kept_backends(&self) -> Result<KeptBackends, Error> {
        let s = self.settings.read()?;
        Ok(KeptBackends {
            backends: s.backends,
            shares: Some(s.shares),
        })
    }

    /// The teams name the runner's backends: a change that would leave one
    /// naming a backend that is not there is refused.
    fn change_backends(
        &self,
        change: Change,
        edit: &mut dyn FnMut(&mut KeptBackends) -> Result<(), Error>,
    ) -> Result<Option<String>, Error> {
        self.settings.change(change, |s| {
            let mut kept = KeptBackends {
                backends: std::mem::take(&mut s.backends),
                shares: Some(std::mem::take(&mut s.shares)),
            };
            edit(&mut kept)?;
            s.backends = kept.backends;
            s.shares = kept.shares.unwrap_or_default();
            let runner = kept::resolve(s, &self.dir)
                .map_err(|e| Error::invalid(format!("nothing was changed: {e}")))?;
            teams::load_all(&runner.llm, None)
                .map(|_| ())
                .map_err(|e| Error::new(Code::Conflict, format!("nothing was changed: {e}")))
        })
    }

    fn backends_changed(&self) -> Option<String> {
        self.settings.changed()
    }

    fn backends(&self) -> Result<Backends, Error> {
        let runner = self.runner()?;
        Ok(Backends {
            direct: runner.llm.direct,
            relayed: runner.llm.hub,
            shares: Some(runner.llm.shares),
        })
    }
}

impl RunnerContext {
    /// This computer's runner, which does not run: its settings, checked as
    /// a runner before they are written.
    pub fn new() -> Result<Self, String> {
        Ok(RunnerContext {
            settings: runner_settings()?,
            dir: kept::dir()?,
            relink: None,
        })
    }

    /// Its settings as they are now, with the keys they name.
    fn runner(&self) -> Result<config::Runner, Error> {
        kept::load()
            .map_err(|e| Error::new(Code::Unavailable, e))?
            .ok_or_else(|| {
                Error::not_found("there is no runner yet").hint("`ensemble-runner setup` makes one")
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
            dir: kept::dir()?,
            relink: Some(Box::new(relink)),
        })
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
struct Login {
    url: String,
    user: Option<String>,
    token: Option<String>,
    password: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct Join {
    url: String,
    code: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
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

#[derive(serde::Deserialize, serde::Serialize)]
struct Nothing {}

#[derive(serde::Deserialize, serde::Serialize)]
struct Named {
    name: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct Workspace {
    path: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct WorksForInput {
    whom: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct SharedCalls {
    most: u64,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct Import {
    settings: String,
}

const NAME: &[Field] = &[Field::text(
    "name",
    "The runner's name, which a hub knows as <you>/<name>: letters, digits, '-', '_' or '.'.",
)
.required()
.positional()];
const WORKSPACE: &[Field] = &[Field::text(
    "path",
    "The folder it works in, of git repositories; ~/ is your home.",
)
.required()
.positional()];
const WORKS_FOR: &[Field] = &[Field::new(
    "whom",
    ensemble_ops::Kind::Choice(&["owner", "hub"]),
    "owner: you alone; hub: also everyone on the hub whose role has `compute`, with the agents of its teams on LLM backends.",
)
.required()
.positional()];
const SHARED_CALLS: &[Field] = &[Field::new(
    "most",
    ensemble_ops::Kind::Number,
    "The most calls of others to its shared backends at once.",
)
.required()
.positional()];
const IMPORT: &[Field] = &[Field::text(
    "settings",
    "Settings as `settings show --json` prints them, whole.",
)
.required()
.positional()];

/// An operation that changes the runner's settings: `run` given the change it
/// makes, for the audit log.
fn changing<I>(
    spec: Spec,
    run: impl Fn(&RunnerContext, I, Change) -> Result<serde_json::Value, Error> + Send + Sync + 'static,
) -> ensemble_ops::Operation<RunnerContext>
where
    I: serde::de::DeserializeOwned + serde::Serialize,
{
    operation(spec, move |c: &RunnerContext, caller: &Caller, input: I| {
        let change = Change::new(caller, &spec, &input);
        run(c, input, change)
    })
}

/// Make `edit` to the runner's settings and keep them with `change`, once
/// they load; say `said`, and what the running runner did with them.
fn edit(
    c: &RunnerContext,
    change: Change,
    said: String,
    edit: impl FnOnce(&mut RunnerSettings) -> Result<(), Error>,
) -> Result<serde_json::Value, Error> {
    let note = c.settings.change(change, edit)?;
    Ok(serde_json::json!({ "said": said, "note": note }))
}

/// Every operation of the runner.
pub fn operations() -> Registry<RunnerContext> {
    let local = Access::Local;
    let mut all = vec![
        operation(
            Spec {
                name: "settings.show",
                summary: "Show every setting of the runner; its keys are never among them.",
                access: local,
                input: &[],
            },
            |c: &RunnerContext, _: &Caller, _: Nothing| {
                let s = serde_json::to_value(c.settings.read()?)
                    .map_err(|e| Error::failed(e.to_string()))?;
                Ok(serde_json::json!({ "settings": s }))
            },
        )
        .shown(|out| ensemble_ops::Said {
            out: ensemble_ops::lines(&out["settings"]),
            note: None,
        }),
        operation(
            Spec {
                name: "settings.history",
                summary: "Show who changed the runner's settings, when, and with which operation.",
                access: local,
                input: &[],
            },
            |c: &RunnerContext, _: &Caller, _: Nothing| {
                let log = ensemble_store::kept::audit(&c.dir.core()).map_err(Error::failed)?;
                let said: Vec<String> = log
                    .iter()
                    .map(|a| format!("{}  {}  {}  {}", a.at, a.who, a.operation, a.input))
                    .collect();
                Ok(serde_json::json!({
                    "said": if said.is_empty() { "no changes yet".to_string() } else { said.join("\n") },
                    "changes": log,
                }))
            },
        ),
        operation(
            Spec {
                name: "settings.import",
                summary: "Replace every setting with settings exported from a runner, once they load.",
                access: local,
                input: IMPORT,
            },
            |c: &RunnerContext, caller: &Caller, input: Import| {
                let spec = Spec {
                    name: "settings.import",
                    summary: "",
                    access: Access::Local,
                    input: IMPORT,
                };
                let change = Change::new(caller, &spec, serde_json::json!({}));
                let value: serde_json::Value = serde_json::from_str(&input.settings)
                    .map_err(|e| Error::invalid(format!("not JSON: {e}")).on("settings"))?;
                let settings: RunnerSettings =
                    serde_json::from_value(value.get("settings").cloned().unwrap_or(value))
                        .map_err(|e| {
                            Error::invalid(format!("not the runner's settings: {e}")).on("settings")
                        })?;
                let note = c.settings.keep(&settings, &change)?;
                Ok(
                    serde_json::json!({ "said": "the runner's settings are those imported", "note": note }),
                )
            },
        ),
        changing(
            Spec {
                name: "settings.name",
                summary: "Name the runner, as its hub knows it.",
                access: local,
                input: NAME,
            },
            |c, input: Named, change| {
                let name = input.name.trim().to_string();
                edit(c, change, format!("the runner is {name}"), |s| {
                    s.name = name.clone();
                    Ok(())
                })
            },
        ),
        changing(
            Spec {
                name: "settings.workspace",
                summary: "Say which folder the runner works in: each git repository in it is a project.",
                access: local,
                input: WORKSPACE,
            },
            |c, input: Workspace, change| {
                let path = input.path.trim().to_string();
                edit(c, change, format!("the runner works in {path}"), |s| {
                    s.workspace = path.clone();
                    Ok(())
                })
            },
        ),
        changing(
            Spec {
                name: "settings.works-for",
                summary: "Say whom the runner works for: you alone, or also the hub's people with `compute`.",
                access: local,
                input: WORKS_FOR,
            },
            |c, input: WorksForInput, change| {
                let whom: WorksFor = serde_json::from_value(serde_json::json!(input.whom))
                    .map_err(|_| Error::invalid("owner or hub").on("whom"))?;
                edit(
                    c,
                    change,
                    format!("the runner works for its {}", whom.as_str()),
                    |s| {
                        s.works_for = whom;
                        Ok(())
                    },
                )
            },
        ),
        changing(
            Spec {
                name: "settings.shared-calls",
                summary: "Say how many calls of others to its shared backends it takes at once.",
                access: local,
                input: SHARED_CALLS,
            },
            |c, input: SharedCalls, change| {
                let most = u32::try_from(input.most)
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| Error::invalid("must be greater than 0").on("most"))?;
                edit(
                    c,
                    change,
                    format!("it takes {most} shared calls at once"),
                    |s| {
                        s.max_shared_calls = most;
                        Ok(())
                    },
                )
            },
        ),
    ];
    all.extend(ensemble_backends::ops::operations::<RunnerContext>(
        Side::Runner,
        Access::Local,
    ));
    all.push(changing(
        Spec {
            name: "login",
            summary: "Sign in to a hub (ws:// or wss://) and keep its token for the runner.",
            access: Access::Local,
            input: LOGIN,
        },
        |c, input: Login, change| login(c, input, change),
    ));
    all.push(changing(
        Spec {
            name: "join",
            summary: "Join a hub with a code made for your account, and keep its token for the runner.",
            access: Access::Local,
            input: JOIN,
        },
        |c, input: Join, change| {
            let url = hub_url(&input.url)?;
            let params = serde_json::json!({ "code": input.code.trim(), "runner": runner_name() });
            let token = sign_in(url, ensemble_link::protocol::JOIN, params)?;
            keep(c, url, &token, change)
        },
    ));
    all.push(changing(
        Spec {
            name: "logout",
            summary: "Forget a hub's token.",
            access: Access::Local,
            input: LOGOUT,
        },
        |c, input: Logout, change| logout(c, &input.url, change),
    ));
    Registry::new(all)
}

/// `login`: sign in to a hub over the link with your account, or take a token
/// made elsewhere, and keep it.
fn login(c: &RunnerContext, input: Login, change: Change) -> Result<serde_json::Value, Error> {
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
    keep(c, url, &token, change)
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
fn keep(
    c: &RunnerContext,
    url: &str,
    token: &str,
    change: Change,
) -> Result<serde_json::Value, Error> {
    let hello = ensemble_core::runtime::block_on(ensemble_link::Link::new(url, token).hello())
        .map_err(|e| Error::new(Code::Unauthorized, e).on("token"))?;
    let mut tokens = crate::kept::Tokens::load()?;
    tokens.set(url, token);
    tokens.save()?;
    let mut said = vec![format!(
        "signed in to {url} as {} ({}: {})",
        hello.user,
        hello.role,
        hello.permissions.join(", ")
    )];
    // The runner names the hub it uses.
    let settings = c.settings.read().ok();
    if let Some(settings) = settings {
        let named = settings.hub;
        if named.as_deref() == Some(url) {
            // The running runner takes the new token as it is.
            if let Some(relink) = &c.relink {
                said.push(match relink() {
                    Ok(relinked) => format!("The running runner: {relinked}."),
                    Err(e) => e,
                });
            }
        } else {
            let set = c.settings.change(change, |s| {
                s.hub = Some(url.to_string());
                Ok(())
            });
            match set {
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
    } else {
        said.push(
            "There is no runner yet: `ensemble-runner setup` makes one; then sign in again, and it names this hub.".into(),
        );
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
fn logout(c: &RunnerContext, url: &str, change: Change) -> Result<serde_json::Value, Error> {
    let mut tokens = crate::kept::Tokens::load()?;
    let mut said = Vec::new();
    if tokens.remove(url) {
        tokens.save()?;
        said.push(format!("signed out of {url}"));
    } else {
        said.push(format!("no token kept for {url}"));
    }
    // A runner that names it would reach no hub: it names none now.
    let names = c
        .settings
        .read()
        .ok()
        .and_then(|s| s.hub)
        .is_some_and(|h| h == url);
    if names {
        let unset = c.settings.change(change, |s| {
            s.hub = None;
            Ok(())
        });
        match unset {
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

/// `ensemble-runner peers`: the runners of your runner's hub that work for
/// you now: yours, and others' that work for the hub with what they lend you.
fn peers() -> Result<ExitCode, String> {
    let (url, token) = runner_hub("peers")?;
    let token = token.as_str();
    let listed = ensemble_core::runtime::block_on(async {
        let mut link = ensemble_link::client::Connection::open(&url, token).await?;
        let listed = link
            .call(ensemble_link::protocol::RUNNERS, serde_json::json!({}))
            .await
            .map_err(|f| f.message());
        link.close().await;
        listed
    })?;
    let runners = listed["runners"].as_array().cloned().unwrap_or_default();
    if runners.is_empty() {
        println!("No runner you may use is connected to {url}.");
    }
    for n in runners {
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
            "{}  (works for {})\n  agents: {}\n  models: {}",
            n["runner"].as_str().unwrap_or("?"),
            n["works_for"].as_str().unwrap_or("?"),
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
    let dir = kept::dir()?;
    let store = kept::settings(&dir);
    if store.read()?.is_some() {
        println!("This computer has a runner already; keeping it.");
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
        let settings = setup::starting(&name, &workspace)?;
        store.write(&settings, &Change::by_this_computer("setup"))?;
        println!(
            "Made the runner '{name}', working in {workspace}, kept in {}.",
            dir.database().display()
        );
    }
    println!();
    let runner = ctx.runner().map_err(|e| format!("setup: {e}"))?;
    let (team, waiting) = teams::load_all(&runner.llm, None)?;
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
