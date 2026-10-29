//! The OpenAI-compatible API adapter.
//!
//! Anything that speaks `POST /chat/completions` — llama.cpp's `llama-server`,
//! vLLM, Ollama, a hosted API. This is the backend type that *can* call the
//! caller's tools, which is why a role with `tools:` has to sit on one.
//!
//! The HTTP transport is injected behind [`Http`] so every branch below — a
//! 500, a 429, a body that is 2xx but malformed, a refused connection — is
//! covered by a test that neither binds a port nor needs a model server.

use crate::config::{Backend, BackendSettings};
use crate::port::{
    Capabilities, Error, Message, ModelPort, Request, Response, Role, ToolCall, Usage,
};
use std::time::Duration;

/// A completed HTTP exchange. Status plus body is all this adapter needs.
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// The transport. `Err` means the request never got an answer — DNS, refused
/// connection, TLS, a read that timed out. A non-2xx status is an `Ok` with
/// that status, because the server did answer and the body usually says why.
pub trait Http: Send + Sync {
    fn post_json(
        &self,
        url: &str,
        bearer: Option<&str>,
        body: &str,
        timeout: Duration,
    ) -> Result<HttpResponse, String>;

    fn get(
        &self,
        url: &str,
        bearer: Option<&str>,
        timeout: Duration,
    ) -> Result<HttpResponse, String>;
}

pub struct UreqHttp;

impl UreqHttp {
    fn agent(timeout: Duration) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            // Hand back non-2xx as a response instead of an error.
            //
            // ureq's default turns a 503 into `Error::StatusCode(503)` with the
            // body already discarded — so "model is still loading" becomes
            // "http status 503" and the operator is left guessing. The body of
            // a failed call is the most useful thing in it.
            .http_status_as_error(false)
            .build()
            .into()
    }

    fn finish(
        r: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<HttpResponse, String> {
        match r {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                // A body that cannot be read is not a reason to lose the
                // status; `api_error` copes with an empty message.
                let body = resp.body_mut().read_to_string().unwrap_or_default();
                Ok(HttpResponse { status, body })
            }
            // Defensive: unreachable with the setting above, but losing a
            // status here would be silent.
            Err(ureq::Error::StatusCode(code)) => Ok(HttpResponse {
                status: code,
                body: String::new(),
            }),
            Err(e) => Err(e.to_string()),
        }
    }
}

impl Http for UreqHttp {
    fn post_json(
        &self,
        url: &str,
        bearer: Option<&str>,
        body: &str,
        timeout: Duration,
    ) -> Result<HttpResponse, String> {
        let agent = Self::agent(timeout);
        let mut req = agent.post(url).header("content-type", "application/json");
        if let Some(key) = bearer {
            req = req.header("authorization", &format!("Bearer {key}"));
        }
        Self::finish(req.send(body))
    }

    fn get(
        &self,
        url: &str,
        bearer: Option<&str>,
        timeout: Duration,
    ) -> Result<HttpResponse, String> {
        let agent = Self::agent(timeout);
        let mut req = agent.get(url);
        if let Some(key) = bearer {
            req = req.header("authorization", &format!("Bearer {key}"));
        }
        Self::finish(req.call())
    }
}

/// One message, in the shape `/chat/completions` wants it.
///
/// `content` is `null` rather than `""` beside `tool_calls`: several local
/// servers reject the empty string there, and the two mean different things.
fn message_json(m: &Message) -> serde_json::Value {
    match m.role {
        Role::Tool => serde_json::json!({
            "role": "tool",
            "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
            "content": m.content,
        }),
        Role::Assistant if !m.tool_calls.is_empty() => {
            let calls: Vec<serde_json::Value> = m
                .tool_calls
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "id": c.id,
                        "type": "function",
                        "function": { "name": c.name, "arguments": c.arguments },
                    })
                })
                .collect();
            let content = if m.content.trim().is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(m.content.clone())
            };
            serde_json::json!({ "role": "assistant", "content": content, "tool_calls": calls })
        }
        _ => serde_json::json!({ "role": m.role.as_str(), "content": m.content }),
    }
}

/// Build the `/chat/completions` request body.
///
/// `tools` is sent only when the caller offered some. A request that offers
/// none must look exactly as it did before this field existed.
pub fn request_body(request: &Request) -> String {
    let messages: Vec<serde_json::Value> = request.messages.iter().map(message_json).collect();

    let mut body = serde_json::json!({
        "model": request.model,
        "messages": messages,
        "stream": false,
    });
    if let Some(n) = request.max_tokens {
        body["max_tokens"] = serde_json::json!(n);
    }
    if let Some(t) = request.temperature {
        body["temperature"] = serde_json::json!(t);
    }
    if let Some(effort) = &request.reasoning_effort {
        body["reasoning_effort"] = serde_json::json!(effort);
    }
    if request.offers_tools() {
        let tools: Vec<serde_json::Value> = request
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    },
                })
            })
            .collect();
        body["tools"] = serde_json::Value::Array(tools);
        // `auto`: a model that has finished must be able to simply answer.
        body["tool_choice"] = serde_json::json!("auto");
    }
    body.to_string()
}

/// A tool result the server cannot pair to a call is a request that will come
/// back a 400 from somewhere further away. Catch it here, where the message
/// index is still known to the caller.
fn check_serviceable(backend: &str, request: &Request) -> Result<(), Error> {
    for (i, m) in request.messages.iter().enumerate() {
        if m.role == Role::Tool && m.tool_call_id.as_deref().unwrap_or("").trim().is_empty() {
            return Err(Error::Unsupported {
                backend: backend.into(),
                detail: format!(
                    "messages[{i}] is a tool result with no tool_call_id; \
                     there is no call for the server to pair it with"
                ),
            });
        }
    }
    Ok(())
}

/// The tool calls on `choices[0].message`, in the order the model wrote them.
fn parse_tool_calls(message: Option<&serde_json::Value>) -> Vec<ToolCall> {
    message
        .and_then(|m| m.get("tool_calls"))
        .and_then(|t| t.as_array())
        .map(|calls| {
            calls
                .iter()
                .filter_map(|c| {
                    let f = c.get("function")?;
                    let name = f.get("name").and_then(|n| n.as_str())?.to_string();
                    // Arguments are a JSON *string* by the spec, but some
                    // servers emit the object. Re-serialising beats rejecting:
                    // the caller only ever needs to hand it back.
                    let arguments = match f.get("arguments") {
                        Some(serde_json::Value::String(s)) => s.clone(),
                        Some(other) => other.to_string(),
                        None => "{}".to_string(),
                    };
                    Some(ToolCall {
                        id: c
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        name,
                        arguments,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Pull the text, tool calls, model and usage out of a chat-completion response.
///
/// `tools_offered` decides what a reply of nothing but tool calls means. If the
/// caller offered none, the model asked for something this port cannot run and
/// that is an error (see below). If it did offer some, the tool calls *are* the
/// answer and are returned as such.
pub fn parse_completion(body: &str, backend: &str, tools_offered: bool) -> Result<Response, Error> {
    let v: serde_json::Value = serde_json::from_str(body.trim()).map_err(|e| Error::Protocol {
        backend: backend.into(),
        detail: format!("not JSON ({e}): {body}"),
    })?;

    // Some servers answer 200 with an `error` object. That is a failure wearing
    // a success status, and treating it as an empty completion would hand the
    // caller silence instead of a reason.
    if let Some(err) = v.get("error") {
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| err.to_string());
        return Err(Error::Api {
            backend: backend.into(),
            status: 200,
            message,
        });
    }

    let choice = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| Error::Protocol {
            backend: backend.into(),
            detail: format!("no `choices[0]`: {body}"),
        })?;

    let message = choice.get("message");
    let content = message
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    // Thinking models (Qwen3.5 on Ollama, for one) sometimes put the whole
    // answer in the reasoning channel and finish with an empty `content`. That
    // is an answer, not silence, and handing the caller an empty assistant turn
    // makes the conversation look like it stopped. Only fall back when there is
    // no tool call: beside a tool call the reasoning is working-out, not what
    // the caller asked for.
    let reasoning = message
        .and_then(|m| m.get("reasoning").or_else(|| m.get("reasoning_content")))
        .and_then(|r| r.as_str())
        .unwrap_or("");

    let finish_reason = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .map(str::to_string);

    let tool_calls = parse_tool_calls(message);

    let usage = v.get("usage");
    let num = |k: &str| {
        usage
            .and_then(|u| u.get(k))
            .and_then(|n| n.as_u64())
            .unwrap_or(0) as u32
    };
    let usage = Usage {
        prompt_tokens: num("prompt_tokens"),
        completion_tokens: num("completion_tokens"),
    };
    let model = v
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown")
        .to_string();

    finish(
        Parsed {
            content,
            reasoning,
            tool_calls,
            finish_reason,
            model,
            usage,
        },
        tools_offered,
        backend,
    )
}

/// What a reply boils down to, whichever shape it arrived in — one blocking
/// response.
struct Parsed<'a> {
    content: &'a str,
    reasoning: &'a str,
    tool_calls: Vec<ToolCall>,
    finish_reason: Option<String>,
    model: String,
    usage: Usage,
}

/// The validation and text-selection a reply goes through once it is reduced
/// to a [`Parsed`].
fn finish(parsed: Parsed, tools_offered: bool, backend: &str) -> Result<Response, Error> {
    let Parsed {
        content,
        reasoning,
        tool_calls,
        finish_reason,
        model,
        usage,
    } = parsed;

    let text = if content.trim().is_empty()
        && tool_calls.is_empty()
        && finish_reason.as_deref() != Some("length")
    {
        reasoning.to_string()
    } else {
        content.to_string()
    };

    // A model that answered a *toolless* request with only `tool_calls` has not
    // done the delegated work — it asked for a tool nobody offered it. Say
    // that, rather than returning an empty string that reads as "it had nothing
    // to say". See the `tools` note on `port::Request`.
    if !tool_calls.is_empty() && !tools_offered {
        return Err(Error::Unsupported {
            backend: backend.into(),
            detail: "the model replied with tool calls instead of an answer. \
                     Ensemble delegates whole tasks and does not execute the \
                     harness's tools on the model's behalf."
                .into(),
        });
    }

    if text.trim().is_empty() && tool_calls.is_empty() && finish_reason.as_deref() == Some("length")
    {
        return Err(Error::Protocol {
            backend: backend.into(),
            detail: "the reply was cut off at the token limit before any content \
                     was produced. Thinking may have used it up, or the context is full: \
                     raise max_tokens or the server's context, or set `reasoning_effort: none` on the role"
                .into(),
        });
    }

    Ok(Response {
        text,
        tool_calls,
        model,
        usage,
        finish_reason,
    })
}

/// Turn a non-2xx into an `Api` error, preferring the server's own message.
fn api_error(backend: &str, status: u16, body: &str) -> Error {
    let message = serde_json::from_str::<serde_json::Value>(body.trim())
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message").or(Some(e)))
                .map(|m| {
                    m.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| m.to_string())
                })
        })
        .unwrap_or_else(|| body.trim().to_string());

    let message = if message.is_empty() {
        "(no body)".to_string()
    } else {
        message
    };

    // 401/403 is nearly always the key variable, and the config knows which one
    // it is — but the adapter does not, so point at the class of fix.
    let message = match status {
        401 | 403 => format!("{message} — check the backend's api_key_env variable"),
        404 => format!("{message} — check base_url points at the /v1 root"),
        _ => message,
    };

    Error::Api {
        backend: backend.into(),
        status,
        message,
    }
}

pub struct OpenAiAdapter {
    name: String,
    base_url: String,
    api_key: Option<String>,
    timeout: Duration,
    http: Box<dyn Http>,
}

impl OpenAiAdapter {
    pub fn new(backend: &Backend, http: Box<dyn Http>) -> Result<Self, Error> {
        let BackendSettings::OpenAiCompatible {
            base_url, api_key, ..
        } = &backend.settings
        else {
            return Err(Error::Unsupported {
                backend: backend.name.clone(),
                detail: "not an openai-compatible-api backend".into(),
            });
        };
        Ok(OpenAiAdapter {
            name: backend.name.clone(),
            base_url: base_url.clone(),
            api_key: api_key.clone(),
            timeout: Duration::from_secs(backend.timeout_seconds),
            http,
        })
    }

    pub fn endpoint(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }
}

impl ModelPort for OpenAiAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn type_id(&self) -> &'static str {
        crate::config::OPENAI_COMPATIBLE_API
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { tool_calling: true }
    }

    fn complete(&self, request: &Request) -> Result<Response, Error> {
        check_serviceable(&self.name, request)?;

        let url = self.endpoint("chat/completions");
        let body = request_body(request);

        let resp = self
            .http
            .post_json(&url, self.api_key.as_deref(), &body, self.timeout)
            .map_err(|detail| classify_transport(&self.name, detail, self.timeout))?;

        if !(200..300).contains(&resp.status) {
            return Err(api_error(&self.name, resp.status, &resp.body));
        }
        parse_completion(&resp.body, &self.name, request.offers_tools())
    }

    fn health(&self) -> Result<(), Error> {
        // `/models` is the cheapest endpoint that proves the server is up and
        // the key is accepted, and it spends no tokens.
        let resp = self
            .http
            .get(
                &self.endpoint("models"),
                self.api_key.as_deref(),
                Duration::from_secs(15),
            )
            .map_err(|detail| classify_transport(&self.name, detail, Duration::from_secs(15)))?;
        if !(200..300).contains(&resp.status) {
            return Err(api_error(&self.name, resp.status, &resp.body));
        }
        Ok(())
    }
}

/// A read that ran out of time is a timeout, not a generic transport failure —
/// they differ in whether retrying the same call is worth suggesting, and in
/// which knob the operator should reach for.
fn classify_transport(backend: &str, detail: String, timeout: Duration) -> Error {
    let lower = detail.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        Error::Timeout {
            backend: backend.into(),
            seconds: timeout.as_secs(),
        }
    } else {
        Error::Transport {
            backend: backend.into(),
            detail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Backend, BackendSettings};
    use crate::port::{Message, ToolDef};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeHttp {
        reply: Mutex<Option<Result<HttpResponse, String>>>,
        posts: Mutex<Vec<(String, Option<String>, String)>>,
    }

    impl FakeHttp {
        fn status(status: u16, body: &str) -> Arc<Self> {
            Arc::new(FakeHttp {
                reply: Mutex::new(Some(Ok(HttpResponse {
                    status,
                    body: body.into(),
                }))),
                posts: Mutex::new(Vec::new()),
            })
        }
        fn ok(body: &str) -> Arc<Self> {
            Self::status(200, body)
        }
        fn broken(detail: &str) -> Arc<Self> {
            Arc::new(FakeHttp {
                reply: Mutex::new(Some(Err(detail.into()))),
                posts: Mutex::new(Vec::new()),
            })
        }
        fn last_post(&self) -> (String, Option<String>, String) {
            self.posts
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("a POST was made")
        }
    }

    impl Http for Arc<FakeHttp> {
        fn post_json(
            &self,
            url: &str,
            bearer: Option<&str>,
            body: &str,
            _t: Duration,
        ) -> Result<HttpResponse, String> {
            self.posts
                .lock()
                .unwrap()
                .push((url.into(), bearer.map(str::to_string), body.into()));
            match &*self.reply.lock().unwrap() {
                Some(Ok(r)) => Ok(HttpResponse {
                    status: r.status,
                    body: r.body.clone(),
                }),
                Some(Err(e)) => Err(e.clone()),
                None => Err("no scripted reply".into()),
            }
        }
        fn get(&self, _u: &str, _b: Option<&str>, _t: Duration) -> Result<HttpResponse, String> {
            match &*self.reply.lock().unwrap() {
                Some(Ok(r)) => Ok(HttpResponse {
                    status: r.status,
                    body: r.body.clone(),
                }),
                Some(Err(e)) => Err(e.clone()),
                None => Err("no scripted reply".into()),
            }
        }
    }

    fn backend(api_key: Option<&str>) -> Backend {
        Backend {
            name: "openai-local".into(),
            timeout_seconds: 60,
            settings: BackendSettings::OpenAiCompatible {
                base_url: "http://127.0.0.1:8000/v1".into(),
                api_key: api_key.map(str::to_string),
                base_url_env: "OPENAI_LOCAL_BASE_URL".into(),
            },
        }
    }

    const GOOD: &str = r#"{
      "id":"chatcmpl-1","object":"chat.completion","model":"qwen3-coder",
      "choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],
      "usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}
    }"#;

    fn adapter(http: Arc<FakeHttp>, key: Option<&str>) -> OpenAiAdapter {
        OpenAiAdapter::new(&backend(key), Box::new(http)).unwrap()
    }

    #[test]
    fn the_url_is_the_base_plus_chat_completions_with_no_double_slash() {
        let http = FakeHttp::ok(GOOD);
        let a = adapter(http.clone(), None);
        a.complete(&Request::new("qwen3-coder", vec![Message::user("hi")]))
            .unwrap();
        assert_eq!(
            http.last_post().0,
            "http://127.0.0.1:8000/v1/chat/completions"
        );
    }

    #[test]
    fn the_api_key_travels_as_a_bearer_header_and_is_absent_when_unset() {
        let with = FakeHttp::ok(GOOD);
        adapter(with.clone(), Some("sk-abc"))
            .complete(&Request::new("m", vec![Message::user("hi")]))
            .unwrap();
        assert_eq!(with.last_post().1.as_deref(), Some("sk-abc"));

        let without = FakeHttp::ok(GOOD);
        adapter(without.clone(), None)
            .complete(&Request::new("m", vec![Message::user("hi")]))
            .unwrap();
        assert_eq!(without.last_post().1, None);
    }

    #[test]
    fn the_body_carries_model_messages_and_no_streaming() {
        let mut req = Request::new(
            "qwen3-coder",
            vec![Message::system("sys"), Message::user("go")],
        );
        req.max_tokens = Some(256);
        req.temperature = Some(0.2);
        let v: serde_json::Value = serde_json::from_str(&request_body(&req)).unwrap();
        assert_eq!(v["model"], "qwen3-coder");
        assert_eq!(v["stream"], false);
        assert_eq!(v["max_tokens"], 256);
        assert_eq!(v["messages"][0]["role"], "system");
        assert_eq!(v["messages"][1]["content"], "go");
        // This request offered no tools, so none are sent and the body looks
        // exactly as it did before the field existed.
        assert!(v.get("tools").is_none());
        assert!(v.get("tool_choice").is_none());
    }

    #[test]
    fn optional_parameters_are_omitted_when_unset() {
        let v: serde_json::Value =
            serde_json::from_str(&request_body(&Request::new("m", vec![Message::user("x")])))
                .unwrap();
        assert!(v.get("max_tokens").is_none());
        assert!(v.get("temperature").is_none());
        assert!(v.get("reasoning_effort").is_none());
    }

    #[test]
    fn reasoning_effort_is_sent_when_set() {
        let mut req = Request::new("m", vec![Message::user("x")]);
        req.reasoning_effort = Some("none".into());
        let v: serde_json::Value = serde_json::from_str(&request_body(&req)).unwrap();
        assert_eq!(v["reasoning_effort"], "none");
    }

    #[test]
    fn a_good_completion_is_parsed_with_its_usage() {
        let r = parse_completion(GOOD, "b", false).unwrap();
        assert_eq!(r.text, "done");
        assert!(r.tool_calls.is_empty(), "none were offered, none come back");
        assert_eq!(r.model, "qwen3-coder");
        assert_eq!(r.usage.prompt_tokens, 10);
        assert_eq!(r.usage.total(), 13);
        assert_eq!(r.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn a_500_becomes_a_retryable_api_error_carrying_the_servers_message() {
        let a = adapter(
            FakeHttp::status(503, r#"{"error":{"message":"model is loading"}}"#),
            None,
        );
        let err = a
            .complete(&Request::new("m", vec![Message::user("hi")]))
            .unwrap_err();
        assert!(matches!(err, Error::Api { status: 503, .. }), "{err}");
        assert!(err.to_string().contains("model is loading"), "{err}");
        assert!(err.is_retryable());
    }

    #[test]
    fn a_401_points_at_the_key_variable_and_is_not_retryable() {
        let a = adapter(
            FakeHttp::status(401, r#"{"error":{"message":"bad key"}}"#),
            Some("sk-x"),
        );
        let err = a
            .complete(&Request::new("m", vec![Message::user("hi")]))
            .unwrap_err();
        assert!(err.to_string().contains("api_key_env"), "{err}");
        assert!(!err.is_retryable());
    }

    #[test]
    fn a_404_points_at_the_base_url() {
        let a = adapter(FakeHttp::status(404, "Not Found"), None);
        let err = a
            .complete(&Request::new("m", vec![Message::user("hi")]))
            .unwrap_err();
        assert!(err.to_string().contains("base_url"), "{err}");
    }

    #[test]
    fn an_error_body_served_with_200_is_still_an_error() {
        // Several local servers do this. Treating it as an empty completion
        // hands the caller silence instead of the reason.
        let a = adapter(
            FakeHttp::ok(r#"{"error":{"message":"context length exceeded"}}"#),
            None,
        );
        let err = a
            .complete(&Request::new("m", vec![Message::user("hi")]))
            .unwrap_err();
        assert!(err.to_string().contains("context length exceeded"), "{err}");
    }

    #[test]
    fn a_malformed_body_is_a_protocol_error() {
        let a = adapter(FakeHttp::ok("<html>502 Bad Gateway</html>"), None);
        let err = a
            .complete(&Request::new("m", vec![Message::user("hi")]))
            .unwrap_err();
        assert!(matches!(err, Error::Protocol { .. }), "{err}");
        assert!(!err.is_retryable());
    }

    #[test]
    fn a_2xx_with_no_choices_is_a_protocol_error() {
        let a = adapter(
            FakeHttp::ok(r#"{"id":"x","object":"chat.completion"}"#),
            None,
        );
        assert!(matches!(
            a.complete(&Request::new("m", vec![Message::user("hi")]))
                .unwrap_err(),
            Error::Protocol { .. }
        ));
    }

    #[test]
    fn a_refused_connection_is_a_retryable_transport_error() {
        let a = adapter(FakeHttp::broken("tcp connect: Connection refused"), None);
        let err = a
            .complete(&Request::new("m", vec![Message::user("hi")]))
            .unwrap_err();
        assert!(matches!(err, Error::Transport { .. }), "{err}");
        assert!(err.is_retryable());
        assert!(err.to_string().contains("Connection refused"), "{err}");
    }

    #[test]
    fn a_transport_timeout_is_reported_as_a_timeout() {
        let a = adapter(FakeHttp::broken("timed out reading response"), None);
        let err = a
            .complete(&Request::new("m", vec![Message::user("hi")]))
            .unwrap_err();
        assert!(matches!(err, Error::Timeout { .. }), "{err}");
    }

    #[test]
    fn a_reply_that_is_only_tool_calls_is_refused_with_an_explanation() {
        let body = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":null,
            "tool_calls":[{"id":"c1","type":"function","function":{"name":"read","arguments":"{}"}}]},
            "finish_reason":"tool_calls"}]}"#;
        let err = parse_completion(body, "b", false).unwrap_err();
        assert!(matches!(err, Error::Unsupported { .. }), "{err}");
        assert!(err.to_string().contains("delegates whole tasks"), "{err}");
    }

    #[test]
    fn an_empty_reply_cut_off_at_the_token_limit_says_so() {
        let body = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":""},
            "finish_reason":"length"}]}"#;
        let err = parse_completion(body, "b", false).unwrap_err();
        assert!(err.to_string().contains("max_tokens"), "{err}");
    }

    #[test]
    fn a_reply_that_finished_in_the_reasoning_channel_is_still_an_answer() {
        // Ollama's Qwen3.5 puts the final answer in `reasoning` and leaves
        // `content` empty on some prompts. Returning "" makes the harness think
        // the conversation stopped after one turn.
        let body = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"",
            "reasoning":"Here are the roles you can delegate to: coder, planner."},
            "finish_reason":"stop"}]}"#;
        let r = parse_completion(body, "b", false).unwrap();
        assert_eq!(
            r.text,
            "Here are the roles you can delegate to: coder, planner."
        );
    }

    #[test]
    fn reasoning_content_is_accepted_as_well_as_reasoning() {
        let body = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":null,
            "reasoning_content":"plan: write it in two steps"},
            "finish_reason":"stop"}]}"#;
        let r = parse_completion(body, "b", false).unwrap();
        assert_eq!(r.text, "plan: write it in two steps");
    }

    #[test]
    fn reasoning_cut_off_at_the_token_limit_still_reports_the_truncation() {
        let body = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"",
            "reasoning":"Here is the start of an answer that never finished"},
            "finish_reason":"length"}]}"#;
        let err = parse_completion(body, "b", false).unwrap_err();
        assert!(err.to_string().contains("max_tokens"), "{err}");
    }

    #[test]
    fn content_wins_over_reasoning_when_both_are_present() {
        let body = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"done",
            "reasoning":"the working-out that led here"},
            "finish_reason":"stop"}]}"#;
        let r = parse_completion(body, "b", false).unwrap();
        assert_eq!(r.text, "done");
    }

    #[test]
    fn reasoning_beside_a_tool_call_is_not_sent_as_content() {
        let body = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"",
            "reasoning":"I should call read.",
            "tool_calls":[{"id":"c1","type":"function",
                "function":{"name":"read","arguments":"{}"}}]},
            "finish_reason":"tool_calls"}]}"#;
        let r = parse_completion(body, "b", true).unwrap();
        assert!(r.text.is_empty(), "the reasoning is not the answer here");
        assert_eq!(r.tool_calls[0].name, "read");
    }

    #[test]
    fn health_hits_models_and_spends_no_tokens() {
        let http = FakeHttp::ok(r#"{"object":"list","data":[]}"#);
        let a = adapter(http.clone(), None);
        a.health().unwrap();
        assert!(
            http.posts.lock().unwrap().is_empty(),
            "health must not POST a completion"
        );
    }

    #[test]
    fn health_surfaces_a_down_server() {
        let a = adapter(FakeHttp::broken("Connection refused"), None);
        assert!(a.health().is_err());
    }

    #[test]
    fn this_backend_declares_tool_calling() {
        let a = adapter(FakeHttp::ok(GOOD), None);
        assert!(a.capabilities().tool_calling);
    }

    // --- tool calling ---------------------------------------------------

    fn read_tool() -> ToolDef {
        ToolDef {
            name: "read".into(),
            description: "Read a file.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "filePath": { "type": "string" } },
                "required": ["filePath"]
            }),
        }
    }

    const CALLED: &str = r#"{
      "id":"chatcmpl-2","object":"chat.completion","model":"qwen3-coder",
      "choices":[{"index":0,"message":{"role":"assistant","content":null,
        "tool_calls":[{"id":"call_abc","type":"function",
          "function":{"name":"read","arguments":"{\"filePath\": \"src/main.rs\"}"}}]},
        "finish_reason":"tool_calls"}],
      "usage":{"prompt_tokens":5,"completion_tokens":7}
    }"#;

    #[test]
    fn tools_are_sent_only_when_the_caller_offers_them() {
        let mut req = Request::new("m", vec![Message::user("go")]);
        req.tools = vec![read_tool()];
        let v: serde_json::Value = serde_json::from_str(&request_body(&req)).unwrap();
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["function"]["name"], "read");
        // The schema travels verbatim rather than being re-modelled.
        assert_eq!(
            v["tools"][0]["function"]["parameters"],
            read_tool().parameters
        );
        assert_eq!(v["tool_choice"], "auto");
    }

    #[test]
    fn an_assistant_turn_with_tool_calls_is_sent_with_a_null_content() {
        // Not "": several local servers reject the empty string beside
        // tool_calls, and the two do not mean the same thing.
        let req = Request::new(
            "m",
            vec![Message::assistant_calling(
                "",
                vec![ToolCall {
                    id: "call_abc".into(),
                    name: "read".into(),
                    arguments: "{\"filePath\":\"x\"}".into(),
                }],
            )],
        );
        let v: serde_json::Value = serde_json::from_str(&request_body(&req)).unwrap();
        assert!(v["messages"][0]["content"].is_null());
        assert_eq!(v["messages"][0]["tool_calls"][0]["id"], "call_abc");
        assert_eq!(v["messages"][0]["tool_calls"][0]["type"], "function");
        assert_eq!(
            v["messages"][0]["tool_calls"][0]["function"]["arguments"],
            "{\"filePath\":\"x\"}"
        );
    }

    #[test]
    fn a_tool_result_turn_carries_the_id_it_answers() {
        let req = Request::new("m", vec![Message::tool_result("call_abc", "ok")]);
        let v: serde_json::Value = serde_json::from_str(&request_body(&req)).unwrap();
        assert_eq!(v["messages"][0]["role"], "tool");
        assert_eq!(v["messages"][0]["tool_call_id"], "call_abc");
        assert_eq!(v["messages"][0]["content"], "ok");
    }

    #[test]
    fn a_tool_result_with_no_id_is_refused_before_it_is_sent() {
        let a = adapter(FakeHttp::ok(GOOD), None);
        let mut m = Message::tool_result("call_abc", "ok");
        m.tool_call_id = None;
        let err = a.complete(&Request::new("m", vec![m])).unwrap_err();
        assert!(matches!(err, Error::Unsupported { .. }), "{err}");
        assert!(err.to_string().contains("tool_call_id"), "{err}");
    }

    #[test]
    fn tool_calls_come_back_when_tools_were_offered() {
        let r = parse_completion(CALLED, "b", true).unwrap();
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].id, "call_abc");
        assert_eq!(r.tool_calls[0].name, "read");
        // Byte for byte: the caller hands this back and pairs on the id.
        assert_eq!(r.tool_calls[0].arguments, "{\"filePath\": \"src/main.rs\"}");
        assert_eq!(r.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(r.text, "");
    }

    #[test]
    fn the_same_reply_is_an_error_when_no_tools_were_offered() {
        let err = parse_completion(CALLED, "b", false).unwrap_err();
        assert!(matches!(err, Error::Unsupported { .. }), "{err}");
        assert!(err.to_string().contains("delegates whole tasks"), "{err}");
    }

    #[test]
    fn arguments_emitted_as_an_object_are_carried_rather_than_rejected() {
        // Out of spec, and several local servers do it anyway. The caller only
        // ever hands the string back, so re-serialising loses nothing.
        let body = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":null,
            "tool_calls":[{"id":"c1","type":"function",
              "function":{"name":"read","arguments":{"filePath":"x"}}}]},
            "finish_reason":"tool_calls"}]}"#;
        let r = parse_completion(body, "b", true).unwrap();
        assert_eq!(r.tool_calls[0].arguments, r#"{"filePath":"x"}"#);
    }

    #[test]
    fn a_completion_with_both_content_and_tool_calls_keeps_both() {
        let body = r#"{"choices":[{"index":0,"message":{"role":"assistant",
            "content":"Reading it now.",
            "tool_calls":[{"id":"c1","type":"function",
              "function":{"name":"read","arguments":"{}"}}]},
            "finish_reason":"tool_calls"}]}"#;
        let r = parse_completion(body, "b", true).unwrap();
        assert_eq!(r.text, "Reading it now.");
        assert_eq!(r.tool_calls.len(), 1);
    }
}
