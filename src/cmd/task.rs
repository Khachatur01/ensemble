//! `ensemble task …`: the tasks of the folder you work in, kept on your
//! node's hub under its project, the folder's name. Your nodes run them; these
//! commands look at them, make them and steer them, and only you merge a
//! task's work.

use crate::cli::{NewTask, TaskCommand};
use crate::{Ctx, board_for, state_for, tell_node};
use ensemble::board::{Board, Cancelled};
use ensemble::config::Config;
use ensemble::tasks;
use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;

pub fn run(ctx: &Ctx, command: TaskCommand) -> Result<ExitCode, String> {
    let dir = &ctx.dir;
    match command {
        TaskCommand::List { status } => list(dir, status.as_deref()),
        TaskCommand::Show { id } => show(dir, &id),
        TaskCommand::New(t) => new(&ctx.teams()?, dir, *t),
        TaskCommand::Assign { id, agent } => assign(&ctx.teams()?, dir, &id, &agent),
        TaskCommand::Answer { id, text } => answer(dir, &id, &text.join(" ")),
        TaskCommand::Cancel { id } => cancel(dir, &id),
        TaskCommand::Review { id } => branch(dir, &id, Look::Review),
        TaskCommand::Diff { id } => branch(dir, &id, Look::Diff),
        TaskCommand::Merge {
            id,
            squash,
            cleanup,
        } => branch(dir, &id, Look::Merge { squash, cleanup }),
    }
}

fn list(dir: &Path, wanted: Option<&str>) -> Result<ExitCode, String> {
    let board = board_for(dir, "task list")?;
    let all = ensemble::runtime::block_on(board.scan()).map_err(|e| format!("task list: {e}"))?;
    let mut shown = 0;
    for task in &all {
        let line = tasks::entry_line(task, &all, None);
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
                None => format!("No tasks on {}.", board.place()),
            }
        );
    }
    if wanted.is_none()
        && let Some(idle) = tasks::idle_line(&all)
    {
        println!("{idle}");
    }
    Ok(ExitCode::SUCCESS)
}

fn show(dir: &Path, id: &str) -> Result<ExitCode, String> {
    let board = board_for(dir, "task show")?;
    let (all, detail) = ensemble::runtime::block_on(async {
        Ok::<_, String>((board.scan().await?, board.get(id).await?))
    })
    .map_err(|e| format!("task show: {e}"))?;
    let Some(detail) = detail else {
        return Err(format!(
            "task show: no task '{id}'. `ensemble task list` lists them."
        ));
    };
    let t = &detail.task.ticket;
    println!("{}", tasks::entry_line(&detail.task, &all, None));
    println!("\n{}", t.prompt());
    if let Some(repo) = &t.repo {
        println!("\nrepo: {repo}");
    }
    if let Some(gate) = &t.gate {
        println!("gate: {} ({} attempts)", gate.name, gate.attempts);
    }
    if let Some(q) = &detail.task.question {
        println!(
            "\nWaiting for an answer to: {q}\nAnswer it with: ensemble task answer {id} <text>"
        );
    }
    if let Some(outcome) = &detail.outcome {
        println!("\n{}", outcome.text);
    }
    Ok(ExitCode::SUCCESS)
}

fn answer(dir: &Path, id: &str, answer: &str) -> Result<ExitCode, String> {
    if answer.trim().is_empty() {
        return Err("task answer: needs the answer".into());
    }
    let board = board_for(dir, "task answer")?;
    ensemble::runtime::block_on(board.answer(id, answer))
        .map_err(|e| format!("task answer: {e}"))?;
    println!("answered {id}: {}.", tell_node(dir));
    Ok(ExitCode::SUCCESS)
}

/// `ensemble task new`: a task for an agent, or in the backlog without one.
fn new(config: &Config, dir: &Path, t: NewTask) -> Result<ExitCode, String> {
    let NewTask {
        title,
        agent,
        text,
        goal,
        context,
        repo,
        gate,
        after,
        files,
        constraints,
        done_when,
    } = t;
    let agent = agent.unwrap_or_default();
    let mut text = text;
    if text.is_none() && !std::io::stdin().is_terminal() {
        use std::io::Read;
        let mut piped = String::new();
        std::io::stdin()
            .read_to_string(&mut piped)
            .map_err(|e| format!("task new: cannot read the task from stdin: {e}"))?;
        text = Some(piped);
    }
    let text = text
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .ok_or("task new: needs the task: --text <text>, or pipe it in")?;
    if title.trim().is_empty() || title.contains('\n') {
        return Err("task new: the title is one non-empty line".into());
    }
    if !agent.is_empty() && config.agent(&agent).is_none() {
        return Err(format!(
            "task new: no agent '{agent}'. Agents: {}.",
            config.agent_names().join(", ")
        ));
    }
    let gate = match gate {
        None => None,
        Some(spec) => {
            let (name, attempts) = spec.split_once(':').unwrap_or((&spec, "1"));
            let attempts: u32 = attempts
                .parse()
                .ok()
                .filter(|n| *n >= 1)
                .ok_or("task new: --gate is name:attempts, with attempts at least 1")?;
            if config.gate_for(&agent, name).is_none() {
                return Err(format!("task new: no gate '{name}' under `gates`"));
            }
            Some(tasks::GateRef {
                name: name.to_string(),
                attempts,
            })
        }
    };
    if let Some(repo) = &repo {
        ensemble::workspace::resolve_repo(dir, repo).map_err(|e| format!("task new: {e}"))?;
    }
    let ticket = tasks::Ticket {
        title: title.trim().to_string(),
        agent,
        context,
        goal,
        constraints,
        files,
        done_when,
        after,
        repo,
        gate,
        text,
        ..Default::default()
    };
    let board = board_for(dir, "task new")?;
    let id =
        ensemble::runtime::block_on(board.create(&ticket)).map_err(|e| format!("task new: {e}"))?;
    println!("task {id} kept on {}: {}.", board.place(), tell_node(dir));
    Ok(ExitCode::SUCCESS)
}

fn assign(config: &Config, dir: &Path, id: &str, agent: &str) -> Result<ExitCode, String> {
    let to = config.agent(agent).map(|a| a.name.clone()).ok_or_else(|| {
        format!(
            "task assign: no agent '{agent}'. Agents: {}.",
            config.agent_names().join(", ")
        )
    })?;
    let board = board_for(dir, "task assign")?;
    ensemble::runtime::block_on(board.assign(id, &to)).map_err(|e| format!("task assign: {e}"))?;
    println!("{id} is assigned to {to}: {}.", tell_node(dir));
    Ok(ExitCode::SUCCESS)
}

/// `ensemble task cancel`: at once when it has not started; by its node, at
/// its next renewal, when it runs.
fn cancel(dir: &Path, id: &str) -> Result<ExitCode, String> {
    let board = board_for(dir, "task cancel")?;
    match ensemble::runtime::block_on(board.cancel(id)).map_err(|e| format!("task cancel: {e}"))? {
        Cancelled::Before => println!("{id} is cancelled; it had not started."),
        Cancelled::Running(node) => println!("{id} is being cancelled: {node} stops it."),
        Cancelled::Ended => println!("{id} had already ended."),
    }
    Ok(ExitCode::SUCCESS)
}

/// What is done with a task's branch.
enum Look {
    Review,
    Diff,
    Merge { squash: bool, cleanup: bool },
}

/// `ensemble task review|diff|merge`: a task's branch, for a person.
fn branch(dir: &Path, id: &str, look: Look) -> Result<ExitCode, String> {
    let command = match look {
        Look::Review => "task review",
        Look::Diff => "task diff",
        Look::Merge { .. } => "task merge",
    };
    let board = board_for(dir, command)?;
    let detail =
        ensemble::runtime::block_on(board.get(id)).map_err(|e| format!("{command}: {e}"))?;
    let task = ensemble::review::find(detail, dir, id).map_err(|e| format!("{command}: {e}"))?;
    match look {
        Look::Review => print!(
            "{}",
            ensemble::review::review(&task).map_err(|e| format!("task review: {e}"))?
        ),
        Look::Diff => println!(
            "{}",
            ensemble::review::diff(&task).map_err(|e| format!("task diff: {e}"))?
        ),
        Look::Merge { squash, cleanup } => {
            let worktree = state_for(dir)?
                .join(ensemble::worktree::WORKTREES_DIR)
                .join(id);
            let said = ensemble::review::merge(
                &task,
                &worktree,
                ensemble::review::MergeOptions { squash, cleanup },
            )
            .map_err(|e| format!("task merge: {e}"))?;
            println!("{said}");
        }
    }
    Ok(ExitCode::SUCCESS)
}
