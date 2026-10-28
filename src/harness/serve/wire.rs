//! The OpenAI chat wire format: parse in, render out, and say no clearly.
//!
//! This module is where a harness's JSON becomes `port::Message` and back
//! again. It holds no state, touches no socket and knows nothing about
//! backends — which is what makes every rule in it testable by calling one
//! function with one `Value`.
//!
//! Two things it is strict about, both because the alternative is a silent
//! wrong answer rather than a loud wrong request:
//!
//! * **The model id.** Exactly one model is served. A request for another one
//!   is a 404, never a quiet substitution — a user who typed the wrong id and
//!   got an answer anyway has no way to find out.
//! * **Tool-call ids and argument bytes.** They pass through untouched in both
//!   directions. The harness pairs its results to its calls on the id, and a
//!   model that writes malformed arguments has to be reported as having done
//!   so rather than quietly corrected.

use crate::harness::delegation;
use crate::port::{Message, Role, ToolCall, ToolDef, Usage};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The one model this server serves.
pub const MODEL_ID: &str = "ensemble/manager";

/// What the model id has to normalise to.
///
/// A provider prefix is the harness's, not ours: OpenCode names the provider
/// in its config and then sends either `manager` or `ensemble/manager`
/// depending on version and code path. Both mean the same model. Anything
/// whose last segment is not `manager` — `ensemble/coder`, say — is a
/// different request and is refused.
const MODEL_LEAF: &str = "manager";

pub fn model_matches(asked: &str) -> bool {
    asked.rsplit('/').next().unwrap_or(asked).trim() == MODEL_LEAF
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A refusal, in the shape an OpenAI client parses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    pub kind: &'static str,
    pub param: Option<String>,
    pub code: &'static str,
}

impl ApiError {
    pub fn invalid(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            message: message.into(),
            kind: "invalid_request_error",
            param: None,
            code,
        }
    }

    pub fn upstream(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            message: message.into(),
            kind: "api_error",
            param: None,
            code,
        }
    }

    pub fn at(mut self, param: impl Into<String>) -> Self {
        self.param = Some(param.into());
        self
    }

    pub fn body(&self) -> String {
        json!({
            "error": {
                "message": self.message,
                "type": self.kind,
                "param": self.param,
                "code": self.code,
            }
        })
        .to_string()
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// What the manager should be offered, after `tool_choice` has had its say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPolicy {
    /// Offer the harness's tools and Ensemble's own.
    Auto,
    /// Offer nothing. The caller asked for a plain answer, and that includes
    /// not spending a delegation it did not ask for.
    None,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// The id as the client wrote it, echoed back unchanged so a client that
    /// compares them is not surprised.
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub tool_policy: ToolPolicy,
    pub stream: bool,
    pub include_usage: bool,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

pub fn parse_body(raw: &[u8]) -> Result<Value, ApiError> {
    serde_json::from_slice(raw).map_err(|e| {
        ApiError::invalid(
            400,
            "invalid_json",
            format!("the request body is not JSON: {e}"),
        )
    })
}

pub fn parse_request(v: &Value) -> Result<ChatRequest, ApiError> {
    let model = v
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ApiError::invalid(400, "missing_required_parameter", "`model` is required").at("model")
        })?
        .to_string();

    if !model_matches(&model) {
        return Err(ApiError::invalid(
            404,
            "model_not_found",
            format!(
                "no model '{model}'. This endpoint serves exactly one: '{MODEL_ID}'. \
                 Roles inside the ensemble are not addressable as models — the manager \
                 chooses who does the work."
            ),
        )
        .at("model"));
    }

    let messages = v.get("messages").and_then(Value::as_array).ok_or_else(|| {
        ApiError::invalid(
            400,
            "missing_required_parameter",
            "`messages` is required and must be an array",
        )
        .at("messages")
    })?;
    if messages.is_empty() {
        return Err(ApiError::invalid(
            400,
            "invalid_value",
            "`messages` is empty — there is nothing to answer",
        )
        .at("messages"));
    }

    let mut out = Vec::with_capacity(messages.len());
    for (i, m) in messages.iter().enumerate() {
        out.push(parse_message(m, i)?);
    }

    if let Some(n) = v.get("n").and_then(Value::as_u64)
        && n > 1
    {
        return Err(ApiError::invalid(
            400,
            "unsupported_value",
            "`n` above 1 is not supported: one manager turn produces one answer",
        )
        .at("n"));
    }

    let tool_policy = match v.get("tool_choice") {
        None | Some(Value::Null) => ToolPolicy::Auto,
        Some(Value::String(s)) if s == "auto" => ToolPolicy::Auto,
        Some(Value::String(s)) if s == "none" => ToolPolicy::None,
        Some(other) => {
            return Err(ApiError::invalid(
                400,
                "unsupported_value",
                format!(
                    "`tool_choice` must be \"auto\" or \"none\"; got {other}. Forcing a \
                     particular tool would also force it on the ensemble's own, which are \
                     not yours to choose."
                ),
            )
            .at("tool_choice"));
        }
    };

    let tools = parse_tools(v)?;

    let stream = v.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let include_usage = v
        .get("stream_options")
        .and_then(|o| o.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // `max_completion_tokens` is the current spelling; `max_tokens` is the one
    // most local servers and older clients still send.
    let max_tokens = v
        .get("max_completion_tokens")
        .or_else(|| v.get("max_tokens"))
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    let temperature = v
        .get("temperature")
        .and_then(Value::as_f64)
        .map(|t| t as f32);

    Ok(ChatRequest {
        model,
        messages: out,
        tools,
        tool_policy,
        stream,
        include_usage,
        max_tokens,
        temperature,
    })
}

fn parse_tools(v: &Value) -> Result<Vec<ToolDef>, ApiError> {
    let Some(tools) = v.get("tools").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(tools.len());
    for (i, t) in tools.iter().enumerate() {
        let f = t.get("function").unwrap_or(t);
        let Some(name) = f.get("name").and_then(Value::as_str) else {
            return Err(
                ApiError::invalid(400, "invalid_value", "a tool has no `function.name`")
                    .at(format!("tools[{i}].function.name")),
            );
        };

        // Refused outright, never shadowed or renamed. Shadowing means the
        // manager calls what it believes is the harness's tool and gets a
        // delegation; renaming ours makes the guidance it was given a lie.
        if delegation::is_ensemble_tool(name) {
            return Err(ApiError::invalid(
                400,
                "tool_name_conflict",
                format!(
                    "the harness offered a tool named '{name}', and Ensemble owns that name on \
                     this endpoint. This usually means the Ensemble MCP server and this HTTP \
                     API are both active in one session; disable the MCP server for it."
                ),
            )
            .at("tools"));
        }

        out.push(ToolDef {
            name: name.to_string(),
            description: f
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            // Carried verbatim. It is the harness's schema and re-modelling it
            // here could only lose detail.
            parameters: f
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"})),
        });
    }
    Ok(out)
}

fn parse_message(m: &Value, i: usize) -> Result<Message, ApiError> {
    let role = m.get("role").and_then(Value::as_str).unwrap_or("");
    let content = parse_content(m.get("content"));

    match role {
        // `developer` is OpenAI's rename of `system`; same turn.
        "system" | "developer" => Ok(Message::system(content)),
        "user" => Ok(Message::user(content)),
        "assistant" => {
            let calls = parse_tool_calls(m.get("tool_calls"), i)?;
            if calls.is_empty() {
                Ok(Message::assistant(content))
            } else {
                Ok(Message::assistant_calling(content, calls))
            }
        }
        "tool" | "function" => {
            let id = m
                .get("tool_call_id")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    ApiError::invalid(
                        400,
                        "missing_required_parameter",
                        "a tool result must say which call it answers",
                    )
                    .at(format!("messages[{i}].tool_call_id"))
                })?;
            Ok(Message::tool_result(id, content))
        }
        other => Err(ApiError::invalid(
            400,
            "invalid_value",
            format!(
                "unknown message role '{other}'. Expected system, developer, user, \
                 assistant or tool."
            ),
        )
        .at(format!("messages[{i}].role"))),
    }
}

/// Content is a string, or an array of parts, or absent.
///
/// A part this manager cannot read becomes a marker rather than nothing. An
/// image that silently vanishes leaves the model answering a question it was
/// never fully shown.
fn parse_content(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => p
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                Some(other) => format!("[{other} omitted: the Ensemble manager reads text only]"),
                None => p
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
    }
}

fn parse_tool_calls(v: Option<&Value>, i: usize) -> Result<Vec<ToolCall>, ApiError> {
    let Some(calls) = v.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(calls.len());
    for (j, c) in calls.iter().enumerate() {
        let f = c.get("function").unwrap_or(c);
        let Some(name) = f.get("name").and_then(Value::as_str) else {
            return Err(
                ApiError::invalid(400, "invalid_value", "a tool call has no name")
                    .at(format!("messages[{i}].tool_calls[{j}].function.name")),
            );
        };
        let arguments = match f.get("arguments") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => "{}".to_string(),
        };
        out.push(ToolCall {
            id: c
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: name.to_string(),
            arguments,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// A completion id. A counter mixed with the clock, rather than a uuid
/// dependency for a value nobody resolves.
pub fn completion_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    format!("chatcmpl-ens-{:08x}{:08x}", nanos, n)
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn tool_calls_json(calls: &[ToolCall]) -> Value {
    Value::Array(
        calls
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "type": "function",
                    "function": { "name": c.name, "arguments": c.arguments },
                })
            })
            .collect(),
    )
}

fn usage_json(usage: &Usage) -> Value {
    json!({
        "prompt_tokens": usage.prompt_tokens,
        "completion_tokens": usage.completion_tokens,
        "total_tokens": usage.total(),
    })
}

pub fn models_list() -> String {
    json!({
        "object": "list",
        "data": [{
            "id": MODEL_ID,
            "object": "model",
            "created": now_secs(),
            "owned_by": "ensemble",
        }]
    })
    .to_string()
}

pub struct Completion {
    pub id: String,
    pub created: u64,
    pub model: String,
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: String,
    pub usage: Usage,
}

impl Completion {
    pub fn body(&self) -> String {
        // `content` is null beside tool calls, as the spec has it; strict
        // clients reject the empty string there.
        let content = if self.text.is_empty() && !self.tool_calls.is_empty() {
            Value::Null
        } else {
            Value::String(self.text.clone())
        };
        let mut message = json!({ "role": "assistant", "content": content });
        if !self.tool_calls.is_empty() {
            message["tool_calls"] = tool_calls_json(&self.tool_calls);
        }
        json!({
            "id": self.id,
            "object": "chat.completion",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "message": message, "finish_reason": self.finish_reason }],
            "usage": usage_json(&self.usage),
        })
        .to_string()
    }

    fn chunk(&self, delta: Value, finish_reason: Value) -> String {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }],
        })
        .to_string()
    }

    pub fn chunk_role(&self) -> String {
        self.chunk(json!({ "role": "assistant", "content": "" }), Value::Null)
    }

    pub fn chunk_content(&self, piece: &str) -> String {
        self.chunk(json!({ "content": piece }), Value::Null)
    }

    /// One delta per call, carrying the whole call.
    ///
    /// The AI SDK accumulates by the `index` *inside* `tool_calls` — which is
    /// the call's position, not the choice's — and string-concatenates
    /// `arguments`. Since the whole call is already known, one delta is both
    /// correct and simpler than splitting it.
    pub fn chunk_tool_call(&self, index: usize, call: &ToolCall) -> String {
        self.chunk(
            json!({
                "tool_calls": [{
                    "index": index,
                    "id": call.id,
                    "type": "function",
                    "function": { "name": call.name, "arguments": call.arguments },
                }]
            }),
            Value::Null,
        )
    }

    pub fn chunk_finish(&self) -> String {
        self.chunk(json!({}), Value::String(self.finish_reason.clone()))
    }

    /// Emitted only when the client asked for it. A `"usage": null` on every
    /// other chunk is rejected by strict client schemas.
    pub fn chunk_usage(&self) -> String {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [],
            "usage": usage_json(&self.usage),
        })
        .to_string()
    }
}

/// An error that happened after the 200 head went out.
///
/// The status cannot be taken back, so the stream carries the reason instead.
/// No `finish_reason` is emitted before it: that would tell the client the
/// turn succeeded.
pub fn chunk_error(message: &str, code: &'static str) -> String {
    json!({
        "error": { "message": message, "type": "api_error", "param": null, "code": code }
    })
    .to_string()
}

/// Split text into SSE-sized pieces on character boundaries.
pub fn content_pieces(text: &str, max_bytes: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if current.len() + ch.len_utf8() > max_bytes && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Render a message back into the wire shape, for the canonical form the
/// conversation store keys on.
pub fn message_signature(m: &Message) -> String {
    let mut s = String::new();
    s.push_str(match m.role {
        Role::System => "s",
        Role::User => "u",
        Role::Assistant => "a",
        Role::Tool => "t",
    });
    s.push('\u{1}');
    s.push_str(m.content.trim());
    if let Some(id) = &m.tool_call_id {
        s.push('\u{1}');
        s.push_str(id);
    }
    for c in &m.tool_calls {
        s.push('\u{1}');
        s.push_str(&c.id);
        s.push('\u{2}');
        s.push_str(&c.name);
        s.push('\u{2}');
        // Canonical JSON, so a harness that re-serialised the arguments on its
        // way back does not look like a different conversation. serde_json's
        // object is a BTreeMap, so key order is already normal.
        match serde_json::from_str::<Value>(&c.arguments) {
            Ok(v) => s.push_str(&v.to_string()),
            Err(_) => s.push_str(c.arguments.trim()),
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(v: Value) -> Result<ChatRequest, ApiError> {
        parse_request(&v)
    }

    fn minimal(extra: Value) -> Value {
        let mut v = json!({"model": MODEL_ID, "messages": [{"role": "user", "content": "hi"}]});
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        v
    }

    #[test]
    fn the_model_id_resolves_with_or_without_its_provider_prefix() {
        assert!(model_matches("manager"));
        assert!(model_matches("ensemble/manager"));
        assert!(model_matches("some-provider/manager"));
        assert!(!model_matches("ensemble/coder"));
        assert!(!model_matches("gpt-4o"));
    }

    #[test]
    fn a_role_name_in_the_model_id_is_refused_like_any_other_unknown_model() {
        // Roles are not addressable. The manager chooses who does the work.
        let err =
            req(json!({"model": "ensemble/coder", "messages": [{"role":"user"}]})).unwrap_err();
        assert_eq!(err.status, 404);
        assert_eq!(err.code, "model_not_found");
        assert_eq!(err.param.as_deref(), Some("model"));
        assert!(err.message.contains(MODEL_ID), "{}", err.message);
    }

    #[test]
    fn a_missing_model_and_an_unknown_model_differ_in_status_and_code() {
        let missing = req(json!({"messages": [{"role":"user"}]})).unwrap_err();
        assert_eq!(missing.status, 400);
        assert_eq!(missing.code, "missing_required_parameter");

        let unknown = req(json!({"model": "zzz", "messages": [{"role":"user"}]})).unwrap_err();
        assert_eq!(unknown.status, 404);
    }

    #[test]
    fn the_model_id_is_echoed_back_exactly_as_the_client_wrote_it() {
        let r = req(minimal(json!({"model": "manager"}))).unwrap();
        assert_eq!(r.model, "manager");
    }

    #[test]
    fn empty_messages_is_a_400_rather_than_an_empty_completion() {
        let err = req(json!({"model": MODEL_ID, "messages": []})).unwrap_err();
        assert_eq!(err.status, 400);
        assert_eq!(err.param.as_deref(), Some("messages"));
    }

    #[test]
    fn missing_messages_names_the_parameter() {
        let err = req(json!({"model": MODEL_ID})).unwrap_err();
        assert_eq!(err.param.as_deref(), Some("messages"));
    }

    #[test]
    fn a_string_content_and_an_array_of_text_parts_translate_to_the_same_turn() {
        let a = req(json!({"model": MODEL_ID,
            "messages": [{"role": "user", "content": "one\ntwo"}]}))
        .unwrap();
        let b = req(
            json!({"model": MODEL_ID, "messages": [{"role": "user", "content": [
            {"type": "text", "text": "one"}, {"type": "text", "text": "two"}]}]}),
        )
        .unwrap();
        assert_eq!(a.messages[0].content, b.messages[0].content);
    }

    #[test]
    fn a_developer_role_is_treated_as_a_system_turn() {
        let r = req(json!({"model": MODEL_ID,
            "messages": [{"role": "developer", "content": "rules"}]}))
        .unwrap();
        assert_eq!(r.messages[0].role, Role::System);
    }

    #[test]
    fn a_non_text_part_becomes_a_marker_rather_than_vanishing() {
        let r = req(
            json!({"model": MODEL_ID, "messages": [{"role": "user", "content": [
            {"type": "text", "text": "look"},
            {"type": "image_url", "image_url": {"url": "data:..."}}]}]}),
        )
        .unwrap();
        assert!(r.messages[0].content.contains("look"));
        assert!(
            r.messages[0].content.contains("image_url omitted"),
            "{}",
            r.messages[0].content
        );
    }

    #[test]
    fn an_assistant_message_with_tool_calls_and_null_content_is_not_a_dropped_turn() {
        let r = req(json!({"model": MODEL_ID, "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_a", "type": "function",
                 "function": {"name": "read", "arguments": "{\"p\":1}"}}]},
            {"role": "tool", "tool_call_id": "call_a", "content": "ok"}]}))
        .unwrap();
        assert_eq!(r.messages.len(), 3);
        assert_eq!(r.messages[1].role, Role::Assistant);
        assert_eq!(r.messages[1].tool_calls.len(), 1);
        assert_eq!(r.messages[2].role, Role::Tool);
    }

    #[test]
    fn tool_call_ids_and_argument_bytes_survive_the_round_trip_unchanged() {
        let args = "{\"path\": \"a b/c.rs\", \"n\": 1}";
        let r = req(json!({"model": MODEL_ID, "messages": [
            {"role": "assistant", "tool_calls": [
                {"id": "call_XYZ", "function": {"name": "read", "arguments": args}}]}]}))
        .unwrap();
        assert_eq!(r.messages[0].tool_calls[0].id, "call_XYZ");
        assert_eq!(r.messages[0].tool_calls[0].arguments, args);

        let c = Completion {
            id: "x".into(),
            created: 0,
            model: MODEL_ID.into(),
            text: String::new(),
            tool_calls: r.messages[0].tool_calls.clone(),
            finish_reason: "tool_calls".into(),
            usage: Usage::default(),
        };
        let v: Value = serde_json::from_str(&c.body()).unwrap();
        assert_eq!(
            v["choices"][0]["message"]["tool_calls"][0]["id"],
            "call_XYZ"
        );
        assert_eq!(
            v["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            args
        );
        assert!(v["choices"][0]["message"]["content"].is_null());
    }

    #[test]
    fn a_tool_message_without_an_id_is_a_400_naming_the_message_index() {
        let err = req(json!({"model": MODEL_ID, "messages": [
            {"role": "user", "content": "go"},
            {"role": "tool", "content": "ok"}]}))
        .unwrap_err();
        assert_eq!(err.status, 400);
        assert_eq!(err.param.as_deref(), Some("messages[1].tool_call_id"));
    }

    #[test]
    fn an_unknown_message_role_is_a_400_naming_the_message_index() {
        let err = req(json!({"model": MODEL_ID, "messages": [
            {"role": "user", "content": "a"}, {"role": "narrator", "content": "b"}]}))
        .unwrap_err();
        assert_eq!(err.param.as_deref(), Some("messages[1].role"));
        assert!(err.message.contains("narrator"), "{}", err.message);
    }

    #[test]
    fn a_harness_tool_named_like_an_ensemble_tool_is_refused_not_shadowed() {
        let err = req(minimal(json!({"tools": [
            {"type": "function", "function": {"name": "ensemble_delegate",
             "parameters": {"type": "object"}}}]})))
        .unwrap_err();
        assert_eq!(err.code, "tool_name_conflict");
        assert!(err.message.contains("MCP server"), "{}", err.message);
    }

    #[test]
    fn harness_tool_schemas_are_carried_verbatim() {
        let schema = json!({"type": "object", "properties": {"p": {"type": "string"}},
                            "required": ["p"]});
        let r = req(minimal(json!({"tools": [{"type": "function", "function":
            {"name": "read", "description": "Read.", "parameters": schema}}]})))
        .unwrap();
        assert_eq!(r.tools[0].name, "read");
        assert_eq!(r.tools[0].parameters, schema);
    }

    #[test]
    fn tool_choice_none_means_no_tools_at_all_including_the_ensembles_own() {
        // A caller asking for a plain answer has not asked to pay for a
        // delegation it cannot see.
        assert_eq!(
            req(minimal(json!({"tool_choice": "none"})))
                .unwrap()
                .tool_policy,
            ToolPolicy::None
        );
        assert_eq!(
            req(minimal(json!({}))).unwrap().tool_policy,
            ToolPolicy::Auto
        );
    }

    #[test]
    fn forcing_a_named_tool_is_refused_rather_than_quietly_ignored() {
        let err = req(minimal(json!({"tool_choice":
            {"type": "function", "function": {"name": "read"}}})))
        .unwrap_err();
        assert_eq!(err.code, "unsupported_value");
        assert_eq!(err.param.as_deref(), Some("tool_choice"));
    }

    #[test]
    fn more_than_one_choice_is_refused() {
        let err = req(minimal(json!({"n": 2}))).unwrap_err();
        assert_eq!(err.param.as_deref(), Some("n"));
    }

    #[test]
    fn both_spellings_of_the_token_limit_are_read() {
        assert_eq!(
            req(minimal(json!({"max_tokens": 10}))).unwrap().max_tokens,
            Some(10)
        );
        assert_eq!(
            req(minimal(json!({"max_completion_tokens": 20})))
                .unwrap()
                .max_tokens,
            Some(20)
        );
    }

    #[test]
    fn streaming_and_usage_are_off_unless_asked_for() {
        let plain = req(minimal(json!({}))).unwrap();
        assert!(!plain.stream);
        assert!(!plain.include_usage);
        let asked = req(minimal(
            json!({"stream": true, "stream_options": {"include_usage": true}}),
        ))
        .unwrap();
        assert!(asked.stream && asked.include_usage);
    }

    #[test]
    fn the_error_body_is_the_shape_an_openai_client_parses() {
        let v: Value = serde_json::from_str(
            &ApiError::invalid(404, "model_not_found", "nope")
                .at("model")
                .body(),
        )
        .unwrap();
        assert_eq!(v["error"]["message"], "nope");
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert_eq!(v["error"]["param"], "model");
        assert_eq!(v["error"]["code"], "model_not_found");
    }

    #[test]
    fn a_body_that_is_not_json_says_so_with_the_position() {
        let err = parse_body(b"<html>").unwrap_err();
        assert_eq!(err.status, 400);
        assert!(err.message.contains("not JSON"), "{}", err.message);
    }

    #[test]
    fn the_models_list_offers_the_manager_and_nothing_else() {
        let v: Value = serde_json::from_str(&models_list()).unwrap();
        assert_eq!(v["data"].as_array().unwrap().len(), 1);
        assert_eq!(v["data"][0]["id"], MODEL_ID);
        assert_eq!(v["data"][0]["owned_by"], "ensemble");
    }

    #[test]
    fn content_is_split_on_character_boundaries_never_inside_one() {
        let text = "é".repeat(10);
        let pieces = content_pieces(&text, 5);
        assert_eq!(pieces.concat(), text);
        assert!(pieces.iter().all(|p| !p.is_empty()));
    }

    #[test]
    fn a_streamed_tool_call_delta_carries_the_index_id_name_and_arguments() {
        let c = Completion {
            id: "cid".into(),
            created: 7,
            model: MODEL_ID.into(),
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_a".into(),
                name: "read".into(),
                arguments: "{}".into(),
            }],
            finish_reason: "tool_calls".into(),
            usage: Usage::default(),
        };
        let v: Value = serde_json::from_str(&c.chunk_tool_call(0, &c.tool_calls[0])).unwrap();
        let d = &v["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(v["object"], "chat.completion.chunk");
        assert_eq!(d["index"], 0);
        assert_eq!(d["id"], "call_a");
        assert_eq!(d["type"], "function");
        assert_eq!(d["function"]["name"], "read");
        // A string, never an object: the client concatenates it.
        assert!(d["function"]["arguments"].is_string());
        assert!(v["choices"][0]["finish_reason"].is_null());

        let f: Value = serde_json::from_str(&c.chunk_finish()).unwrap();
        assert_eq!(f["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn the_usage_chunk_has_no_choices_and_a_total() {
        let c = Completion {
            id: "cid".into(),
            created: 0,
            model: MODEL_ID.into(),
            text: "x".into(),
            tool_calls: Vec::new(),
            finish_reason: "stop".into(),
            usage: Usage {
                prompt_tokens: 3,
                completion_tokens: 4,
            },
        };
        let v: Value = serde_json::from_str(&c.chunk_usage()).unwrap();
        assert_eq!(v["choices"].as_array().unwrap().len(), 0);
        assert_eq!(v["usage"]["total_tokens"], 7);
    }

    #[test]
    fn an_error_chunk_never_claims_the_turn_finished() {
        let v: Value = serde_json::from_str(&chunk_error("it broke", "upstream_error")).unwrap();
        assert_eq!(v["error"]["code"], "upstream_error");
        assert!(v.get("choices").is_none(), "no finish_reason anywhere: {v}");
    }

    #[test]
    fn two_conversations_differing_by_one_turn_have_different_signatures() {
        let a = message_signature(&Message::user("hello"));
        let b = message_signature(&Message::user("hell0"));
        assert_ne!(a, b);
    }

    #[test]
    fn a_reserialised_argument_object_signs_the_same_as_the_original() {
        // The harness is free to re-encode what we sent it; that must not look
        // like a different conversation.
        let one = Message::assistant_calling(
            "",
            vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: "{\"a\":1,\"b\":2}".into(),
            }],
        );
        let two = Message::assistant_calling(
            "",
            vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: "{ \"b\" : 2 , \"a\" : 1 }".into(),
            }],
        );
        assert_eq!(message_signature(&one), message_signature(&two));
    }
}
