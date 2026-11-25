//! `ensemble-runner harness …`: the runner's harnesses, the programs that
//! drive an LLM to work in a repository, as operations. A runner has only the
//! harnesses a person added, at most one of each kind; a CLI is found on PATH
//! or missing, and Claude Code and Codex drive only the models of their own
//! vendor's a person added to them.

use super::runner::{RunnerContext, changing, edit};
use crate::setup;
use ensemble_config::{
    BuiltinHarness, CliHarness, Harness, HarnessChange, HarnessKind, HarnessModel, Tier,
};
use ensemble_ops::{Access, Caller, Code, Error, Field, Kind, Operation, Spec, operation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const HARNESS: Field = Field::new(
    "harness",
    Kind::Choice(HarnessKind::NAMES),
    "The harness: claude-code, codex, opencode or builtin.",
)
.required()
.positional();
const OWN: Field = Field::new(
    "harness",
    Kind::Choice(&["claude-code", "codex"]),
    "The harness that drives models of its own vendor's: claude-code or codex.",
)
.required()
.positional();
const MODEL: Field = Field::text(
    "model",
    "The model, as the CLI takes it, such as sonnet or gpt-5-codex.",
)
.required()
.positional();
const COMMAND: Field = Field::text(
    "command",
    "The CLI, by its name on PATH or its path, such as claude, codex or opencode: needed for each of them; builtin runs none.",
);
const MAX_SECONDS: Field = Field::new("max_seconds", Kind::Number, "Seconds one run may take.");
const MAX_STEPS: Field = Field::new("max_steps", Kind::Number, "The most steps of one run.");
const MAX_CONCURRENT: Field = Field::new(
    "max_concurrent",
    Kind::Number,
    "The most runs on it at once.",
);
const MAX_TOKENS: Field = Field::new(
    "max_tokens",
    Kind::Number,
    "The most tokens of answer per model call: builtin's alone, and builtin needs it.",
);
const ONE: &[Field] = &[HARNESS];
const ADD: &[Field] = &[
    HARNESS,
    COMMAND,
    MAX_SECONDS.required(),
    MAX_STEPS.required(),
    MAX_CONCURRENT.required(),
    MAX_TOKENS,
];
const SET: &[Field] = &[
    HARNESS,
    COMMAND,
    MAX_SECONDS,
    MAX_STEPS,
    MAX_CONCURRENT,
    MAX_TOKENS,
];
const ADD_MODEL: &[Field] = &[
    OWN,
    MODEL,
    Field::new(
        "tier",
        Kind::Choice(Tier::NAMES),
        "How strong it is, for choosing one that is enough; Ensemble's catalog says when left out.",
    ),
];
const ONE_MODEL: &[Field] = &[OWN, MODEL];

#[derive(Deserialize, Serialize)]
struct One {
    harness: String,
}

#[derive(Deserialize, Serialize)]
struct Limits {
    harness: String,
    command: Option<String>,
    max_seconds: Option<u64>,
    max_steps: Option<u32>,
    max_concurrent: Option<u32>,
    max_tokens: Option<u32>,
}

#[derive(Deserialize, Serialize)]
struct AddModel {
    harness: String,
    model: String,
    tier: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct OneModel {
    harness: String,
    model: String,
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
                "List the harnesses added: each found or missing, logged in where its CLI says, how long and how many steps a run may take, how many run at once, and what it drives; and the CLIs installed that are not added.",
                &[],
            ),
            |c: &RunnerContext, _: &Caller, _: Nothing| list(c),
        ),
        changing(
            spec(
                "harness.add",
                "Add a harness, so tasks may run on it: a CLI must be found. Claude Code and Codex then need a model of their vendor's added to them.",
                ADD,
            ),
            |c, input: Limits, change| add(c, input, change),
        ),
        changing(
            spec(
                "harness.remove",
                "Take a harness out, with the models added to it: no task runs on it.",
                ONE,
            ),
            |c, input: One, change| remove(c, &input.harness, change),
        ),
        changing(
            spec(
                "harness.set",
                "Change a harness's command, how long and how many steps a run may take, or how many run at once.",
                SET,
            ),
            |c, input: Limits, change| set(c, input, change),
        ),
        changing(
            spec(
                "harness.add-model",
                "Add a model of its own vendor's to Claude Code or Codex, as the CLI takes it, such as sonnet.",
                ADD_MODEL,
            ),
            |c, input: AddModel, change| add_model(c, input, change),
        ),
        changing(
            spec(
                "harness.remove-model",
                "Take a model out of Claude Code or Codex.",
                ONE_MODEL,
            ),
            |c, input: OneModel, change| remove_model(c, input, change),
        ),
        changing(
            spec(
                "harness.refresh",
                "Look again for the CLI of each harness added.",
                &[],
            ),
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

/// The harness of `kind` added to the runner, or why there is none.
fn added(c: &RunnerContext, kind: HarnessKind) -> Result<Harness, Error> {
    c.settings.read()?.harnesses.get(kind).ok_or_else(|| {
        Error::not_found(format!("{} is not added", kind.as_str()))
            .on("harness")
            .hint(format!("`harness add {}` adds it", kind.as_str()))
    })
}

/// What a harness of `kind` drives, in a few words.
fn drives(h: &Harness) -> String {
    match h.kind {
        HarnessKind::ClaudeCode | HarnessKind::Codex if h.models.is_empty() => format!(
            "no model yet: `harness add-model {} <model>` adds one",
            h.kind.as_str()
        ),
        HarnessKind::ClaudeCode | HarnessKind::Codex => format!(
            "its own models: {}",
            h.models
                .iter()
                .map(|m| format!("{} ({})", m.id, m.tier.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        HarnessKind::OpenCode => "LLMs of the ANN pool's OpenAI-compatible providers".into(),
        HarnessKind::Builtin => "LLMs of the ANN pool that make tool calls".into(),
    }
}

/// One harness as `harness.list` says it.
fn listed(h: &Harness) -> Value {
    let models: Vec<Value> = h
        .models
        .iter()
        .map(|m| json!({ "id": m.id, "tier": m.tier.as_str() }))
        .collect();
    let suggested: Vec<Value> = ensemble_backends::catalog::harness_models(h.kind)
        .iter()
        .filter(|(id, _)| !h.models.iter().any(|m| m.id == *id))
        .map(|(id, tier)| json!({ "id": id, "tier": tier.as_str() }))
        .collect();
    json!({
        "harness": h.kind,
        "missing": h.missing,
        "command": h.command,
        "max_seconds": h.max_seconds,
        "max_steps": h.max_steps,
        "max_concurrent": h.max_concurrent,
        "max_tokens": h.max_tokens,
        "logged_in": setup::logged_in(h.kind),
        "models": models,
        "suggested": suggested,
        "drives": drives(h),
    })
}

/// One harness in a line: `claude-code  found  claude, logged in; 1800 s and
/// 50 steps a run; its own models: …`.
fn line(h: &Harness) -> String {
    let state = if h.missing { "missing" } else { "found" };
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
        drives(h)
    )
}

fn list(c: &RunnerContext) -> Result<Value, Error> {
    let settings = c.settings.read()?;
    let all = settings.harnesses.each();
    let not_added = setup::not_added(&settings.harnesses);
    let mut said: Vec<String> = all.iter().map(line).collect();
    if all.is_empty() {
        said.push("no harness yet; `ensemble-runner harness add <harness>` adds one".into());
    }
    if !not_added.is_empty() {
        said.push(format!(
            "installed, not added: {}",
            not_added
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(json!({
        "said": said.join("\n"),
        "harnesses": all.iter().map(listed).collect::<Vec<_>>(),
        "not_added": not_added,
    }))
}

/// Refuse what is not a limit: 0, or `max_tokens` for a harness other than
/// `builtin`; and a command for `builtin`.
fn check_limits(kind: HarnessKind, input: &Limits) -> Result<Option<String>, Error> {
    let command = input
        .command
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(String::from);
    if input.command.is_some() && command.is_none() {
        return Err(Error::invalid("the command is empty").on("command"));
    }
    if command.is_some() && kind == HarnessKind::Builtin {
        return Err(
            Error::invalid("builtin runs no command: it is Ensemble's own loop").on("command"),
        );
    }
    for (value, field) in [
        (input.max_seconds.map(|n| n as u128), "max_seconds"),
        (input.max_steps.map(u128::from), "max_steps"),
        (input.max_concurrent.map(u128::from), "max_concurrent"),
        (input.max_tokens.map(u128::from), "max_tokens"),
    ] {
        if value == Some(0) {
            return Err(Error::invalid(format!("`{field}` must be greater than 0")).on(field));
        }
    }
    if input.max_tokens.is_some() && kind != HarnessKind::Builtin {
        return Err(
            Error::invalid("only builtin takes `max_tokens`: a CLI keeps its own").on("max_tokens"),
        );
    }
    Ok(command)
}

fn add(
    c: &RunnerContext,
    input: Limits,
    change: ensemble_config::kept::Change,
) -> Result<Value, Error> {
    let kind = kind_of(&input.harness)?;
    let name = kind.as_str();
    if c.settings.read()?.harnesses.get(kind).is_some() {
        return Ok(json!({ "said": format!("{name} is added already") }));
    }
    let command = check_limits(kind, &input)?;
    let required = |value: Option<u64>, field: &'static str| {
        value.ok_or_else(|| Error::invalid(format!("`{field}` is required")).on(field))
    };
    let max_seconds = required(input.max_seconds, "max_seconds")?;
    let max_steps = required(input.max_steps.map(u64::from), "max_steps")? as u32;
    let max_concurrent = required(input.max_concurrent.map(u64::from), "max_concurrent")? as u32;
    let said = match kind {
        HarnessKind::Builtin => {
            let max_tokens = input.max_tokens.ok_or_else(|| {
                Error::invalid("`max_tokens` is required: the most tokens of answer per model call")
                    .on("max_tokens")
            })?;
            let builtin = BuiltinHarness {
                max_seconds,
                max_steps,
                max_concurrent,
                max_tokens,
            };
            return edit(
                c,
                change,
                "builtin is added: it drives LLMs of the ANN pool that make tool calls".into(),
                |s| {
                    s.harnesses.builtin = Some(builtin.clone());
                    Ok(())
                },
            );
        }
        HarnessKind::ClaudeCode | HarnessKind::Codex => format!(
            "{name} is added; add a model of its vendor's with `harness add-model {name} <model>`"
        ),
        HarnessKind::OpenCode => {
            format!("{name} is added: it drives LLMs of the ANN pool's OpenAI-compatible providers")
        }
    };
    let command = command.ok_or_else(|| {
        Error::invalid(format!(
            "`command` is required: the {name} CLI, by its name on PATH or its path"
        ))
        .on("command")
    })?;
    if !setup::found(&command) {
        return Err(Error::new(
            Code::NotFound,
            format!("`{command}` is not found on this computer"),
        )
        .on("command")
        .hint(format!(
            "install it, or name its path with `harness add {name} --command <path>`"
        )));
    }
    let harness = CliHarness {
        missing: false,
        command,
        max_seconds,
        max_steps,
        max_concurrent,
        models: Vec::new(),
    };
    edit(c, change, said, |s| {
        if let Some(slot) = s.harnesses.cli_slot(kind) {
            *slot = Some(harness.clone());
        }
        Ok(())
    })
}

fn remove(
    c: &RunnerContext,
    name: &str,
    change: ensemble_config::kept::Change,
) -> Result<Value, Error> {
    let kind = kind_of(name)?;
    added(c, kind)?;
    edit(
        c,
        change,
        format!("{name} is out: no task runs on it"),
        |s| {
            match s.harnesses.cli_slot(kind) {
                Some(slot) => *slot = None,
                None => s.harnesses.builtin = None,
            }
            Ok(())
        },
    )
}

fn set(
    c: &RunnerContext,
    input: Limits,
    change: ensemble_config::kept::Change,
) -> Result<Value, Error> {
    let kind = kind_of(&input.harness)?;
    added(c, kind)?;
    let command = check_limits(kind, &input)?;
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
        if let (Some(command), Some(h)) = (&command, s.harnesses.cli_mut(kind)) {
            h.command = command.clone();
            h.missing = !setup::found(command);
        }
        s.harnesses.change(
            kind,
            HarnessChange {
                max_seconds: input.max_seconds,
                max_steps: input.max_steps,
                max_concurrent: input.max_concurrent,
                max_tokens: input.max_tokens,
            },
        );
        Ok(())
    })
}

/// The harness `name` that drives models of its own vendor's, added.
fn own(c: &RunnerContext, name: &str) -> Result<HarnessKind, Error> {
    let kind = kind_of(name)?;
    if !kind.has_own_models() {
        return Err(Error::invalid(format!(
            "{name} drives LLMs of the ANN pool: add one with `model add <provider>:<model>`"
        ))
        .on("harness"));
    }
    added(c, kind)?;
    Ok(kind)
}

fn add_model(
    c: &RunnerContext,
    input: AddModel,
    change: ensemble_config::kept::Change,
) -> Result<Value, Error> {
    let kind = own(c, &input.harness)?;
    let id = input.model.trim().to_string();
    if id.is_empty() {
        return Err(Error::invalid("the model is empty").on("model"));
    }
    let given = input
        .tier
        .as_deref()
        .map(|t| {
            Tier::parse(t).ok_or_else(|| {
                Error::invalid(format!("'{t}' is not a tier: {}", Tier::NAMES.join(", ")))
                    .on("tier")
            })
        })
        .transpose()?;
    let name = kind.as_str();
    if added(c, kind)?.models.iter().any(|m| m.id == id) {
        return Ok(json!({ "said": format!("{name} drives {id} already") }));
    }
    let tier = given
        .or_else(|| {
            ensemble_backends::catalog::harness_models(kind)
                .into_iter()
                .find(|(known, _)| *known == id)
                .map(|(_, tier)| tier)
        })
        .or_else(|| ensemble_backends::catalog::tier(&id))
        .unwrap_or(Tier::Medium);
    let said = format!("{name} drives {id} ({})", tier.as_str());
    edit(c, change, said, |s| {
        if let Some(h) = s.harnesses.cli_mut(kind) {
            h.models.push(HarnessModel {
                id: id.clone(),
                tier,
            });
            h.models.sort_by(|a, b| a.id.cmp(&b.id));
        }
        Ok(())
    })
}

fn remove_model(
    c: &RunnerContext,
    input: OneModel,
    change: ensemble_config::kept::Change,
) -> Result<Value, Error> {
    let kind = own(c, &input.harness)?;
    let id = input.model.trim().to_string();
    let name = kind.as_str();
    if !added(c, kind)?.models.iter().any(|m| m.id == id) {
        return Err(Error::not_found(format!("{name} drives no model {id}")).on("model"));
    }
    edit(c, change, format!("{name} no longer drives {id}"), |s| {
        if let Some(h) = s.harnesses.cli_mut(kind) {
            h.models.retain(|m| m.id != id);
        }
        Ok(())
    })
}

fn refresh(c: &RunnerContext, change: ensemble_config::kept::Change) -> Result<Value, Error> {
    let mut harnesses = c.settings.read()?.harnesses;
    let mut said = setup::look_for(&mut harnesses);
    let not_added = setup::not_added(&harnesses);
    let installed = (!not_added.is_empty()).then(|| {
        format!(
            "installed, not added: {}",
            not_added
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    });
    if said.is_empty() {
        said.push("every harness added is as it was".into());
        said.extend(installed);
        return Ok(json!({ "said": said.join("; ") }));
    }
    said.extend(installed);
    edit(c, change, said.join("; "), |s| {
        s.harnesses = harnesses.clone();
        Ok(())
    })
}

/// Look for the CLIs of the harnesses added as the runner starts, as this
/// computer, before it reads its settings: one gone missing runs no task.
pub fn refresh_at_start(c: &RunnerContext) -> Result<String, Error> {
    let out = refresh(
        c,
        ensemble_config::kept::Change::by_this_computer("harness.refresh"),
    )?;
    Ok(out["said"].as_str().unwrap_or_default().to_string())
}
