//! Asking a decision model: a state and typed questions in, typed answers
//! with their probabilities out, over `POST /v1/systemone`. TypeSafe's Jev
//! and Laya's `laya-serve` answer the same request, so one client serves
//! both: a `decision-api` provider of the hub's ANN pool.
//!
//! A question is one of three types: a `choice` among named options, each
//! with a description; a `score`, one of an ordered list of levels; or a
//! `noul`, yes or no. The options are given with each request, so they may
//! change from one to the next. What an answer carries besides is ignored:
//! Laya adds its own fields about routing.

use crate::chat::post;
use ensemble_config::LlmBackend;
use ensemble_core::port::Error;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Where the questions are asked, after the provider's `base_url`.
pub const PATH: &str = "/v1/systemone";

/// One question, as it is asked.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    /// One of `criteria`, each option's name with what it means.
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
    /// One of the levels of `criteria`, lowest first.
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
    /// Yes or no.
    Noul { instructions: String },
}

/// One answer, as it comes back.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Choice {
        /// The option chosen.
        choice: String,
        /// From 0 to 1, by how the probabilities are spread.
        confidence: f64,
        /// Each option's.
        probabilities: BTreeMap<String, f64>,
    },
    Score {
        /// The expected level, weighed by the probabilities.
        score: f64,
        confidence: f64,
        /// Each level's, by its number from 0.
        probabilities: BTreeMap<String, f64>,
    },
    Noul {
        /// The probability of yes, from 0 to 1.
        noul: f64,
    },
}

/// What a decision model answered.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Answers {
    /// The model that answered, as it names itself.
    pub model: String,
    /// Each question's answer, by the question's name.
    pub answers: BTreeMap<String, Answer>,
}

/// Ask the model `model` of `provider` the `questions` about `state`. It waits
/// at most the provider's `timeout_seconds`.
pub fn decide(
    provider: &LlmBackend,
    model: &str,
    state: &Value,
    questions: &BTreeMap<String, Question>,
) -> Result<Answers, Error> {
    let body = serde_json::json!({ "model": model, "state": state, "questions": questions });
    let (status, text) = post(provider, PATH, &body)?;
    parse(&provider.name, status, &text)
}

fn parse(provider: &str, status: u16, body: &str) -> Result<Answers, Error> {
    if !(200..300).contains(&status) {
        return Err(Error::Api {
            backend: provider.into(),
            status,
            message: body.to_string(),
        });
    }
    serde_json::from_str(body).map_err(|e| Error::Protocol {
        backend: provider.into(),
        detail: format!("{PATH} answered what is not its answers: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_are_asked_by_their_type() {
        let asked = serde_json::to_value(BTreeMap::from([
            (
                "work".to_string(),
                Question::Choice {
                    instructions: "Does it change files?".into(),
                    criteria: BTreeMap::from([
                        ("change".into(), "It changes the repository.".into()),
                        ("answer".into(), "It only needs an answer.".into()),
                    ]),
                },
            ),
            (
                "long".to_string(),
                Question::Noul {
                    instructions: "Does it need a long context?".into(),
                },
            ),
            (
                "how".to_string(),
                Question::Score {
                    instructions: "How hard is it?".into(),
                    criteria: vec!["easy".into(), "hard".into()],
                },
            ),
        ]))
        .unwrap();
        assert_eq!(
            asked,
            serde_json::json!({
                "work": {"type": "choice", "instructions": "Does it change files?",
                         "criteria": {"answer": "It only needs an answer.", "change": "It changes the repository."}},
                "long": {"type": "noul", "instructions": "Does it need a long context?"},
                "how": {"type": "score", "instructions": "How hard is it?", "criteria": ["easy", "hard"]},
            })
        );
    }

    #[test]
    fn answers_are_read_and_what_else_comes_with_them_is_left() {
        let read = parse(
            "laya",
            200,
            r#"{"model": "laya", "usage": {"input_tokens": 120, "output_tokens": 0},
                "answers": {
                  "work": {"type": "choice", "choice": "change", "confidence": 0.7,
                           "probabilities": {"change": 0.85, "answer": 0.15},
                           "route": "auto", "abstain": false},
                  "long": {"type": "noul", "noul": 0.2},
                  "how": {"type": "score", "score": 0.4, "confidence": 0.5,
                          "probabilities": {"0": 0.6, "1": 0.4}, "legend": {"0": "easy", "1": "hard"}}},
                "routing": {"engine": "vega"}}"#,
        )
        .unwrap();
        assert_eq!(read.model, "laya");
        assert_eq!(
            read.answers["work"],
            Answer::Choice {
                choice: "change".into(),
                confidence: 0.7,
                probabilities: BTreeMap::from([("answer".into(), 0.15), ("change".into(), 0.85)]),
            }
        );
        assert_eq!(read.answers["long"], Answer::Noul { noul: 0.2 });
        assert!(matches!(read.answers["how"], Answer::Score { score, .. } if score == 0.4));
    }

    #[test]
    fn a_refusal_keeps_its_body_and_a_strange_answer_is_a_protocol_error() {
        let err = parse("jev", 422, r#"{"detail": "no questions"}"#).unwrap_err();
        assert!(err.to_string().contains("no questions"), "{err}");
        assert!(matches!(
            parse("jev", 200, r#"{"model": "jev"}"#).unwrap_err(),
            Error::Protocol { .. }
        ));
    }
}
