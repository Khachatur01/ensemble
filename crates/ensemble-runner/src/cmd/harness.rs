//! `ensemble-runner harness …`: the runner's harnesses, the programs that
//! drive an LLM to work in a repository, as operations. One of each kind is
//! always there; a CLI is found on PATH or missing, and each is off until a
//! person turns it on.

use super::runner::{RunnerContext, changing, edit};
use crate::setup;
use ensemble_config::{Harness, HarnessChange, HarnessKind};
use ensemble_ops::{Access, Caller, Error, Field, Kind, Operation, Spec, operation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const HARNESS: Field = Field::new(
    "harness",
    Kind::Choice(HarnessKind::NAMES),
    "The harness: claude-code, codex, opencode or builtin.",
)
.required()
.positional();
const ONE: &[Field] = &[HARNESS];
const SET: &[Field] = &[
    HARNESS,
    Field::text(
        "command",
        "The CLI, by its name on PATH or its path; builtin runs none.",
    ),
    Field::new("max_seconds", Kind::Number, "Seconds one run may take."),
    Field::new("max_steps", Kind::Number, "The most steps of one run."),
    Field::new(
        "max_concurrent",
        Kind::Number,
        "The most runs on it at once.",
    ),
    Field::new(
        "max_tokens",
        Kind::Number,
        "The most tokens of answer per model call: builtin's alone.",
    ),
];

#[derive(Deserialize, Serialize)]
struct One {
    harness: String,
}

#[derive(Deserialize, Serialize)]
struct Set {
    harness: String,
    command: Option<String>,
    max_seconds: Option<u64>,
    max_steps: Option<u32>,
    max_concurrent: Option<u32>,
    max_tokens: Option<u32>,
}

#[derive(Deserialize, Serialize)]
struct Nothing {}

/// The operations on the runner's harnesses.
pub fn operations() -> Vec<Operation<RunnerContext>> {
    let spec = |name, summary, input| Spec {
        name,
        summary,
        access: Access::Local,
        input,
    };
    vec![
        operation(
            spec(
                "harness.list",
                "List the harnesses: each found or missing, on or off, logged in where its CLI says, how long and how many steps a run may take, how many run at once, and what it drives.",
                &[],
            ),
            |c: &RunnerContext, _: &Caller, _: Nothing| list(c),
        ),
        changing(
            spec(
                "harness.on",
                "Let tasks run on a harness; one that is missing must be found first.",
                ONE,
            ),
            |c, input: One, change| turn(c, &input.harness, true, change),
        ),
        changing(
            spec("harness.off", "Let no task run on a harness.", ONE),
            |c, input: One, change| turn(c, &input.harness, false, change),
        ),
        changing(
            spec(
                "harness.set",
                "Change a harness's command, how long and how many steps a run may take, or how many run at once.",
                SET,
            ),
            |c, input: Set, change| set(c, input, change),
        ),
        changing(
            spec("harness.refresh", "Look again for each harness's CLI.", &[]),
            |c, _: Nothing, change| refresh(c, change),
        ),
    ]
}

fn kind_of(name: &str) -> Result<HarnessKind, Error> {
    HarnessKind::parse(name).ok_or_else(|| {
        Error::invalid(format!(
            "there is no harness '{name}': it is one of {}",
            HarnessKind::NAMES.join(", ")
        ))
        .on("harness")
    })
}

/// What drives the LLMs of `kind`, in a few words.
fn drives(kind: HarnessKind) -> String {
    match kind {
        HarnessKind::ClaudeCode | HarnessKind::Codex => format!(
            "its own models: {}",
            ensemble_backends::catalog::harness_models(kind)
                .iter()
                .map(|(id, tier)| format!("{id} ({})", tier.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        HarnessKind::OpenCode => "LLMs of the ANN pool's OpenAI-compatible providers".into(),
        HarnessKind::Builtin => "LLMs of the ANN pool that make tool calls".into(),
    }
}

/// One harness as `harness.list` says it.
fn listed(h: &Harness) -> Value {
    let models: Vec<Value> = ensemble_backends::catalog::harness_models(h.kind)
        .iter()
        .map(|(id, tier)| json!({ "id": id, "tier": tier.as_str() }))
        .collect();
    json!({
        "harness": h.kind,
        "on": h.on,
        "missing": h.missing,
        "command": h.command,
        "max_seconds": h.max_seconds,
        "max_steps": h.max_steps,
        "max_concurrent": h.max_concurrent,
        "max_tokens": h.max_tokens,
        "logged_in": setup::logged_in(h.kind),
        "models": models,
        "drives": drives(h.kind),
    })
}

/// One harness in a line: `claude-code  on  claude, logged in; 1800 s and
/// 50 steps a run; its own models: …`.
fn line(h: &Harness) -> String {
    let state = if h.missing {
        "missing"
    } else if h.on {
        "on"
    } else {
        "off"
    };
    let program = match (&h.command, setup::logged_in(h.kind)) {
        (Some(c), Some(true)) => format!("{c}, logged in; "),
        (Some(c), Some(false)) => format!("{c}, not logged in; "),
        (Some(c), None) => format!("{c}; "),
        (None, _) => String::new(),
    };
    format!(
        "{:<12} {state:<8} {program}{} s and {} steps a run, {} at once; {}",
        h.kind.as_str(),
        h.max_seconds,
        h.max_steps,
        h.max_concurrent,
        drives(h.kind)
    )
}

fn list(c: &RunnerContext) -> Result<Value, Error> {
    let all = c.settings.read()?.harnesses.each();
    let said: Vec<String> = all.iter().map(line).collect();
    Ok(json!({
        "said": said.join("\n"),
        "harnesses": all.iter().map(listed).collect::<Vec<_>>(),
    }))
}

fn turn(
    c: &RunnerContext,
    name: &str,
    on: bool,
    change: ensemble_config::kept::Change,
) -> Result<Value, Error> {
    let kind = kind_of(name)?;
    let now = c.settings.read()?.harnesses.get(kind);
    if now.on == on {
        return Ok(
            json!({ "said": format!("{name} is {} already", if on { "on" } else { "off" }) }),
        );
    }
    if on && now.missing {
        let command = now.command.unwrap_or_default();
        return Err(Error::invalid(format!(
            "{name} is missing: `{command}` was not found when the runner last looked"
        ))
        .on("harness")
        .hint(format!(
            "install it, or name it with `harness set {name} --command <path>`, then `harness refresh`"
        )));
    }
    let said = format!(
        "{name} is {}",
        if on {
            "on: tasks may run on it"
        } else {
            "off: no task runs on it"
        }
    );
    edit(c, change, said, |s| {
        s.harnesses.change(
            kind,
            HarnessChange {
                on: Some(on),
                ..HarnessChange::default()
            },
        );
        Ok(())
    })
}

fn set(
    c: &RunnerContext,
    input: Set,
    change: ensemble_config::kept::Change,
) -> Result<Value, Error> {
    let kind = kind_of(&input.harness)?;
    let command = input
        .command
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    if input.command.is_some() && command.is_none() {
        return Err(Error::invalid("the command is empty").on("command"));
    }
    if command.is_some() && kind == HarnessKind::Builtin {
        return Err(
            Error::invalid("builtin runs no command: it is Ensemble's own loop").on("command"),
        );
    }
    if input.max_seconds == Some(0) {
        return Err(Error::invalid("`max_seconds` must be greater than 0").on("max_seconds"));
    }
    if input.max_steps == Some(0) {
        return Err(Error::invalid("`max_steps` must be greater than 0").on("max_steps"));
    }
    if input.max_concurrent == Some(0) {
        return Err(Error::invalid("`max_concurrent` must be greater than 0").on("max_concurrent"));
    }
    if input.max_tokens == Some(0) {
        return Err(Error::invalid("`max_tokens` must be greater than 0").on("max_tokens"));
    }
    if input.max_tokens.is_some() && kind != HarnessKind::Builtin {
        return Err(
            Error::invalid("only builtin takes `max_tokens`: a CLI keeps its own").on("max_tokens"),
        );
    }
    if command.is_none()
        && input.max_seconds.is_none()
        && input.max_steps.is_none()
        && input.max_concurrent.is_none()
        && input.max_tokens.is_none()
    {
        return Err(Error::invalid(
            "say what changes: --command, --max-seconds, --max-steps, --max-concurrent or --max-tokens",
        ));
    }
    let name = kind.as_str();
    edit(c, change, format!("{name} is changed"), |s| {
        if let (Some(command), Some(h)) = (command, s.harnesses.cli_mut(kind)) {
            h.command = command.to_string();
            h.missing = !setup::found(command);
        }
        s.harnesses.change(
            kind,
            HarnessChange {
                max_seconds: input.max_seconds,
                max_steps: input.max_steps,
                max_concurrent: input.max_concurrent,
                max_tokens: input.max_tokens,
                ..HarnessChange::default()
            },
        );
        Ok(())
    })
}

fn refresh(c: &RunnerContext, change: ensemble_config::kept::Change) -> Result<Value, Error> {
    let mut harnesses = c.settings.read()?.harnesses;
    let said = setup::look_for(&mut harnesses);
    if said.is_empty() {
        return Ok(json!({ "said": "every harness is as it was" }));
    }
    edit(c, change, said.join("; "), |s| {
        s.harnesses = harnesses.clone();
        Ok(())
    })
}

/// Look for the harnesses' CLIs as the runner starts, as this computer,
/// before it reads its settings: what is found is off until a person turns
/// it on, and one gone missing runs no task.
pub fn refresh_at_start(c: &RunnerContext) -> Result<String, Error> {
    let out = refresh(
        c,
        ensemble_config::kept::Change::by_this_computer("harness.refresh"),
    )?;
    Ok(out["said"].as_str().unwrap_or_default().to_string())
}
