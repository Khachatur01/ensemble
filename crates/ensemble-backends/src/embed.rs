//! Embeddings: texts as vectors, from an OpenAI-compatible `/embeddings`, so
//! that texts which mean the same are found close together.

use crate::chat::post;
use ensemble_config::{BackendSettings, LlmBackend};
use ensemble_core::port::Error;
use serde_json::{Value, json};

/// The vectors of `texts`, in their order, from `model` on `backend`.
pub fn embed(backend: &LlmBackend, model: &str, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
    if !matches!(backend.settings, BackendSettings::OpenAiCompatible { .. }) {
        return Err(Error::Unsupported {
            backend: backend.name.clone(),
            detail: "only an openai-compatible-api provider serves embeddings".into(),
        });
    }
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let (status, text) = post(
        backend,
        "/embeddings",
        &json!({ "model": model, "input": texts }),
    )?;
    if !(200..300).contains(&status) {
        return Err(Error::Api {
            backend: backend.name.clone(),
            status,
            message: text,
        });
    }
    let protocol = |detail: String| Error::Protocol {
        backend: backend.name.clone(),
        detail,
    };
    let json: Value = serde_json::from_str(&text)
        .map_err(|e| protocol(format!("the answer is not JSON: {e}")))?;
    let data = json["data"]
        .as_array()
        .ok_or_else(|| protocol("the answer has no `data` list".into()))?;
    let mut vectors: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
    for (i, item) in data.iter().enumerate() {
        let index = item["index"].as_u64().map_or(i, |n| n as usize);
        let vector: Vec<f32> = item["embedding"]
            .as_array()
            .ok_or_else(|| protocol(format!("data[{i}] has no `embedding`")))?
            .iter()
            .map(|x| x.as_f64().map(|x| x as f32))
            .collect::<Option<_>>()
            .ok_or_else(|| protocol(format!("data[{i}].embedding is not numbers")))?;
        match vectors.get_mut(index) {
            Some(slot) => *slot = Some(vector),
            None => {
                return Err(protocol(format!(
                    "data[{i}] has index {index}, past the input"
                )));
            }
        }
    }
    vectors
        .into_iter()
        .enumerate()
        .map(|(i, v)| v.ok_or_else(|| protocol(format!("no embedding for input {i}"))))
        .collect()
}

/// How alike two vectors point, from -1 to 1; 0 when either is empty or of
/// another length.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut aa, mut bb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        aa += x * x;
        bb += y * y;
    }
    if aa == 0.0 || bb == 0.0 {
        0.0
    } else {
        dot / (aa.sqrt() * bb.sqrt())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::embedding_server;
    use ensemble_config::BackendSettings;

    fn backend(url: &str) -> LlmBackend {
        LlmBackend {
            name: "local".into(),
            timeout_seconds: 5,
            settings: BackendSettings::OpenAiCompatible {
                base_url: url.into(),
                api_key: None,
            },
        }
    }

    #[test]
    fn texts_come_back_as_vectors_in_their_order() {
        let server = embedding_server(&[("deploy", &[1.0, 0.0]), ("release", &[0.9, 0.1])]);
        let texts = vec!["release day".to_string(), "deploy day".to_string()];
        let vectors = embed(&backend(&server.url), "e", &texts).unwrap();
        assert_eq!(vectors, vec![vec![0.9, 0.1], vec![1.0, 0.0]]);
        let asked: Value = serde_json::from_str(&server.requests.lock().unwrap()[0]).unwrap();
        assert_eq!(asked["model"], "e");
        assert_eq!(asked["input"][1], "deploy day");
    }

    #[test]
    fn alike_vectors_are_close_and_unalike_far() {
        assert!((cosine(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }
}
