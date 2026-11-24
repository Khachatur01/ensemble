//! `ensemble-runner agent …`: the agents of the runner's teams, named
//! `<team>/<agent>`: how each runs, one task sent to one, their health and
//! record, and adding or removing one in its team's config.yaml.

use crate::cli::AgentCommand;
use crate::teams;
use crate::{Ctx, command_line};
use ensemble_agents::external::{claude_code, opencode};
use ensemble_agents::router::Ensemble;
use ensemble_config::{self as config, Adapter, Config};
use std::path::PathBuf;
use std::process::ExitCode;

pub fn run(ctx: &Ctx, command: AgentCommand) -> Result<ExitCode, String> {
    match command {
        AgentCommand::List => list(ctx.teams()?, ctx.dir.clone()),
        AgentCommand::Show { agent, probe } => show(&ctx.teams()?, &agent, probe),
        AgentCommand::Run {
            agent,
            task,
            context,
        } => run_one(
            ctx.teams()?,
            ctx.dir.clone(),
            &agent,
            &task.join(" "),
            context,
        ),
        AgentCommand::Health => health(ctx.teams()?, ctx.dir.clone()),
        AgentCommand::Stats { since, json } => stats(ctx.teams()?, ctx.dir.clone(), since, json),
        AgentCommand::Add {
            agent,
            adapter,
            model,
            source,
            prompt,
            writes,
        } => add(
            &agent,
            &config::edit::NewAgent {
                adapter: &adapter,
                prompt: prompt.as_deref(),
                model: model.as_deref(),
                source: source.as_deref(),
                writes,
            },
        ),
        AgentCommand::Remove { agent } => remove(&agent),
    }
}

/// The team and the name of `agent`, `<team>/<name>`, and its team's
/// config.yaml, which only a team made here or linked may have changed.
fn in_team(agent: &str, command: &str) -> Result<(String, PathBuf), String> {
    let (team, name) = agent
        .split_once('/')
        .ok_or_else(|| format!("{command}: name the agent as <team>/<name>, such as web/coder"))?;
    let dir = teams::editable(team).map_err(|e| format!("{command}: {e}"))?;
    Ok((name.to_string(), config::config_path(&dir)))
}

fn list(config: Config, dir: PathBuf) -> Result<ExitCode, String> {
    let ensemble = Ensemble::new(config, Some(dir));
    for role in ensemble.agents() {
        let (runs_on, model) = ensemble.route_of(&role).map_err(|e| e.to_string())?;
        println!("{role}\t{runs_on}\t{model}");
    }
    Ok(ExitCode::SUCCESS)
}

fn health(config: Config, dir: PathBuf) -> Result<ExitCode, String> {
    let ensemble = Ensemble::new(config, Some(dir));
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

fn run_one(
    config: Config,
    dir: PathBuf,
    role: &str,
    task: &str,
    context: Option<String>,
) -> Result<ExitCode, String> {
    if task.trim().is_empty() {
        return Err("agent run: needs a task".into());
    }
    let ensemble = Ensemble::new(config, Some(dir));
    let outcome =
        ensemble_core::runtime::block_on(ensemble.delegate(role, task, context.as_deref()))
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
fn show(config: &Config, name: &str, probe: bool) -> Result<ExitCode, String> {
    let agent = config.agent(name).ok_or_else(|| {
        format!(
            "agent show: no agent '{name}'. Agents: {}.",
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
            let argv = ensemble_agents::external::codex::arguments(agent, cx, TASK);
            println!("{}", command_line(&cx.command, &argv));
        }
        Adapter::Direct(la) => {
            let (path, body) = match config.llm_backends.get(la.source()) {
                Some(backend) => {
                    let (path, body) = ensemble_agents::external::direct::request_for(
                        backend, agent, la, TASK, None,
                    );
                    (format!("{}{path}", backend.settings.base_url()), body)
                }
                None => (
                    format!("<no source {}>", la.source()),
                    ensemble_agents::external::direct::request(agent, la, TASK, None),
                ),
            };
            println!("POST {path}");
            println!(
                "{}",
                serde_json::to_string_pretty(&body).map_err(|e| e.to_string())?
            );
        }
        Adapter::OpenCode(oc) => {
            let backend = oc.source().and_then(|b| config.llm_backends.get(b));
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
            let backend = config.llm_backends.get(ba.source()).map_or_else(
                || format!("<no source {}>", ba.source()),
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
        println!("  {}. {}:{}", i + 1, c.source, c.model);
    }
    if probe {
        let candidates: Vec<(&config::LlmBackend, &str)> = agent
            .pool_models()
            .into_iter()
            .filter_map(|(b, m)| config.llm_backends.get(b).map(|b| (b, m)))
            .collect();
        match ensemble_backends::resolve::Resolver::default().choose(&candidates) {
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

fn stats(
    config: Config,
    dir: PathBuf,
    since: Option<String>,
    json: bool,
) -> Result<ExitCode, String> {
    let ensemble = Ensemble::new(config, Some(dir));
    let records_dir = ensemble
        .records_dir()
        .ok_or("stats: cannot tell where runs are recorded")?;
    let since = match since {
        Some(text) => {
            let ago = ensemble_core::records::parse_duration(&text)
                .map_err(|e| format!("--since: {e}"))?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            Some(now.saturating_sub(ago))
        }
        None => None,
    };
    let summaries =
        ensemble_core::records::summarize(&ensemble_core::records::read(&records_dir), since);
    if json {
        println!("{}", ensemble_core::records::json(&summaries));
    } else {
        print!("{}", ensemble_core::records::table(&summaries));
    }
    Ok(ExitCode::SUCCESS)
}

/// `ensemble-runner agent add <team>/<name>`: an agent added to its team, every
/// setting written.
fn add(agent: &str, new: &config::edit::NewAgent) -> Result<ExitCode, String> {
    let (name, path) = in_team(agent, "agent add")?;
    let name = name.as_str();
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("agent add: cannot read {}: {e}", path.display()))?;
    let block = config::edit::agent_block(name, new).map_err(|e| format!("agent add: {e}"))?;
    let edited =
        config::edit::add_agent(&text, name, &block).map_err(|e| format!("agent add: {e}"))?;
    config::parse(&edited, &path).map_err(|e| {
        format!("agent add: the result would not load, so nothing was written: {e}")
    })?;
    std::fs::write(&path, edited).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!("added {name} to {}", path.display());
    Ok(ExitCode::SUCCESS)
}

/// `ensemble-runner agent remove <team>/<name>`.
fn remove(agent: &str) -> Result<ExitCode, String> {
    let (name, path) = in_team(agent, "agent remove")?;
    let name = name.as_str();
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("agent remove: cannot read {}: {e}", path.display()))?;
    let edited =
        config::edit::remove_agent(&text, name).map_err(|e| format!("agent remove: {e}"))?;
    config::parse(&edited, &path).map_err(|e| {
        format!("agent remove: the result would not load, so nothing was written: {e}")
    })?;
    std::fs::write(&path, edited).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!("removed {name} from {}", path.display());
    Ok(ExitCode::SUCCESS)
}
