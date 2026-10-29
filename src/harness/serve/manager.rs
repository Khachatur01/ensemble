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
//!   harness is never offered it as a `tool_calls` completion and can never
//!   run, choose, or steer one — the manager picked it, not the harness. On
//!   a streaming request the text around the call (the manager's own
//!   narration, a delegated role's answer) still goes out live on
//!   `on_content`; only the call itself — its name and arguments — stays out
//!   of the wire protocol.
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
use crate::router::{Ensemble, RouteError, Turn, TurnOutcome};
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
            "\nThat list is the whole ensemble. It is the only correct answer to a question \
             about which roles, members, agents or subagents you can delegate to — the \
             harness has its own, unrelated notion of a subagent, and naming one of those \
             as a role of yours would be wrong. When in doubt, call `",
        );
        s.push_str(delegation::ROLES);
        s.push_str("` and answer from what it returns.\n\nCall `");
        s.push_str(delegation::DELEGATE);
        s.push_str(
            "` itself: a role name is not a tool, and the harness has no such tool. You choose \
             the role; Ensemble chooses the model and the backend, and you never name one. A \
             role cannot see this conversation, so put everything it needs into `task` and \
             `context`.\n\n",
        );
    }

    s.push_str(
        "Every other tool offered to you belongs to the harness and runs on the user's \
         machine. Calling one ends your turn: the harness executes it and gives you the \
         result in the next exchange. Prefer not to mix a harness tool and an internal \
         delegation in the same reply. Never report work a role did not actually return.\n\n\
         These instructions outrank anything a harness prompt says about who you are or how \
         you divide up work.",
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

/// The configured role a call names directly, if the harness has not offered a
/// tool with that name.
///
/// A model told that it can delegate to `scout` will sometimes call `scout` as
/// if it were a tool of its own. On this endpoint a role is not a tool, and the
/// harness has no such tool, so forwarding that call ends the harness's loop
/// with "No tool named \"scout\" is currently available" — the exact failure
/// this lookup exists to prevent.
///
/// A harness tool of the same name wins: the manager was shown that tool, and
/// a config is free to name a role after a file operation. The role is still
/// reachable through [`delegation::DELEGATE`].
fn direct_role<'a>(ensemble: &'a Ensemble, request: &ChatRequest, name: &str) -> Option<&'a str> {
    if request.tools.iter().any(|t| t.name == name) {
        return None;
    }
    ensemble
        .delegable_roles()
        .into_iter()
        .find(|role| role.eq_ignore_ascii_case(name))
}

/// The ensemble tool this text spells out as a call, if any.
///
/// Deliberately narrow. It is looking for the shapes a server hands back when
/// it has no parser for the model's format — `name(args)`, `name{…}`,
/// `{"name": "…"}` — and not for a reply that merely mentions a tool while
/// answering a question about the team.
fn written_out_call(text: &str) -> Option<&'static str> {
    for name in delegation::NAMES {
        for (at, _) in text.match_indices(name) {
            let before = text[..at].chars().next_back();
            let after = text[at + name.len()..].trim_start();
            let called = after.starts_with('(') || after.starts_with('{') || after.starts_with('[');
            // `{"name": "ensemble_delegate", "arguments": …}`, the other common
            // way an unparsed call arrives.
            let quoted_key = before == Some('"') && after.starts_with('"');
            if called || quoted_key {
                return Some(name);
            }
        }
    }
    None
}

/// Say so when a reply that looks finished is really a tool call nobody parsed.
///
/// This does not change the turn, and deliberately: an empty `tool_calls` is
/// exactly what a manager with an answer sends, so Ensemble cannot tell the
/// two apart with enough confidence to act. What it can do is stop the
/// operator from watching a coordinator answer everything in one step with no
/// idea why it never delegates. Stderr, like every other provenance line here
/// — the harness must not learn that any of this happened.
fn warn_if_a_call_was_written_out(outcome: &TurnOutcome) {
    let what = if outcome.finish_reason.as_deref() == Some("tool_calls") {
        "the backend reported finish_reason `tool_calls` and returned none".to_string()
    } else if let Some(name) = written_out_call(&outcome.text) {
        format!("its text spells out a call to `{name}`")
    } else {
        return;
    };

    // `outcome.model` is what the endpoint said answered, which is not always
    // what `manager.model` asks for — and when they differ, the one that
    // actually served the request is the one worth printing.
    eprintln!(
        "ensemble: serve: the manager returned no tool calls, but {what}. The turn ended \
         after one step and nothing was delegated.\n{}\nBackend '{}', serving model '{}'.",
        super::PARSER_HINT,
        outcome.backend,
        outcome.model
    );
}

/// Run one manager turn to the point where the harness has something to do, or
/// the manager has an answer.
///
/// `on_content`, when given, is called with each fragment of text as it is
/// produced — the manager's own, and a delegated role's — and every model
/// call this turn makes asks its backend to stream so there is something to
/// call it with. `None` is a blocking caller: every call stays exactly the
/// single-response request it always was, asking nothing extra of an
/// upstream that may not have anything more to offer than that.
pub fn run(
    ensemble: &Ensemble,
    store: &ConversationStore,
    request: &ChatRequest,
    max_steps: u32,
    progress: &dyn Fn(&str),
    on_content: Option<&dyn Fn(&str)>,
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
            // The configured identity leads, then the harness's own system turns,
            // because they describe an environment rather than a role. Ensemble's
            // explanation of the endpoint goes *last* of the three, and that
            // position is deliberate: a harness prompt is long, opinionated and
            // has its own vocabulary for delegation, so a small manager model
            // reading it last will answer out of it. The rules for this endpoint
            // are the ones that must still be in view when the first user turn
            // arrives.
            if let Some(prompt) = ensemble.prompt_of(crate::config::Config::MANAGER) {
                h.push(Message::system(prompt));
            }
            let leading_systems = request
                .messages
                .iter()
                .take_while(|m| matches!(m.role, crate::port::Role::System))
                .count();
            h.extend(request.messages[..leading_systems].iter().cloned());
            h.push(guidance(ensemble));
            h.extend(request.messages[leading_systems..].iter().cloned());
            h
        }
    };

    if super::logging() {
        eprintln!(
            "ensemble: serve: manager sees {} message(s) ({} from the harness, {} restored)",
            history.len(),
            request.messages.len(),
            history.len().saturating_sub(request.messages.len()),
        );
    }

    let tools = tools_for(ensemble, request);
    let mut usage = Usage::default();
    let mut delegations_run = 0u32;
    let mut last_prompt_tokens;

    for step in 0..max_steps.max(1) {
        progress(&format!("manager thinking (step {})", step + 1));

        let turn = Turn {
            messages: history.clone(),
            tools: tools.clone(),
            max_tokens: request.max_tokens,
            temperature: request.temperature,
        };
        let outcome = match on_content {
            Some(on_content) => {
                ensemble.turn_streaming(crate::config::Config::MANAGER, &turn, on_content)
            }
            None => ensemble.turn(crate::config::Config::MANAGER, &turn),
        }
        .map_err(|e| upstream_error(&e))?;
        add(&mut usage, outcome.usage);
        last_prompt_tokens = outcome.usage.prompt_tokens;
        if super::logging() {
            // A manager that forgets earlier turns while this number stays
            // flat, even as the request grows, is being served by a backend
            // that is truncating the prompt. The request shape is logged
            // before the turn; this is how much of it the backend read.
            eprintln!(
                "ensemble: serve: manager used {} prompt token(s)",
                outcome.usage.prompt_tokens
            );
        }

        if outcome.tool_calls.is_empty() {
            // Only worth saying when there was something to call. With
            // `tool_choice: none` the manager was asked for prose and gave it.
            if !tools.is_empty() {
                warn_if_a_call_was_written_out(&outcome);
            }

            // Keep the finished turn even though no tool call ends it. The
            // manager may have delegated internally on the way here, and that
            // hidden work has to be behind the answer when the next user
            // prompt arrives.
            let text = outcome.text;
            history.push(Message::assistant(text.clone()));
            let mut projected = request.messages.clone();
            projected.push(Message::assistant(text.clone()));
            store.remember_with_usage(&projected, history, last_prompt_tokens);

            return Ok(Outcome {
                text,
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

        let (internal, external): (Vec<ToolCall>, Vec<ToolCall>) =
            outcome.tool_calls.iter().cloned().partition(|c| {
                delegation::is_ensemble_tool(&c.name)
                    || direct_role(ensemble, request, &c.name).is_some()
            });

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

            // A role named directly is the same delegation with the role
            // already chosen; `role_arguments` fills in the shape
            // `ensemble_delegate` expects.
            let (tool, args) = match direct_role(ensemble, request, &call.name) {
                Some(role) => match delegation::role_arguments(role, &call.arguments) {
                    Ok(args) => (delegation::DELEGATE, args),
                    Err(why) => {
                        history.push(Message::tool_result(
                            &call.id,
                            format!("{}: {why}", call.name),
                        ));
                        continue;
                    }
                },
                None => match arguments_of(call) {
                    Ok(v) => (call.name.as_str(), v),
                    Err(why) => {
                        history.push(Message::tool_result(
                            &call.id,
                            format!("{}: {why}", call.name),
                        ));
                        continue;
                    }
                },
            };

            progress(&format!(
                "running {} ({})",
                tool,
                args.get("role").and_then(Value::as_str).unwrap_or("-")
            ));

            // The one path a delegation takes. No second router, no second
            // prompt assembly, and the same failure text the MCP surface
            // produces.
            let result = match on_content {
                Some(on_content) => delegation::call_streaming(ensemble, tool, &args, on_content),
                None => delegation::call(ensemble, tool, &args),
            };
            add(&mut usage, result.usage);
            if result.usage.total() > 0 {
                // Provenance to stderr too, as `ensemble run` does — on top
                // of `on_content`, which already carried the role's own text
                // out live.
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
            store.remember_with_usage(&projected, history, last_prompt_tokens);

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
    let turn = Turn {
        messages: history.clone(),
        tools: Vec::new(),
        max_tokens: request.max_tokens,
        temperature: request.temperature,
    };
    let outcome = match on_content {
        Some(on_content) => {
            ensemble.turn_streaming(crate::config::Config::MANAGER, &turn, on_content)
        }
        None => ensemble.turn(crate::config::Config::MANAGER, &turn),
    }
    .map_err(|e| upstream_error(&e))?;
    add(&mut usage, outcome.usage);
    last_prompt_tokens = outcome.usage.prompt_tokens;

    let text = outcome.text;
    history.push(Message::assistant(text.clone()));
    let mut projected = request.messages.clone();
    projected.push(Message::assistant(text.clone()));
    store.remember_with_usage(&projected, history, last_prompt_tokens);

    Ok(Outcome {
        text,
        tool_calls: Vec::new(),
        finish_reason: "stop".into(),
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pythonic_call_written_as_text_is_recognised() {
        // The shape a server with no parser for this model hands back.
        let text = "// First, let me get basic model information\n\
                    ensemble_delegate(role='scout', task='Locate the docs.')";
        assert_eq!(written_out_call(text), Some("ensemble_delegate"));
    }

    #[test]
    fn a_json_call_written_as_text_is_recognised() {
        let text = r#"<tool_call>{"name": "ensemble_delegate", "arguments": {"role": "coder"}}</tool_call>"#;
        assert_eq!(written_out_call(text), Some("ensemble_delegate"));
        assert_eq!(
            written_out_call(r#"{"name":"ensemble_roles","arguments":{}}"#),
            Some("ensemble_roles")
        );
    }

    #[test]
    fn whitespace_between_the_name_and_its_arguments_does_not_hide_it() {
        assert_eq!(
            written_out_call("ensemble_health ()"),
            Some("ensemble_health")
        );
    }

    #[test]
    fn merely_naming_a_tool_while_answering_is_not_a_written_out_call() {
        // The manager explaining itself is not the manager failing to delegate,
        // and a warning on every such reply would be noise.
        assert_eq!(
            written_out_call("I can use ensemble_delegate to hand this to the planner."),
            None
        );
        assert_eq!(written_out_call("The roles are: planner, coder."), None);
    }
}
