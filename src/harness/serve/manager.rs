//! The manager turn: one HTTP request, as many internal steps as it takes.
//!
//! WHAT MAKES THIS DIFFERENT FROM `Ensemble::delegate`
//! ---------------------------------------------------
//! `delegate` is one question and one answer. This is a loop, because the
//! manager coordinates: it asks a role for work, reads the result, and decides
//! what to do next — all inside the single request the harness is waiting on.
//!
//! TWO KINDS OF TOOL, AND THE LINE BETWEEN THEM
//! --------------------------------------------
//! The manager is offered the ensemble's own tools and the harness's. The
//! difference is who runs them:
//!
//! * An **ensemble tool** runs here, immediately, and the loop continues. The
//!   harness never learns it happened. That is the point: the harness selected
//!   one model and should not have to know, or be able to steer, how the work
//!   is divided behind it.
//! * A **harness tool** ends the turn. It comes back as an ordinary
//!   `tool_calls` completion, the harness runs it, and the result arrives on
//!   the next request.
//!
//! When one reply contains both, the internal calls run now and only the
//! external ones are forwarded. The harness must answer exactly the ids it was
//! handed, so it is given a message containing exactly those — while the
//! manager's own copy keeps all of them, and `state.rs` carries that copy
//! across the round trip.

use crate::harness::delegation;
use crate::port::{Message, ToolCall, ToolDef, Usage};
use crate::router::{Ensemble, RouteError, Turn};
use serde_json::Value;

use super::state::ConversationStore;
use super::wire::{ApiError, ChatRequest, ToolPolicy};

/// How many times the manager may be asked in one HTTP request.
pub const DEFAULT_MAX_STEPS: u32 = 8;

/// How many delegations one HTTP request may run. A separate budget from the
/// steps because one reply can ask for several at once.
pub const MAX_DELEGATIONS: u32 = 16;

pub struct Outcome {
    pub text: String,
    /// Harness tools only. Delegations have already run.
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: String,
    /// Every model call this request made, the invisible ones included. The
    /// caller paid for a delegation whether or not it saw it happen.
    pub usage: Usage,
}

/// The system turn that tells the manager what it is and how delegation works.
///
/// Written here rather than in `ensemble.yaml` because it describes a fact
/// about this endpoint, not a preference about this team. The role list comes
/// from the config; no model or backend is ever named, for the same reason the
/// tools do not take one.
fn guidance(ensemble: &Ensemble) -> Message {
    let roles = ensemble.delegable_roles();
    let mut s = String::new();
    s.push_str(
        "You are the manager of an ensemble of models, reached through a chat API by a \
         coding harness.\n\n",
    );

    if roles.is_empty() {
        s.push_str(
            "No other roles are configured, so there is nobody to delegate to: do the work \
             yourself.\n\n",
        );
    } else {
        s.push_str(&format!(
            "You have an internal tool, `{}`. Ensemble runs it here, inside this reply, and \
             hands you the result before you answer. The harness never sees those calls and \
             must never be told to make one. Available roles:\n",
            delegation::DELEGATE
        ));
        for role in &roles {
            let purpose = ensemble
                .prompt_of(role)
                .map(delegation::first_line)
                .unwrap_or_else(|| "(no description configured)".to_string());
            s.push_str(&format!("  - {role}: {purpose}\n"));
        }
        s.push_str(
            "\nYou choose the role; Ensemble chooses the model and the backend, and you never \
             name one. A role cannot see this conversation, so put everything it needs into \
             `task` and `context`.\n\n",
        );
    }

    s.push_str(
        "Every other tool offered to you belongs to the harness and runs on the user's \
         machine. Calling one ends your turn: the harness executes it and gives you the \
         result in the next exchange. Prefer not to mix a harness tool and an internal \
         delegation in the same reply. Never report work a role did not actually return.",
    );

    Message::system(s)
}

/// The tools the manager is offered, ensemble-owned first.
fn tools_for(ensemble: &Ensemble, request: &ChatRequest) -> Vec<ToolDef> {
    if request.tool_policy == ToolPolicy::None {
        // The caller asked for a plain answer. That includes not spending a
        // delegation it cannot see and did not ask for.
        return Vec::new();
    }
    let mut tools: Vec<ToolDef> = delegation::tools(&ensemble.delegable_roles())
        .into_iter()
        .map(|t| ToolDef {
            name: t.name.to_string(),
            description: t.description,
            parameters: t.parameters,
        })
        .collect();
    tools.extend(request.tools.iter().cloned());
    tools
}

/// Map a routing failure onto the status an OpenAI client expects.
fn upstream_error(e: &RouteError) -> ApiError {
    use crate::port::Error;
    match e {
        RouteError::UnknownRole { .. } => ApiError::upstream(
            500,
            "configuration_error",
            format!("the server is misconfigured: {e}"),
        ),
        RouteError::Backend(Error::Timeout { .. }) => {
            ApiError::upstream(504, "upstream_timeout", e.to_string())
        }
        RouteError::Backend(Error::Api { status: 429, .. }) => {
            ApiError::upstream(429, "upstream_rate_limited", e.to_string())
        }
        // The backend's own Display already names the backend and says what to
        // check; repeating that here in other words would only make the two
        // drift.
        RouteError::Backend(_) => ApiError::upstream(502, "upstream_error", e.to_string()),
    }
}

fn add(total: &mut Usage, one: Usage) {
    total.prompt_tokens = total.prompt_tokens.saturating_add(one.prompt_tokens);
    total.completion_tokens = total
        .completion_tokens
        .saturating_add(one.completion_tokens);
}

/// Arguments arrive as the model wrote them. A model that writes something
/// that is not an object gets told so, as a tool result, and can try again.
fn arguments_of(call: &ToolCall) -> Result<Value, String> {
    match serde_json::from_str::<Value>(&call.arguments) {
        Ok(v) if v.is_object() => Ok(v),
        Ok(other) => Err(format!(
            "the arguments must be a JSON object; got {other}. Call it again with an object."
        )),
        Err(e) => Err(format!(
            "the arguments were not valid JSON ({e}). Call it again with a JSON object."
        )),
    }
}

/// Run one manager turn to the point where the harness has something to do, or
/// the manager has an answer.
pub fn run(
    ensemble: &Ensemble,
    store: &ConversationStore,
    request: &ChatRequest,
    max_steps: u32,
    progress: &dyn Fn(&str),
) -> Result<Outcome, ApiError> {
    // The conversation as the manager should see it: either what we kept from
    // the previous round trip plus what the harness has added since, or — on a
    // miss, or on the first request — a fresh preamble in front of the
    // harness's own messages.
    let mut history = match store.restore(&request.messages) {
        Some((mut kept, trailing)) => {
            kept.extend(trailing);
            kept
        }
        None => {
            let mut h = Vec::new();
            // The configured identity leads; Ensemble's own explanation of the
            // endpoint follows; the harness's system prompt comes after both,
            // because it describes an environment, not a role.
            if let Some(prompt) = ensemble.prompt_of(crate::config::Config::MANAGER) {
                h.push(Message::system(prompt));
            }
            h.push(guidance(ensemble));
            h.extend(request.messages.iter().cloned());
            h
        }
    };

    let tools = tools_for(ensemble, request);
    let mut usage = Usage::default();
    let mut delegations_run = 0u32;

    for step in 0..max_steps.max(1) {
        progress(&format!("manager thinking (step {})", step + 1));

        let outcome = ensemble
            .turn(
                crate::config::Config::MANAGER,
                &Turn {
                    messages: history.clone(),
                    tools: tools.clone(),
                    max_tokens: request.max_tokens,
                    temperature: request.temperature,
                },
            )
            .map_err(|e| upstream_error(&e))?;
        add(&mut usage, outcome.usage);

        if outcome.tool_calls.is_empty() {
            return Ok(Outcome {
                text: outcome.text,
                tool_calls: Vec::new(),
                finish_reason: outcome
                    .finish_reason
                    .filter(|r| r != "tool_calls")
                    .unwrap_or_else(|| "stop".into()),
                usage,
            });
        }

        // The manager's copy keeps every call it made.
        history.push(Message::assistant_calling(
            outcome.text.clone(),
            outcome.tool_calls.clone(),
        ));

        let (internal, external): (Vec<ToolCall>, Vec<ToolCall>) = outcome
            .tool_calls
            .iter()
            .cloned()
            .partition(|c| delegation::is_ensemble_tool(&c.name));

        for call in &internal {
            if delegations_run >= MAX_DELEGATIONS {
                history.push(Message::tool_result(
                    &call.id,
                    format!(
                        "not run: this reply has already used its budget of \
                         {MAX_DELEGATIONS} delegations. Answer with what you have."
                    ),
                ));
                continue;
            }
            delegations_run += 1;

            let args = match arguments_of(call) {
                Ok(v) => v,
                Err(why) => {
                    history.push(Message::tool_result(
                        &call.id,
                        format!("{}: {why}", call.name),
                    ));
                    continue;
                }
            };

            progress(&format!(
                "running {} ({})",
                call.name,
                args.get("role").and_then(Value::as_str).unwrap_or("-")
            ));

            // The one path a delegation takes. No second router, no second
            // prompt assembly, and the same failure text the MCP surface
            // produces.
            let result = delegation::call(ensemble, &call.name, &args);
            add(&mut usage, result.usage);
            if result.usage.total() > 0 {
                // Provenance to stderr, as `ensemble run` does. Never into the
                // response: the harness must not learn the shape of the team.
                eprintln!(
                    "ensemble: serve: {} ({} in, {} out){}",
                    call.name,
                    result.usage.prompt_tokens,
                    result.usage.completion_tokens,
                    if result.is_error { " [failed]" } else { "" }
                );
            }
            history.push(Message::tool_result(&call.id, result.text));
        }

        if !external.is_empty() {
            // What the harness will hold after this response: its own messages
            // plus an assistant turn carrying only the calls it must answer.
            let mut projected = request.messages.clone();
            projected.push(Message::assistant_calling(
                outcome.text.clone(),
                external.clone(),
            ));
            store.remember(&projected, history);

            return Ok(Outcome {
                text: outcome.text,
                tool_calls: external,
                finish_reason: "tool_calls".into(),
                usage,
            });
        }
    }

    // The step budget is spent. One last call with no tools at all, so the
    // client gets an answer rather than a loop that silently stopped.
    progress("step budget reached; asking for a final answer");
    history.push(Message::user(
        "You have used this reply's budget for internal steps. Answer now, with what you \
         have, and say plainly what is still unresolved.",
    ));
    let outcome = ensemble
        .turn(
            crate::config::Config::MANAGER,
            &Turn {
                messages: history,
                tools: Vec::new(),
                max_tokens: request.max_tokens,
                temperature: request.temperature,
            },
        )
        .map_err(|e| upstream_error(&e))?;
    add(&mut usage, outcome.usage);

    Ok(Outcome {
        text: outcome.text,
        tool_calls: Vec::new(),
        finish_reason: "stop".into(),
        usage,
    })
}
