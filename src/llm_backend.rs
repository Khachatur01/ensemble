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
    let BackendSettings::OpenAiCompatible {
        base_url, api_key, ..
    } = &backend.settings;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(PROBE_TIMEOUT))
        // Hand back a non-2xx as a response: its body says why ("model is
        // still loading"), and ureq would otherwise discard it.
        .http_status_as_error(false)
        .build()
        .into();
    let mut req = agent.get(format!("{base_url}/models"));
    if let Some(key) = api_key {
        req = req.header("authorization", &format!("Bearer {key}"));
    }
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
}
