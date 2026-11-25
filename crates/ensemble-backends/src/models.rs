//! Asking a provider of the ANN pool which models it serves, and what each is.
//!
//! A harness calls its model itself. Ensemble asks a provider only what it
//! serves: `GET /models` is the cheapest request that proves the server is up
//! and the key is accepted, it spends no tokens, and its answer says whether
//! a model a person adds, or added, is really there.

use crate::catalog;
use ensemble_config::{BackendSettings, LlmBackend, ModelKind, Tier};
use ensemble_core::port::Error;
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
        BackendSettings::Decision { .. } => {
            let req = agent.get(format!("{base_url}/v1/models"));
            match api_key {
                Some(key) => req.header("authorization", &format!("Bearer {key}")),
                None => req,
            }
        }
        BackendSettings::Anthropic { .. } => {
            let req = agent
                .get(format!("{base_url}/v1/models"))
                .header("anthropic-version", crate::chat::ANTHROPIC_VERSION);
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

/// A model a provider lists, with what it is as far as the provider says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub id: String,
    /// None: the provider did not say whether it is an LLM or an embedding
    /// model.
    pub kind: Option<ModelKind>,
}

/// The models `backend` serves, each with what it is: Anthropic's are LLMs,
/// a decision model's API's are decision models, Ollama says what each of
/// its can do, and of another OpenAI-compatible
/// server only the LLMs of a family Ensemble knows are known before one is
/// added.
pub fn discover(backend: &LlmBackend) -> Result<Vec<Found>, Error> {
    let ids = models(backend)?;
    let ollama = match &backend.settings {
        BackendSettings::OpenAiCompatible { base_url, .. } => ollama_at(base_url),
        BackendSettings::Anthropic { .. } | BackendSettings::Decision { .. } => None,
    };
    Ok(ids
        .into_iter()
        .map(|id| {
            let kind = match (&backend.settings, &ollama) {
                (BackendSettings::Anthropic { .. }, _) => Some(ModelKind::Llm {
                    tier: catalog::tier(&id).unwrap_or(Tier::Medium),
                    context_window: None,
                    tools: Some(true),
                }),
                (BackendSettings::Decision { .. }, _) => Some(ModelKind::Decision),
                (_, Some(origin)) => ollama_kind(origin, &id),
                (_, None) => catalog::family(&id).map(|tier| ModelKind::Llm {
                    tier,
                    context_window: None,
                    tools: None,
                }),
            };
            Found { id, kind }
        })
        .collect())
}

/// An LLM `id` whose provider did not say what it is, as Ensemble takes it.
pub fn llm(id: &str) -> ModelKind {
    ModelKind::Llm {
        tier: catalog::tier(id).unwrap_or(Tier::Medium),
        context_window: None,
        tools: None,
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(PROBE_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into()
}

/// Where Ollama's own API is, when the server at `base_url` is Ollama: its
/// origin, without `/v1`, where `/api/version` answers.
fn ollama_at(base_url: &str) -> Option<String> {
    let origin = base_url.trim_end_matches('/').trim_end_matches("/v1");
    let mut resp = agent().get(format!("{origin}/api/version")).call().ok()?;
    if resp.status().as_u16() != 200 {
        return None;
    }
    let body: serde_json::Value =
        serde_json::from_str(&resp.body_mut().read_to_string().ok()?).ok()?;
    body.get("version")?;
    Some(origin.to_string())
}

/// What Ollama at `origin` says the model `id` is.
fn ollama_kind(origin: &str, id: &str) -> Option<ModelKind> {
    let shown = agent()
        .post(format!("{origin}/api/show"))
        .header("content-type", "application/json")
        .send(serde_json::json!({ "model": id }).to_string())
        .ok()
        .filter(|r| r.status().as_u16() == 200)
        .and_then(|mut r| r.body_mut().read_to_string().ok())
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    ollama_says(id, &shown?)
}

/// What a model is, by what Ollama's `/api/show` answered for it.
fn ollama_says(id: &str, shown: &serde_json::Value) -> Option<ModelKind> {
    let can = |what: &str| {
        shown["capabilities"]
            .as_array()
            .is_some_and(|c| c.iter().any(|c| c == what))
    };
    // Its facts are under its architecture's name, such as
    // `qwen3.context_length`.
    let info = |key: &str| {
        shown["model_info"].as_object().and_then(|info| {
            info.iter()
                .find(|(k, _)| k.ends_with(key))
                .and_then(|(_, v)| v.as_u64())
                .and_then(|n| u32::try_from(n).ok())
        })
    };
    if can("embedding") {
        return info(".embedding_length")
            .filter(|d| *d > 0)
            .map(|dimensions| ModelKind::Embedding { dimensions });
    }
    if !can("completion") {
        return None;
    }
    let size = shown["details"]["parameter_size"]
        .as_str()
        .and_then(catalog::billions)
        .map(catalog::by_size);
    Some(ModelKind::Llm {
        tier: catalog::family(id)
            .or(size)
            .or_else(|| catalog::tier(id))
            .unwrap_or(Tier::Medium),
        context_window: info(".context_length").filter(|n| *n > 0),
        tools: Some(can("tools")),
    })
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
    // OpenAI's `data` of objects with an `id`; a decision model's API may say
    // `models`, of names or of objects with an `id` or a `name`.
    let data = json
        .get("data")
        .or_else(|| json.get("models"))
        .and_then(|d| d.as_array())
        .ok_or_else(|| Error::Protocol {
            backend: backend.into(),
            detail: "/models has no `data` list".into(),
        })?;
    Ok(data
        .iter()
        .filter_map(|m| {
            m.as_str()
                .or_else(|| m.get("id").and_then(|id| id.as_str()))
                .or_else(|| m.get("name").and_then(|n| n.as_str()))
                .map(String::from)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_says_what_each_model_can_do() {
        let chat = serde_json::json!({
            "capabilities": ["completion", "tools"],
            "details": { "parameter_size": "72.7B" },
            "model_info": { "qwen2.context_length": 32768 },
        });
        assert_eq!(
            ollama_says("my-qwen", &chat),
            Some(ModelKind::Llm {
                tier: Tier::Strong,
                context_window: Some(32768),
                tools: Some(true),
            })
        );
        let embedding = serde_json::json!({
            "capabilities": ["embedding"],
            "model_info": { "bert.embedding_length": 768 },
        });
        assert_eq!(
            ollama_says("nomic-embed-text", &embedding),
            Some(ModelKind::Embedding { dimensions: 768 })
        );
        assert_eq!(ollama_says("x", &serde_json::json!({})), None);
    }

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
    fn a_decision_model_api_may_list_its_models_by_name() {
        let ids = parse(
            "laya",
            200,
            r#"{"models":["laya",{"name":"laya-multilingual"}]}"#,
        )
        .unwrap();
        assert_eq!(ids, ["laya", "laya-multilingual"]);
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

        let (url, request) = answering(r#"{"data":[{"id":"jev-latest"}]}"#);
        let decision = backend(BackendSettings::Decision {
            base_url: url,
            api_key: Some("ts".into()),
        });
        let found = discover(&decision).unwrap();
        assert_eq!(
            found,
            [Found {
                id: "jev-latest".into(),
                kind: Some(ModelKind::Decision)
            }]
        );
        let sent = request.recv().unwrap().to_ascii_lowercase();
        assert!(sent.starts_with("get /v1/models "), "{sent}");
        assert!(sent.contains("authorization: bearer ts"), "{sent}");
    }
}
