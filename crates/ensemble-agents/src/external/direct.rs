//! The `direct` adapter: a light agent that is one chat request to an LLM
//! backend, with no CLI in between. No tools: the model answers from the task,
//! its context and the agent's prompt, so it suits cheap work that needs no
//! files, such as summarising, classifying or drafting from what it is given.
//! The request itself is `ensemble_backends::chat`; this is the agent around it.

use super::AgentRun;
use crate::runtime::until_set;
use ensemble_backends::chat::{self, Ask};
use ensemble_config::{AgentConfig, DIRECT_AGENT, DirectAdapter, LlmBackend};
use ensemble_core::port::Error;
use ensemble_core::workspace::Changes;
use serde_json::Value;
use std::sync::atomic::AtomicBool;

use std::time::Duration;

/// The one user message: the task, after its context when there is one.
fn user_message(task: &str, context: Option<&str>) -> String {
    match context.filter(|c| !c.trim().is_empty()) {
        Some(ctx) => format!("Context:\n{ctx}\n\n{task}"),
        None => task.to_string(),
    }
}

fn ask<'a>(agent: &'a AgentConfig, la: &'a DirectAdapter, user: &'a str) -> Ask<'a> {
    Ask {
        model: la.model(),
        system: agent.prompt.as_deref(),
        user,
        max_tokens: la.max_tokens,
    }
}

/// The request for `backend`: the path after its `base_url`, and the body.
pub fn request_for(
    backend: &LlmBackend,
    agent: &AgentConfig,
    la: &DirectAdapter,
    task: &str,
    context: Option<&str>,
) -> (&'static str, Value) {
    let user = user_message(task, context);
    chat::request(backend, &ask(agent, la, &user))
}

/// The request, as it is sent to `{base_url}/chat/completions`.
pub fn request(
    agent: &AgentConfig,
    la: &DirectAdapter,
    task: &str,
    context: Option<&str>,
) -> Value {
    let user = user_message(task, context);
    chat::openai_body(&ask(agent, la, &user))
}

/// Send the task and return the answer. A cancel or the agent's
/// `max_seconds` ends the wait; the request itself ends at the backend's
/// `timeout_seconds`.
pub async fn run(
    agent: &AgentConfig,
    la: &DirectAdapter,
    backend: &LlmBackend,
    task: &str,
    context: Option<&str>,
    progress: &(dyn Fn(&str) + Sync),
    cancel: &AtomicBool,
) -> Result<AgentRun, Error> {
    let (path, body) = request_for(backend, agent, la, task, context);
    let backend = backend.clone();
    progress(&format!(
        "{} · asking {} on {}",
        agent.name,
        la.model(),
        backend.name
    ));
    let request = tokio::task::spawn_blocking(move || chat::send(&backend, path, &body));
    let limit = Duration::from_secs(u64::from(agent.max_seconds));
    let answer = tokio::select! {
        answer = request => answer.map_err(|e| Error::Protocol {
            backend: DIRECT_AGENT.into(),
            detail: format!("the request ended without an answer: {e}"),
        })??,
        () = tokio::time::sleep(limit) => return Ok(unfinished(
            &format!("stopped at the {}s time limit", agent.max_seconds),
            la.model(),
        )),
        () = until_set(cancel) => return Ok(unfinished("cancelled", la.model())),
    };
    progress(&format!("{} · answered", agent.name));
    Ok(AgentRun {
        text: answer.text,
        model: answer.model,
        usage: answer.usage,
        steps: 1,
        ..empty(la.model())
    })
}

fn unfinished(why: &str, model: &str) -> AgentRun {
    AgentRun {
        text: format!("[{why} before the model answered; the work is unfinished]"),
        ..empty(model)
    }
}

fn empty(model: &str) -> AgentRun {
    AgentRun {
        text: String::new(),
        model: model.to_string(),
        usage: Default::default(),
        steps: 0,
        tool_calls: 0,
        changes: Changes::new(),
        commands: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn run(
        agent: &AgentConfig,
        la: &DirectAdapter,
        backend: &LlmBackend,
        task: &str,
        context: Option<&str>,
        progress: &(dyn Fn(&str) + Sync),
        cancel: &AtomicBool,
    ) -> Result<AgentRun, Error> {
        crate::runtime::block_on(super::run(
            agent, la, backend, task, context, progress, cancel,
        ))
    }
    use ensemble_config::{
        Adapter, BackendSettings, Budget, Candidate, Fallback, Isolation, WhenNone,
    };
    use serde_json::json;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::mpsc;
    use std::time::Instant;

    fn agent(prompt: Option<&str>, max_seconds: u32) -> (AgentConfig, DirectAdapter) {
        let la = DirectAdapter {
            fallback: Fallback {
                candidates: vec![Candidate {
                    provider: "local".into(),
                    model: "small".into(),
                    context_window: None,
                }],
                when_none: WhenNone::Fail,
            },
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
