//! Asking an LLM backend which models it serves.
//!
//! An agent's harness calls its model; only a light `llm` agent is a request
//! Ensemble makes itself, in `external::llm`. This is
//! only the health check: `GET /models` is the cheapest request that proves
//! the server is up and the key is accepted, it spends no tokens, and its
//! answer says whether the model an agent names is really there.

use crate::config::{BackendSettings, LlmBackend};
use crate::port::Error;
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// The ids of the models `backend` serves.
pub fn models(backend: &LlmBackend) -> Result<Vec<String>, Error> {
    let (base_url, api_key) = (backend.settings.base_url(), backend.settings.api_key());
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(PROBE_TIMEOUT))
        // Hand back a non-2xx as a response: its body says why ("model is
        // still loading"), and ureq would otherwise discard it.
        .http_status_as_error(false)
        .build()
        .into();
    let req = match &backend.settings {
        BackendSettings::OpenAiCompatible { .. } => {
            let req = agent.get(format!("{base_url}/models"));
            match api_key {
                Some(key) => req.header("authorization", &format!("Bearer {key}")),
                None => req,
            }
        }
        BackendSettings::Anthropic { .. } => {
            let req = agent
                .get(format!("{base_url}/v1/models"))
                .header("anthropic-version", crate::external::llm::ANTHROPIC_VERSION);
            match api_key {
                Some(key) => req.header("x-api-key", key),
                None => req,
            }
        }
    };
    let mut resp = req
        .call()
        .map_err(|e| transport(&backend.name, e.to_string()))?;
    let status = resp.status().as_u16();
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    parse(&backend.name, status, &body)
}

/// A read that ran out of time is a timeout, not a generic transport failure.
fn transport(backend: &str, detail: String) -> Error {
    let lower = detail.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        Error::Timeout {
            backend: backend.into(),
            seconds: PROBE_TIMEOUT.as_secs(),
        }
    } else {
        Error::Transport {
            backend: backend.into(),
            detail,
        }
    }
}

fn parse(backend: &str, status: u16, body: &str) -> Result<Vec<String>, Error> {
    if !(200..300).contains(&status) {
        return Err(Error::Api {
            backend: backend.into(),
            status,
            message: body.to_string(),
        });
    }
    let json: serde_json::Value = serde_json::from_str(body).map_err(|e| Error::Protocol {
        backend: backend.into(),
        detail: format!("/models is not JSON: {e}"),
    })?;
    let data = json
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| Error::Protocol {
            backend: backend.into(),
            detail: "/models has no `data` list".into(),
        })?;
    Ok(data
        .iter()
        .filter_map(|m| m.get("id").and_then(|id| id.as_str()).map(String::from))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_ids_are_read_from_data() {
        let ids = parse(
            "local",
            200,
            r#"{"object":"list","data":[{"id":"qwen3.5-9b"},{"id":"other"}]}"#,
        )
        .unwrap();
        assert_eq!(ids, ["qwen3.5-9b", "other"]);
    }

    #[test]
    fn a_failed_status_keeps_its_body() {
        let err = parse("local", 503, "model is loading").unwrap_err();
        assert!(err.to_string().contains("503"), "{err}");
        assert!(err.to_string().contains("model is loading"), "{err}");
    }

    #[test]
    fn an_unexpected_shape_is_a_protocol_error() {
        assert!(matches!(
            parse("local", 200, "{}").unwrap_err(),
            Error::Protocol { .. }
        ));
        assert!(matches!(
            parse("local", 200, "<html>").unwrap_err(),
            Error::Protocol { .. }
        ));
    }

    #[test]
    fn a_refused_connection_is_a_transport_error_and_a_slow_one_a_timeout() {
        assert!(matches!(
            transport("b", "Connection refused".into()),
            Error::Transport { .. }
        ));
        assert!(matches!(
            transport("b", "timed out reading response".into()),
            Error::Timeout { .. }
        ));
    }

    /// A server that answers one request with `body`, and hands back what it got.
    fn answering(body: &str) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let body = body.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(&stream);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                    break;
                }
                request.push_str(&line);
            }
            let _ = tx.send(request);
            let mut out = &stream;
            let _ = write!(
                out,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        });
        (url, rx)
    }

    #[test]
    fn each_kind_of_backend_is_asked_for_its_models_its_own_way() {
        let backend = |settings| LlmBackend {
            name: "b".into(),
            timeout_seconds: 5,
            settings,
        };
        let (url, request) = answering(r#"{"data":[{"id":"claude-haiku-4-5"}]}"#);
        let ids = models(&backend(BackendSettings::Anthropic {
            base_url: url,
            api_key: Some("sk-ant".into()),
        }))
        .unwrap();
        assert_eq!(ids, ["claude-haiku-4-5"]);
        let sent = request.recv().unwrap().to_ascii_lowercase();
        assert!(sent.starts_with("get /v1/models "), "{sent}");
        assert!(
            sent.contains("x-api-key: sk-ant") && sent.contains("anthropic-version: 2023-06-01"),
            "{sent}"
        );

        let (url, request) = answering(r#"{"data":[{"id":"qwen"}]}"#);
        let ids = models(&backend(BackendSettings::OpenAiCompatible {
            base_url: format!("{url}/v1"),
            api_key: Some("k".into()),
        }))
        .unwrap();
        assert_eq!(ids, ["qwen"]);
        let sent = request.recv().unwrap().to_ascii_lowercase();
        assert!(sent.starts_with("get /v1/models "), "{sent}");
        assert!(
            sent.contains("authorization: bearer k") && !sent.contains("x-api-key"),
            "{sent}"
        );
    }
}
