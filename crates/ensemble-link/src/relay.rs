//! Model calls relayed through the hub: the frames of a runner's second
//! WebSocket, at [`PATH`], beside its link. A call to a model another runner
//! shares goes from the caller to the hub, from the hub to the runner that
//! serves it, which calls its own source, and its answer streams back the
//! same way. The hub calls runners' models on this path too, from its side.
//!
//! The runner says what it shares first ([`Offer`]), and the hub answers with
//! its name as the hub knows it, or why it refuses, and closes. Then each
//! side may start calls: a runner to reach others' models, the hub to have
//! the runner serve one of its own. Every call is numbered by whoever started
//! it, and its frames go one way or the other:
//!
//! - from the caller: [`Frame::Open`], the request's body in [`Frame::Body`]
//!   chunks, then [`Frame::End`]; or [`Frame::Cancel`] once it is no longer
//!   wanted;
//! - from whoever serves it: [`Frame::Head`] (the status and type of the
//!   answer), its body in [`Frame::Reply`] chunks, then [`Frame::Done`]; or
//!   [`Frame::Error`] at any point.
//!
//! The way a frame goes says whose number it carries, so the two never mix.
//! Control frames are JSON text; body chunks are binary: a byte for which way
//! they go, the call's number in eight bytes, then the bytes.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
pub use tokio_tungstenite::tungstenite::Message;

/// The path of a runner's relay socket on the hub's port.
pub const PATH: &str = "/relay";
/// How long a call waits for a runner that serves as many calls as it takes
/// at once, before it is answered busy.
pub const BUSY_WAIT: Duration = Duration::from_secs(10);
/// The most of a body sent in one frame.
pub const CHUNK: usize = 64 * 1024;
/// The status of an answer that says the model cannot be reached now: its
/// runner is not connected, or busy, or the call may not use it. The caller
/// tries the next model of its list.
pub const UNAVAILABLE: u16 = 503;

/// What a runner says first on its relay socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    /// Its name, which the hub knows it by as `<owner>/<name>`.
    pub name: String,
    /// The sources of its own it shares models of, by name.
    pub sources: std::collections::BTreeMap<String, Shared>,
}

/// One source a runner shares models of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shared {
    /// The most calls to it at once, the runner's own and others' together;
    /// the hub keeps others' waiting for a place among them.
    pub max_concurrent: u32,
    /// The models of it shared.
    pub models: Vec<SharedModel>,
}

/// A model a runner shares, and what it is, as its settings say: a model's
/// `kind` as the ANN pool keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedModel {
    pub id: String,
    pub kind: Value,
}

/// What a call asks for: a request to the model of `source` of `runner`
/// the request's body names, at `path` of the source's base URL. With no
/// source, it asks the runner which models it shares: `GET /models`, which
/// the hub answers itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Open {
    /// `<owner>/<name>`, as the hub knows the runner.
    pub runner: String,
    #[serde(default)]
    pub source: String,
    pub method: String,
    /// Such as `/chat/completions`.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Open {
        call: u64,
        open: Open,
    },
    Body {
        call: u64,
        bytes: Vec<u8>,
    },
    End {
        call: u64,
    },
    Cancel {
        call: u64,
    },
    Head {
        call: u64,
        status: u16,
        content_type: String,
    },
    Reply {
        call: u64,
        bytes: Vec<u8>,
    },
    Done {
        call: u64,
    },
    Error {
        call: u64,
        status: u16,
        message: String,
    },
}

/// The first byte of a binary frame: which way its bytes go.
const TO_SERVER: u8 = 0;
const TO_CALLER: u8 = 1;

impl Frame {
    /// The number of the call it is about.
    pub fn call(&self) -> u64 {
        match self {
            Frame::Open { call, .. }
            | Frame::Body { call, .. }
            | Frame::End { call }
            | Frame::Cancel { call }
            | Frame::Head { call, .. }
            | Frame::Reply { call, .. }
            | Frame::Done { call }
            | Frame::Error { call, .. } => *call,
        }
    }

    /// The same frame about call `call`.
    pub fn renumbered(mut self, to: u64) -> Frame {
        match &mut self {
            Frame::Open { call, .. }
            | Frame::Body { call, .. }
            | Frame::End { call }
            | Frame::Cancel { call }
            | Frame::Head { call, .. }
            | Frame::Reply { call, .. }
            | Frame::Done { call }
            | Frame::Error { call, .. } => *call = to,
        }
        self
    }

    /// Does it go from the caller to whoever serves the call?
    pub fn to_server(&self) -> bool {
        matches!(
            self,
            Frame::Open { .. } | Frame::Body { .. } | Frame::End { .. } | Frame::Cancel { .. }
        )
    }

    /// Is it the last frame of an answer?
    pub fn ends_answer(&self) -> bool {
        matches!(self, Frame::Done { .. } | Frame::Error { .. })
    }

    pub fn error(call: u64, status: u16, message: impl Into<String>) -> Frame {
        Frame::Error {
            call,
            status,
            message: message.into(),
        }
    }

    pub fn to_message(&self) -> Message {
        let binary = |way: u8, call: u64, bytes: &[u8]| {
            let mut out = Vec::with_capacity(9 + bytes.len());
            out.push(way);
            out.extend_from_slice(&call.to_be_bytes());
            out.extend_from_slice(bytes);
            Message::binary(out)
        };
        let text = |v: Value| Message::text(v.to_string());
        match self {
            Frame::Body { call, bytes } => binary(TO_SERVER, *call, bytes),
            Frame::Reply { call, bytes } => binary(TO_CALLER, *call, bytes),
            Frame::Open { call, open } => text(json!({"call": call, "open": open})),
            Frame::End { call } => text(json!({"call": call, "end": true})),
            Frame::Cancel { call } => text(json!({"call": call, "cancel": true})),
            Frame::Head {
                call,
                status,
                content_type,
            } => text(
                json!({"call": call, "head": {"status": status, "content_type": content_type}}),
            ),
            Frame::Done { call } => text(json!({"call": call, "done": true})),
            Frame::Error {
                call,
                status,
                message,
            } => text(json!({"call": call, "error": {"status": status, "message": message}})),
        }
    }

    /// The frame `message` carries, or none when it carries none.
    pub fn from_message(message: &Message) -> Option<Frame> {
        match message {
            Message::Binary(data) if data.len() >= 9 => {
                let call = u64::from_be_bytes(data[1..9].try_into().ok()?);
                let bytes = data[9..].to_vec();
                match data[0] {
                    TO_SERVER => Some(Frame::Body { call, bytes }),
                    TO_CALLER => Some(Frame::Reply { call, bytes }),
                    _ => None,
                }
            }
            Message::Text(t) => {
                let v: Value = serde_json::from_str(t).ok()?;
                let call = v["call"].as_u64()?;
                if let Some(open) = v.get("open") {
                    let open = serde_json::from_value(open.clone()).ok()?;
                    Some(Frame::Open { call, open })
                } else if v.get("end").is_some() {
                    Some(Frame::End { call })
                } else if v.get("cancel").is_some() {
                    Some(Frame::Cancel { call })
                } else if let Some(head) = v.get("head") {
                    Some(Frame::Head {
                        call,
                        status: head["status"]
                            .as_u64()
                            .and_then(|s| u16::try_from(s).ok())?,
                        content_type: head["content_type"]
                            .as_str()
                            .unwrap_or("application/json")
                            .to_string(),
                    })
                } else if v.get("done").is_some() {
                    Some(Frame::Done { call })
                } else {
                    let e = v.get("error")?;
                    Some(Frame::Error {
                        call,
                        status: e["status"]
                            .as_u64()
                            .and_then(|s| u16::try_from(s).ok())
                            .unwrap_or(502),
                        message: e["message"].as_str().unwrap_or("").to_string(),
                    })
                }
            }
            _ => None,
        }
    }
}

/// The frames that send a request's `body` for call `call`: its chunks, then
/// its end.
pub fn body_frames(call: u64, body: &[u8]) -> Vec<Frame> {
    body.chunks(CHUNK)
        .map(|bytes| Frame::Body {
            call,
            bytes: bytes.to_vec(),
        })
        .chain(std::iter::once(Frame::End { call }))
        .collect()
}

/// The source of the model a request's `body` names as the runner that
/// shares it names it, `<source>:<id>`, and the body naming it `<id>`, as
/// that source serves it; none for a body that names no model so.
pub fn routed(body: &[u8]) -> Option<(String, Vec<u8>)> {
    let mut json: Value = serde_json::from_slice(body).ok()?;
    let named = json.get("model")?.as_str()?;
    let (source, id) = named.split_once(':')?;
    if source.is_empty() || id.is_empty() {
        return None;
    }
    let source = source.to_string();
    json["model"] = Value::String(id.to_string());
    Some((source, serde_json::to_vec(&json).ok()?))
}

/// What `GET /models` of a runner answers: the models it shares, each named
/// `<source>:<id>`, as an OpenAI-compatible list.
pub fn models_answer(names: &[String]) -> Vec<u8> {
    let data: Vec<Value> = names
        .iter()
        .map(|n| json!({ "id": n, "object": "model" }))
        .collect();
    json!({ "object": "list", "data": data })
        .to_string()
        .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_frame_comes_back_as_it_went() {
        let open = Open {
            runner: "ana/gpu-box".into(),
            source: "local".into(),
            method: "POST".into(),
            path: "/chat/completions".into(),
        };
        for frame in [
            Frame::Open { call: 7, open },
            Frame::Body {
                call: 7,
                bytes: b"{\"model\"".to_vec(),
            },
            Frame::End { call: 7 },
            Frame::Cancel { call: 7 },
            Frame::Head {
                call: 7,
                status: 200,
                content_type: "text/event-stream".into(),
            },
            Frame::Reply {
                call: u64::MAX,
                bytes: vec![0, 255],
            },
            Frame::Done { call: 7 },
            Frame::error(7, 503, "busy"),
        ] {
            assert_eq!(
                Frame::from_message(&frame.to_message()),
                Some(frame.clone())
            );
        }
        assert_eq!(Frame::from_message(&Message::text("{}")), None);
        assert_eq!(Frame::from_message(&Message::binary(vec![9; 12])), None);
        let frames = body_frames(3, &vec![1; CHUNK + 1]);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[2], Frame::End { call: 3 });
        assert_eq!(body_frames(3, b""), [Frame::End { call: 3 }]);
    }

    #[test]
    fn a_model_is_routed_to_its_source_as_the_runner_that_shares_it_names_it() {
        let (source, body) = routed(br#"{"model":"ollama:qwen3.5:9b","stream":true}"#).unwrap();
        assert_eq!(source, "ollama");
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["model"], "qwen3.5:9b");
        assert_eq!(body["stream"], true);
        assert_eq!(routed(br#"{"model":"qwen"}"#), None);
        assert_eq!(routed(b""), None);
    }
}
