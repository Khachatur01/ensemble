//! `ensemble-runner team …`: the runner's helper teams, kept in its teams
//! directory: made here, linked, pulled from git and pinned, or written by an
//! agent and approved.

use crate::cli::TeamCommand;
use crate::{Ctx, absolute, boards, reload_runner, this_runner};
use crate::{remote, setup, teams};
use ensemble_config::{self as config, Config};
use ensemble_ops::cli::confirm;
use ensemble_tasks::board::Board;
use ensemble_tasks::tasks;
use std::path::Path;
use std::process::ExitCode;

pub fn run(ctx: &Ctx, command: TeamCommand) -> Result<ExitCode, String> {
    match command {
        TeamCommand::List => list(ctx),
        TeamCommand::New { name, stdout } => new(name, stdout),
        TeamCommand::Link { name, dir } => link(&name, &dir),
        TeamCommand::Pull { source, name, yes } => pull(&source, name, yes),
        TeamCommand::Update { name, git_ref, yes } => update(&name, git_ref, yes),
        TeamCommand::Approve { name, yes } => approve(ctx, &name, yes),
        TeamCommand::Remove { name, yes } => remove(&name, yes),
        TeamCommand::Check => check(ctx.runner_if_any()?.as_ref(), ctx.teams()?),
        TeamCommand::Schema => {
            print!("{}", config::SCHEMA);
            Ok(ExitCode::SUCCESS)
        }
        TeamCommand::Config { name, action } => {
            let path = config::config_path(&teams::editable(&name)?);
            let checked = path.clone();
            let file = config::SettingsFile::new(path, "`ensemble-runner team new`", move |text| {
                config::parse(text, &checked)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })
            .on_change(|| {
                reload_runner();
                None
            });
            super::config::run(file, action)
        }
    }
}

/// `ensemble-runner team list`: the runner's teams, where each comes from, and
/// its agents.
fn list(_ctx: &Ctx) -> Result<ExitCode, String> {
    let list = teams::list()?;
    if list.is_empty() {
        println!(
            "No teams in {}. Make one with `ensemble-runner team new <name>`, or pull one with `ensemble-runner team pull <url>`.",
            teams::dir()?.display()
        );
    }
    for team in &list {
        println!("{}  ({})", team.name, team.describe());
        let loaded = teams::config_dir(team)
            .and_then(|dir| config::load(&config::config_path(&dir)).map_err(|e| e.to_string()));
        match loaded {
            Ok(config) => {
                for agent in config.agents.values() {
                    let about = agent
                        .prompt
                        .as_deref()
                        .map(ensemble_tasks::delegation::first_line)
                        .unwrap_or_default();
                    println!("  {}/{}: {about}", team.name, agent.name);
                }
            }
            Err(e) => println!("  cannot be read: {e}"),
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn new(name: Option<String>, stdout: bool) -> Result<ExitCode, String> {
    if stdout {
        print!("{}", config::SAMPLE);
        return Ok(ExitCode::SUCCESS);
    }
    let name = name.as_deref().ok_or("team new: needs the team's name")?;
    let dir = teams::make(name, config::SAMPLE).map_err(|e| format!("team new: {e}"))?;
    println!(
        "wrote {}: the team '{name}', a sample with every setting. Edit its agents, then check it with `ensemble-runner team check`. API keys go in ~/.config/ensemble/runner/.env or the environment; never in this directory.",
        config::config_path(&dir).display()
    );
    reload_runner();
    Ok(ExitCode::SUCCESS)
}

fn link(name: &str, dir: &Path) -> Result<ExitCode, String> {
    let target = absolute(dir)?;
    teams::link(name, &target).map_err(|e| format!("team link: {e}"))?;
    println!("linked {} as the team '{name}'.", target.display());
    reload_runner();
    Ok(ExitCode::SUCCESS)
}

fn pull(spec: &str, name: Option<String>, yes: bool) -> Result<ExitCode, String> {
    let (url, sha, git_ref) = match remote::parse_source(spec)? {
        remote::Source::Remote { url, sha, git_ref } => (url, sha, git_ref),
        remote::Source::Local(_) => {
            return Err(format!(
                "team pull: {spec} is a folder; link it with `ensemble-runner team link <name> {spec}`"
            ));
        }
    };
    let name = name.clone().unwrap_or_else(|| teams::name_of(&url));
    config::teams::check_new(&teams::dir()?, &name).map_err(|e| format!("team pull: {e}"))?;
    // Pinned to the commit it is now, and shown, before it is kept.
    let sha = match sha {
        Some(sha) => sha,
        None => remote::resolve(&url, git_ref.as_deref())?,
    };
    trust(&url, &sha, yes)?;
    teams::pin(&name, &url, &sha, true)?;
    println!(
        "pulled {} as the team '{name}'.",
        remote::pinned(&url, &sha)
    );
    reload_runner();
    Ok(ExitCode::SUCCESS)
}

fn update(name: &str, git_ref: Option<String>, yes: bool) -> Result<ExitCode, String> {
    let team = config::teams::entry(&teams::dir()?, name).ok_or_else(|| teams::missing(name))?;
    let config::teams::Origin::Pulled { url, commit } = &team.origin else {
        return Err(format!(
            "team update: '{name}' was not pulled; change its config.yaml where it is"
        ));
    };
    let new = remote::resolve(url, git_ref.as_deref())?;
    if *commit == new {
        println!("'{name}' is at {new} already.");
        return Ok(ExitCode::SUCCESS);
    }
    let fetched = remote::fetch(url, &new)?;
    match remote::diff(&fetched, commit, &new) {
        Ok(diff) if diff.trim().is_empty() => {
            println!("config.yaml did not change between {commit} and {new}.")
        }
        Ok(diff) => println!("{diff}"),
        Err(e) => println!("cannot show what changed since {commit}: {e}"),
    }
    trust(url, &new, yes)?;
    teams::pin(name, url, &new, false)?;
    println!("'{name}' is at {} now.", remote::pinned(url, &new));
    reload_runner();
    Ok(ExitCode::SUCCESS)
}

fn approve(ctx: &Ctx, name: &str, yes: bool) -> Result<ExitCode, String> {
    let team = config::teams::entry(&teams::dir()?, name).ok_or_else(|| teams::missing(name))?;
    if !team.pending {
        println!("'{name}' does not wait for approval.");
        return Ok(ExitCode::SUCCESS);
    }
    let runner = ctx.runner()?;
    let config = config::load(&config::config_path(&team.path))
        .and_then(|c| c.with_llm(&runner.llm, None))
        .map_err(|e| format!("team approve: {e}"))?;
    println!(
        "{} (written by an agent)\n",
        config::config_path(&team.path).display()
    );
    print!("{}", remote::describe(&config));
    println!();
    if !confirm(
        "Approve this team? It will run the commands above on this machine.",
        yes,
    )? {
        return Err("not approved, so it does not run.".into());
    }
    teams::approve(name)?;
    println!("approved '{name}'.");
    reload_runner();
    Ok(ExitCode::SUCCESS)
}

fn remove(name: &str, yes: bool) -> Result<ExitCode, String> {
    if !confirm(&format!("Remove the team '{name}' from this runner?"), yes)? {
        return Err("nothing was removed.".into());
    }
    teams::remove(name)?;
    println!("removed '{name}'.");
    reload_runner();
    Ok(ExitCode::SUCCESS)
}

/// Fetch `url` at `sha`, say what its config would run, and, if the person
/// approves, record that they trust exactly this commit.
fn trust(url: &str, sha: &str, yes: bool) -> Result<(), String> {
    let dir = remote::fetch(url, sha)?;
    // On this computer's backends, when it has a runner yet.
    let config = config::load(&config::config_path(&dir))
        .and_then(|c| match this_runner() {
            Ok(runner) => c.with_llm(&runner.llm, None),
            Err(_) => Ok(c),
        })
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
        yes,
    )? {
        return Err("not trusted, so nothing was done.".into());
    }
    remote::trust(url, sha)?;
    println!("Trusted {}.", remote::pinned(url, sha));
    Ok(())
}

/// `ensemble-runner team check`: every team, loaded and validated, reported as
/// one helper team, with the tasks on the hub this runner cannot run.
fn check(runner: Option<&config::Runner>, config: Config) -> Result<ExitCode, String> {
    // Reaching here means every team already loaded and validated; loading
    // returns an error that names the team otherwise.
    if config.agents.is_empty() {
        return Err(format!(
            "check: no team in {} has agents. Make one with `ensemble-runner team new <name>`, or pull one with `ensemble-runner team pull <url>`.",
            config.source.display()
        ));
    }
    for team in teams::list()?.iter().filter(|t| !t.pending) {
        println!("{}: ok ({})", team.name, team.describe());
    }
    println!();
    println!("LLM backends, of runner.yaml:");
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
    for (name, r) in runner.iter().flat_map(|r| &r.llm.hub) {
        println!(
            "  {name}  [through the hub]  {}'s {}, while `ensemble-runner serve` runs",
            r.runner, r.backend
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

    // A task this runner would run fails only when it starts; say so now.
    let mut bad = Vec::new();
    for (project, board) in boards("check").unwrap_or_default() {
        match ensemble_core::runtime::block_on(board.scan()) {
            Ok(all) => {
                let unfinished: Vec<&tasks::Task> =
                    all.iter().filter(|t| t.status.is_none()).collect();
                let mut elsewhere = Vec::new();
                let mut failing = Vec::new();
                for t in &unfinished {
                    let t = &t.ticket;
                    if t.agent.is_empty() {
                        continue;
                    }
                    if config.agent(&t.agent).is_none() {
                        elsewhere.push(format!(
                            "{}: agent '{}' is not one of this runner's; another of your runners runs it",
                            t.id, t.agent
                        ));
                    } else if let Some(gate) = &t.gate
                        && config.gate_for(&t.agent, &gate.name).is_none()
                    {
                        failing.push(format!(
                            "{}: gate '{}' is not configured under `gates`; it would fail at once",
                            t.id, gate.name
                        ));
                    }
                }
                if !all.is_empty() {
                    println!();
                    println!(
                        "tasks of {project}: {} on {}, {} unfinished",
                        all.len(),
                        board.place(),
                        unfinished.len()
                    );
                    for line in failing.iter().chain(&elsewhere) {
                        println!("  {line}");
                    }
                }
                bad.extend(failing);
            }
            Err(e) => {
                println!();
                println!(
                    "tasks of {project}: cannot read them on {}: {e}",
                    board.place()
                );
            }
        }
    }

    println!();
    println!("ready. `ensemble-runner agent health` checks the agent CLIs and LLM backends.");
    Ok(if bad.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
