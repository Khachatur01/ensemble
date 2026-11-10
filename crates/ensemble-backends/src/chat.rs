//! One chat request to an LLM backend, in the backend's own format: an
//! OpenAI-compatible `/chat/completions`, or Anthropic's `/v1/messages`. The
//! `direct` adapter is this and nothing more; whatever else in Ensemble has to
//! ask a model by itself asks it here.

use ensemble_config::{BackendSettings, LlmBackend};
use ensemble_core::port::{Error, Usage};
use serde_json::{Value, json};
use std::time::Duration;

/// The version of Anthropic's API that is asked for.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// What one request asks.
#[derive(Debug, Clone, Copy)]
pub struct Ask<'a> {
    /// The id the backend serves the model under.
    pub model: &'a str,
    /// The system prompt, when there is one.
    pub system: Option<&'a str>,
    /// The one user message.
    pub user: &'a str,
    /// Most tokens of answer.
    pub max_tokens: u32,
}

/// What came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// The answer's text, marked when it was cut off at `max_tokens`.
    pub text: String,
    /// The model that answered, as the backend named it, else the one asked.
    pub model: String,
    pub usage: Usage,
}

/// The request for `backend`: the path after its `base_url`, and the body.
pub fn request(backend: &LlmBackend, ask: &Ask<'_>) -> (&'static str, Value) {
    match backend.settings {
        BackendSettings::OpenAiCompatible { .. } => ("/chat/completions", openai_body(ask)),
        BackendSettings::Anthropic { .. } => ("/v1/messages", anthropic_body(ask)),
    }
}

/// The body of an OpenAI-compatible `/chat/completions` request.
pub fn openai_body(ask: &Ask<'_>) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = ask.system.filter(|p| !p.trim().is_empty()) {
        messages.push(json!({ "role": "system", "content": system }));
    }
    messages.push(json!({ "role": "user", "content": ask.user }));
    json!({ "model": ask.model, "messages": messages, "max_tokens": ask.max_tokens })
}

/// The body of an Anthropic `/v1/messages` request.
pub fn anthropic_body(ask: &Ask<'_>) -> Value {
    let mut body = json!({
        "model": ask.model,
        "max_tokens": ask.max_tokens,
        "messages": [{ "role": "user", "content": ask.user }],
    });
    if let Some(system) = ask.system.filter(|p| !p.trim().is_empty()) {
        body["system"] = json!(system);
    }
    body
}

/// Send `body` to `path` of `backend` and read the answer. It waits at most the
/// backend's `timeout_seconds`.
pub fn send(backend: &LlmBackend, path: &str, body: &Value) -> Result<Answer, Error> {
    let (base_url, api_key) = (backend.settings.base_url(), backend.settings.api_key());
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(backend.timeout_seconds)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut req = agent
        .post(format!("{base_url}{path}"))
        .header("content-type", "application/json");
    req = match (&backend.settings, api_key) {
        (BackendSettings::OpenAiCompatible { .. }, Some(key)) => {
            req.header("authorization", &format!("Bearer {key}"))
        }
        (BackendSettings::Anthropic { .. }, key) => {
            let req = req.header("anthropic-version", ANTHROPIC_VERSION);
            match key {
                Some(key) => req.header("x-api-key", key),
                None => req,
            }
        }
        (_, None) => req,
    };
    let mut resp = req.send(body.to_string()).map_err(|e| {
        let detail = e.to_string();
        if detail.to_ascii_lowercase().contains("timeout") || detail.contains("timed out") {
            Error::Timeout {
                backend: backend.name.clone(),
                seconds: backend.timeout_seconds,
            }
        } else {
            Error::Transport {
                backend: backend.name.clone(),
                detail,
            }
        }
    })?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    let model = body["model"].as_str().unwrap_or("");
    match backend.settings {
        BackendSettings::OpenAiCompatible { .. } => parse(&backend.name, model, status, &text),
        BackendSettings::Anthropic { .. } => parse_anthropic(&backend.name, model, status, &text),
    }
}

/// An answer from Anthropic's Messages API: the text blocks of `content`.
fn parse_anthropic(backend: &str, model: &str, status: u16, body: &str) -> Result<Answer, Error> {
    if !(200..300).contains(&status) {
        return Err(Error::Api {
            backend: backend.into(),
            status,
            message: body.to_string(),
        });
    }
    let protocol = |detail: String| Error::Protocol {
        backend: backend.into(),
        detail,
    };
    let json: Value =
        serde_json::from_str(body).map_err(|e| protocol(format!("the answer is not JSON: {e}")))?;
    let blocks = json["content"]
        .as_array()
        .ok_or_else(|| protocol(format!("no content in: {body}")))?;
    let mut text = blocks
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    if json["stop_reason"] == "max_tokens" {
        text.push_str("\n[cut off at max_tokens; the answer is unfinished]");
    }
    let tokens = |key: &str| json["usage"][key].as_u64().unwrap_or(0) as u32;
    Ok(Answer {
        text,
        model: json["model"].as_str().unwrap_or(model).to_string(),
        usage: Usage {
            prompt_tokens: tokens("input_tokens"),
            completion_tokens: tokens("output_tokens"),
        },
    })
}

fn parse(backend: &str, model: &str, status: u16, body: &str) -> Result<Answer, Error> {
    if !(200..300).contains(&status) {
        return Err(Error::Api {
            backend: backend.into(),
            status,
            message: body.to_string(),
        });
    }
    let protocol = |detail: String| Error::Protocol {
        backend: backend.into(),
        detail,
    };
    let json: Value =
        serde_json::from_str(body).map_err(|e| protocol(format!("the answer is not JSON: {e}")))?;
    let choice = &json["choices"][0];
    let content = choice["message"]["content"]
        .as_str()
        .ok_or_else(|| protocol(format!("no choices[0].message.content in: {body}")))?;
    let mut text = content.trim().to_string();
    if choice["finish_reason"] == "length" {
        text.push_str("\n[cut off at max_tokens; the answer is unfinished]");
    }
    let tokens = |key: &str| json["usage"][key].as_u64().unwrap_or(0) as u32;
    Ok(Answer {
        text,
        model: json["model"].as_str().unwrap_or(model).to_string(),
        usage: Usage {
            prompt_tokens: tokens("prompt_tokens"),
            completion_tokens: tokens("completion_tokens"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_anthropic_answer_cut_off_or_refused_says_so() {
        let cut = parse_anthropic(
            "b",
            "m",
            200,
            r#"{"content":[{"type":"text","text":"half"}],"stop_reason":"max_tokens"}"#,
        )
        .unwrap();
        assert!(cut.text.contains("cut off at max_tokens"), "{}", cut.text);
        let refused = parse_anthropic("b", "m", 401, "{\"error\":\"bad key\"}").unwrap_err();
        assert!(refused.to_string().contains("401"), "{refused}");
        assert!(parse_anthropic("b", "m", 200, "{}").is_err());
    }

    #[test]
    fn an_openai_answer_reads_text_model_and_usage() {
        let a = parse(
            "b",
            "asked",
            200,
            r#"{"model":"served","choices":[{"message":{"content":" hi "},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":1}}"#,
        )
        .unwrap();
        assert_eq!((a.text.as_str(), a.model.as_str()), ("hi", "served"));
        assert_eq!((a.usage.prompt_tokens, a.usage.completion_tokens), (4, 1));
        assert!(parse("b", "m", 200, "{}").is_err());
    }

    #[test]
    fn a_system_prompt_that_is_blank_is_left_out() {
        let ask = Ask {
            model: "m",
            system: Some("  "),
            user: "u",
            max_tokens: 5,
        };
        assert_eq!(openai_body(&ask)["messages"].as_array().unwrap().len(), 1);
        assert!(anthropic_body(&ask).get("system").is_none());
    }
}
