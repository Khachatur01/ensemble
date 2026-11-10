//! The `direct` adapter: a light agent that is one chat request to an LLM
//! backend, with no CLI in between. No tools: the model answers from the task, its context
//! and the agent's prompt, so it suits cheap work that needs no files, such
//! as summarising, classifying or drafting from what it is given.

use super::AgentRun;
use crate::config::{AgentConfig, BackendSettings, DIRECT_AGENT, DirectAdapter, LlmBackend};
use crate::port::{Error, Usage};
use crate::workspace::Changes;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The version of Anthropic's API that is asked for.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The request for `backend`: the path after its `base_url`, and the body.
pub fn request_for(
    backend: &LlmBackend,
    agent: &AgentConfig,
    la: &DirectAdapter,
    task: &str,
    context: Option<&str>,
) -> (&'static str, Value) {
    match backend.settings {
        BackendSettings::OpenAiCompatible { .. } => {
            ("/chat/completions", request(agent, la, task, context))
        }
        BackendSettings::Anthropic { .. } => {
            let user = match context.filter(|c| !c.trim().is_empty()) {
                Some(ctx) => format!("Context:\n{ctx}\n\n{task}"),
                None => task.to_string(),
            };
            let mut body = json!({
                "model": la.model,
                "max_tokens": la.max_tokens,
                "messages": [{ "role": "user", "content": user }],
            });
            if let Some(prompt) = agent.prompt.as_deref().filter(|p| !p.trim().is_empty()) {
                body["system"] = json!(prompt);
            }
            ("/v1/messages", body)
        }
    }
}

/// The request, as it is sent to `{base_url}/chat/completions`.
pub fn request(
    agent: &AgentConfig,
    la: &DirectAdapter,
    task: &str,
    context: Option<&str>,
) -> Value {
    let mut messages = Vec::new();
    if let Some(prompt) = agent.prompt.as_deref().filter(|p| !p.trim().is_empty()) {
        messages.push(json!({ "role": "system", "content": prompt }));
    }
    let user = match context.filter(|c| !c.trim().is_empty()) {
        Some(ctx) => format!("Context:\n{ctx}\n\n{task}"),
        None => task.to_string(),
    };
    messages.push(json!({ "role": "user", "content": user }));
    json!({ "model": la.model, "messages": messages, "max_tokens": la.max_tokens })
}

/// Send the task and return the answer. The request runs on a thread of its
/// own so a cancel or the agent's `max_seconds` can end the wait; the
/// request itself ends at the backend's `timeout_seconds`.
pub fn run(
    agent: &AgentConfig,
    la: &DirectAdapter,
    backend: &LlmBackend,
    task: &str,
    context: Option<&str>,
    progress: &dyn Fn(&str),
    cancel: &AtomicBool,
) -> Result<AgentRun, Error> {
    let (path, body) = request_for(backend, agent, la, task, context);
    let backend = backend.clone();
    progress(&format!(
        "{} · asking {} on {}",
        agent.name, la.model, backend.name
    ));
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(send(&backend, path, &body));
    });
    let deadline = Instant::now() + Duration::from_secs(u64::from(agent.max_seconds));
    let answer = loop {
        let why = if cancel.load(Ordering::Relaxed) {
            Some("cancelled".to_string())
        } else if Instant::now() >= deadline {
            Some(format!("stopped at the {}s time limit", agent.max_seconds))
        } else {
            None
        };
        if let Some(why) = why {
            return Ok(AgentRun {
                text: format!("[{why} before the model answered; the work is unfinished]"),
                ..empty(&la.model)
            });
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(answer) => break answer?,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Error::Protocol {
                    backend: DIRECT_AGENT.into(),
                    detail: "the request ended without an answer".into(),
                });
            }
        }
    };
    progress(&format!("{} · answered", agent.name));
    Ok(answer)
}

fn empty(model: &str) -> AgentRun {
    AgentRun {
        text: String::new(),
        model: model.to_string(),
        usage: Usage::default(),
        steps: 0,
        tool_calls: 0,
        changes: Changes::new(),
        commands: Vec::new(),
    }
}

fn send(backend: &LlmBackend, path: &str, body: &Value) -> Result<AgentRun, Error> {
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
fn parse_anthropic(backend: &str, model: &str, status: u16, body: &str) -> Result<AgentRun, Error> {
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
    Ok(AgentRun {
        text,
        model: json["model"].as_str().unwrap_or(model).to_string(),
        usage: Usage {
            prompt_tokens: tokens("input_tokens"),
            completion_tokens: tokens("output_tokens"),
        },
        steps: 1,
        ..empty(model)
    })
}

fn parse(backend: &str, model: &str, status: u16, body: &str) -> Result<AgentRun, Error> {
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
    Ok(AgentRun {
        text,
        model: json["model"].as_str().unwrap_or(model).to_string(),
        usage: Usage {
            prompt_tokens: tokens("prompt_tokens"),
            completion_tokens: tokens("completion_tokens"),
        },
        steps: 1,
        ..empty(model)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Adapter, Budget, Isolation};
    use std::io::{BufRead, BufReader, Read, Write};

    fn agent(prompt: Option<&str>, max_seconds: u32) -> (AgentConfig, DirectAdapter) {
        let la = DirectAdapter {
            backend: "local".into(),
            model: "small".into(),
            max_tokens: 200,
        };
        let a = AgentConfig {
            name: "summariser".into(),
            prompt: prompt.map(String::from),
            max_seconds,
            max_concurrent: 1,
            isolation: Isolation::Shared,
            budget: Budget::Unlimited,
            limit: None,
            adapter: Adapter::Direct(la.clone()),
        };
        (a, la)
    }

    /// A server that answers one request with `status` and `body` after
    /// `delay`, and hands back the request it got.
    fn server(status: u16, body: &str, delay: Duration) -> (LlmBackend, mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = body.to_string();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(&stream);
            let mut request = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
                request.push_str(&line);
            }
            request.push_str("\r\n");
            let mut content = vec![0; length];
            let _ = reader.read_exact(&mut content);
            request.push_str(&String::from_utf8_lossy(&content));
            let _ = tx.send(request);
            std::thread::sleep(delay);
            let mut stream = &stream;
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        });
        let backend = LlmBackend {
            name: "local".into(),
            timeout_seconds: 20,
            settings: BackendSettings::OpenAiCompatible {
                base_url: format!("http://127.0.0.1:{port}/v1"),
                api_key: Some("sk-test".into()),
            },
        };
        (backend, rx)
    }

    #[test]
    fn the_task_goes_as_one_chat_request_and_the_answer_comes_back() {
        let (a, la) = agent(Some("You summarise."), 30);
        let (backend, seen) = server(
            200,
            r#"{"model":"small-q4","choices":[{"message":{"content":" short "},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":3}}"#,
            Duration::ZERO,
        );
        let r = run(
            &a,
            &la,
            &backend,
            "Summarise it.",
            Some("A long text."),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            (r.text.as_str(), r.model.as_str(), r.steps),
            ("short", "small-q4", 1)
        );
        assert_eq!((r.usage.prompt_tokens, r.usage.completion_tokens), (12, 3));
        let request = seen.recv().unwrap();
        assert!(
            request.starts_with("POST /v1/chat/completions"),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer sk-test"),
            "{request}"
        );
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"model": "small", "max_tokens": 200, "messages": [
                {"role": "system", "content": "You summarise."},
                {"role": "user", "content": "Context:\nA long text.\n\nSummarise it."}
            ]})
        );
    }

    #[test]
    fn a_cut_off_answer_says_so_and_a_refusal_is_an_api_error() {
        let (a, la) = agent(None, 30);
        let (backend, _) = server(
            200,
            r#"{"choices":[{"message":{"content":"half"},"finish_reason":"length"}]}"#,
            Duration::ZERO,
        );
        let r = run(
            &a,
            &la,
            &backend,
            "t",
            None,
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            r.text,
            "half\n[cut off at max_tokens; the answer is unfinished]"
        );
        assert_eq!(r.model, "small");

        let (backend, _) = server(503, "loading", Duration::ZERO);
        let e = run(
            &a,
            &la,
            &backend,
            "t",
            None,
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(matches!(e, Error::Api { status: 503, .. }), "{e:?}");
    }

    fn anthropic(backend: LlmBackend, key: Option<&str>) -> LlmBackend {
        let url = backend
            .settings
            .base_url()
            .trim_end_matches("/v1")
            .to_string();
        LlmBackend {
            settings: BackendSettings::Anthropic {
                base_url: url,
                api_key: key.map(String::from),
            },
            ..backend
        }
    }

    #[test]
    fn an_anthropic_backend_gets_a_messages_request_with_its_key_and_version() {
        let (a, la) = agent(Some("You summarise."), 30);
        let reply = r#"{"model":"claude-haiku-4-5","content":[{"type":"text","text":"three"},{"type":"text","text":"bullets"}],"stop_reason":"end_turn","usage":{"input_tokens":11,"output_tokens":3}}"#;
        let (backend, request) = server(200, reply, Duration::ZERO);
        let backend = anthropic(backend, Some("sk-ant"));
        let r = run(
            &a,
            &la,
            &backend,
            "summarise",
            Some("ctx"),
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(r.text, "three\nbullets");
        assert_eq!(r.model, "claude-haiku-4-5");
        assert_eq!((r.usage.prompt_tokens, r.usage.completion_tokens), (11, 3));
        let sent = request.recv().unwrap();
        assert!(sent.starts_with("POST /v1/messages "), "{sent}");
        let lower = sent.to_ascii_lowercase();
        assert!(lower.contains("x-api-key: sk-ant"), "{sent}");
        assert!(lower.contains("anthropic-version: 2023-06-01"), "{sent}");
        assert!(!lower.contains("authorization"), "{sent}");
        let body: Value = serde_json::from_str(sent.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["system"], "You summarise.");
        assert_eq!(body["max_tokens"], 200);
        assert_eq!(body["messages"][0]["content"], "Context:\nctx\n\nsummarise");
    }

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
    fn the_request_for_each_backend_kind_names_its_path() {
        let (a, la) = agent(None, 30);
        let (open, _) = server(200, "{}", Duration::ZERO);
        assert_eq!(
            request_for(&open, &a, &la, "t", None).0,
            "/chat/completions"
        );
        let claude = anthropic(open, None);
        let (path, body) = request_for(&claude, &a, &la, "t", None);
        assert_eq!(path, "/v1/messages");
        assert!(body.get("system").is_none(), "no prompt, no system: {body}");
    }

    #[test]
    fn the_time_limit_ends_the_wait() {
        let (a, la) = agent(None, 1);
        let (backend, _) = server(200, "{}", Duration::from_secs(5));
        let started = Instant::now();
        let r = run(
            &a,
            &la,
            &backend,
            "t",
            None,
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(
            r.text.starts_with("[stopped at the 1s time limit"),
            "{}",
            r.text
        );
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
