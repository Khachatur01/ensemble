//! The Model Port: the one interface every backend adapter implements.
//!
//! Everything above this line (the router, the MCP server, the CLI) is written
//! against `ModelPort` and the types in this module. Everything below it — how
//! Claude Code is spawned, how an HTTP endpoint is addressed, what an API error
//! body looks like — lives inside an adapter and is not visible from here.
//!
//! The error type is shared rather than per-adapter on purpose. A role failing
//! because a subprocess was killed and a role failing because an endpoint
//! returned 503 are the same event to the harness: this role did not answer, and
//! here is why in one sentence.

use std::fmt;

/// Who is speaking in a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Message {
            role: Role::System,
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Message {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// One completion request, in the only shape the port understands.
///
/// There is no `tools` field. Only one of the two shipped adapters can call a
/// caller's tools at all (see [`Capabilities::tool_calling`]), and a field that
/// half the implementations silently drop is worse than no field: the caller
/// believes it was honoured. Delegated work reaches tools through the backend's
/// own tooling — Claude Code runs its own Read/Grep/Edit and reports back.
#[derive(Debug, Clone)]
pub struct Request {
    /// The model id as the *backend* names it. The router fills this in from
    /// config; callers name a role, never a model.
    pub model: String,
    pub messages: Vec<Message>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Working directory for backends that act on a checkout. Ignored by
    /// adapters that have no notion of one.
    pub workdir: Option<String>,
}

impl Request {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Request {
            model: model.into(),
            messages,
            max_tokens: None,
            temperature: None,
            workdir: None,
        }
    }

    /// The concatenated `system` turns, which is how both adapters want them.
    pub fn system_text(&self) -> String {
        self.messages
            .iter()
            .filter(|m| m.role == Role::System)
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

impl Usage {
    pub fn total(&self) -> u32 {
        self.prompt_tokens + self.completion_tokens
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub text: String,
    /// What actually answered, as reported by the backend. Not necessarily what
    /// was asked for — an endpoint is free to serve something else, and the
    /// difference belongs in the record rather than being smoothed over.
    pub model: String,
    pub usage: Usage,
    pub finish_reason: Option<String>,
}

/// What a backend *type* can do. Declared, never inferred.
///
/// `tool_calling` is load-bearing: the manager coordinates by calling tools, so
/// a manager on a backend that cannot call them would answer in prose while the
/// delegation never happens. Config validation refuses that outright rather than
/// letting it fail silently at run time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub tool_calling: bool,
    pub streaming: bool,
}

/// Everything that can go wrong between asking a role for work and getting it.
#[derive(Debug)]
pub enum Error {
    /// The backend binary could not be started at all.
    Spawn {
        backend: String,
        program: String,
        detail: String,
    },
    /// It started and exited non-zero.
    ProcessFailed {
        backend: String,
        code: Option<i32>,
        stderr: String,
    },
    /// It was still running when the deadline passed, and was killed.
    Timeout { backend: String, seconds: u64 },
    /// The request never reached the endpoint: DNS, refused connection, TLS.
    Transport { backend: String, detail: String },
    /// The endpoint answered with a non-2xx status.
    Api {
        backend: String,
        status: u16,
        message: String,
    },
    /// It answered 2xx with something that is not the agreed shape.
    Protocol { backend: String, detail: String },
    /// The caller asked for something this backend does not serve.
    Unsupported { backend: String, detail: String },
}

impl Error {
    /// The backend an error came from, for log lines and run records.
    pub fn backend(&self) -> &str {
        match self {
            Error::Spawn { backend, .. }
            | Error::ProcessFailed { backend, .. }
            | Error::Timeout { backend, .. }
            | Error::Transport { backend, .. }
            | Error::Api { backend, .. }
            | Error::Protocol { backend, .. }
            | Error::Unsupported { backend, .. } => backend,
        }
    }

    /// Is retrying the same request plausibly going to work?
    ///
    /// Used to tell the harness whether to suggest trying again. A 4xx, a bad
    /// response shape, or an unsupported request will fail identically next
    /// time; a timeout or a 5xx may not.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Timeout { .. } | Error::Transport { .. } => true,
            Error::Api { status, .. } => *status == 429 || *status >= 500,
            Error::ProcessFailed { .. }
            | Error::Spawn { .. }
            | Error::Protocol { .. }
            | Error::Unsupported { .. } => false,
        }
    }
}

/// Messages are written for the model that will read them: what failed, on which
/// backend, and where to look. A truncation marker is added rather than silently
/// cutting, so nobody debugs a sentence that was never complete.
fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}… [truncated]")
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Spawn {
                backend,
                program,
                detail,
            } => write!(
                f,
                "backend '{backend}': could not start '{program}': {detail}. \
                 Is it installed and on PATH?"
            ),
            Error::ProcessFailed {
                backend,
                code,
                stderr,
            } => {
                let code = code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into());
                write!(
                    f,
                    "backend '{backend}': exited {code}: {}",
                    clip(stderr, 800)
                )
            }
            Error::Timeout { backend, seconds } => write!(
                f,
                "backend '{backend}': no answer within {seconds}s, killed. \
                 Raise timeout_seconds on the backend if this is normal for the task."
            ),
            Error::Transport { backend, detail } => {
                write!(
                    f,
                    "backend '{backend}': could not reach the endpoint: {detail}"
                )
            }
            Error::Api {
                backend,
                status,
                message,
            } => {
                write!(
                    f,
                    "backend '{backend}': endpoint returned {status}: {}",
                    clip(message, 800)
                )
            }
            Error::Protocol { backend, detail } => {
                write!(
                    f,
                    "backend '{backend}': unreadable response: {}",
                    clip(detail, 800)
                )
            }
            Error::Unsupported { backend, detail } => {
                write!(f, "backend '{backend}': {detail}")
            }
        }
    }
}

impl std::error::Error for Error {}

/// The port itself.
///
/// `&self` rather than `&mut self`: adapters hold configuration, not
/// conversation state, so one instance serves every role assigned to it and the
/// router can hand out shared references.
pub trait ModelPort: Send + Sync {
    /// The backend instance name from `ensemble.yaml`, for errors and logs.
    fn name(&self) -> &str;

    /// The backend type id — `claude-code-cli`, `openai-compatible-api`.
    fn type_id(&self) -> &'static str;

    fn capabilities(&self) -> Capabilities;

    fn complete(&self, request: &Request) -> Result<Response, Error>;

    /// Cheap reachability check. Must not spend a model call.
    fn health(&self) -> Result<(), Error>;
}

#[cfg(test)]
pub mod testing {
    //! Fakes used by several test modules. Kept beside the trait they implement
    //! so a change to the port breaks them here rather than in six files.

    use super::*;
    use std::sync::Mutex;

    /// Records what it was asked and returns a canned answer or a canned error.
    pub struct FakePort {
        pub name: String,
        pub caps: Capabilities,
        pub reply: Mutex<Result<String, String>>,
        pub seen: Mutex<Vec<Request>>,
    }

    impl FakePort {
        pub fn ok(name: &str, reply: &str) -> Self {
            FakePort {
                name: name.into(),
                caps: Capabilities {
                    tool_calling: true,
                    streaming: true,
                },
                reply: Mutex::new(Ok(reply.into())),
                seen: Mutex::new(Vec::new()),
            }
        }

        pub fn failing(name: &str, detail: &str) -> Self {
            FakePort {
                name: name.into(),
                caps: Capabilities {
                    tool_calling: true,
                    streaming: true,
                },
                reply: Mutex::new(Err(detail.into())),
                seen: Mutex::new(Vec::new()),
            }
        }

        pub fn last(&self) -> Option<Request> {
            self.seen.lock().unwrap().last().cloned()
        }

        pub fn call_count(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
    }

    impl ModelPort for FakePort {
        fn name(&self) -> &str {
            &self.name
        }
        fn type_id(&self) -> &'static str {
            "fake"
        }
        fn capabilities(&self) -> Capabilities {
            self.caps
        }
        fn complete(&self, request: &Request) -> Result<Response, Error> {
            self.seen.lock().unwrap().push(request.clone());
            match &*self.reply.lock().unwrap() {
                Ok(text) => Ok(Response {
                    text: text.clone(),
                    model: request.model.clone(),
                    usage: Usage {
                        prompt_tokens: 1,
                        completion_tokens: 2,
                    },
                    finish_reason: Some("stop".into()),
                }),
                Err(detail) => Err(Error::Transport {
                    backend: self.name.clone(),
                    detail: detail.clone(),
                }),
            }
        }
        fn health(&self) -> Result<(), Error> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_turns_are_concatenated_in_order() {
        let req = Request::new(
            "m",
            vec![
                Message::system("first"),
                Message::user("ignored"),
                Message::system("second"),
            ],
        );
        assert_eq!(req.system_text(), "first\n\nsecond");
    }

    #[test]
    fn retryability_follows_the_class_of_failure() {
        let b = || "b".to_string();
        assert!(
            Error::Timeout {
                backend: b(),
                seconds: 1
            }
            .is_retryable()
        );
        assert!(
            Error::Transport {
                backend: b(),
                detail: "refused".into()
            }
            .is_retryable()
        );
        assert!(
            Error::Api {
                backend: b(),
                status: 503,
                message: String::new()
            }
            .is_retryable()
        );
        assert!(
            Error::Api {
                backend: b(),
                status: 429,
                message: String::new()
            }
            .is_retryable()
        );
        // A bad request will be just as bad the second time.
        assert!(
            !Error::Api {
                backend: b(),
                status: 400,
                message: String::new()
            }
            .is_retryable()
        );
        assert!(
            !Error::Protocol {
                backend: b(),
                detail: "junk".into()
            }
            .is_retryable()
        );
        assert!(
            !Error::ProcessFailed {
                backend: b(),
                code: Some(1),
                stderr: String::new()
            }
            .is_retryable()
        );
    }

    #[test]
    fn long_payloads_are_marked_as_truncated_not_silently_cut() {
        let long = "x".repeat(5000);
        let msg = Error::Api {
            backend: "b".into(),
            status: 500,
            message: long,
        }
        .to_string();
        assert!(msg.contains("[truncated]"), "{msg}");
        assert!(msg.len() < 1200, "still bounded: {}", msg.len());
    }

    #[test]
    fn errors_name_the_backend_they_came_from() {
        let e = Error::Spawn {
            backend: "claude".into(),
            program: "claude".into(),
            detail: "No such file".into(),
        };
        assert_eq!(e.backend(), "claude");
        assert!(e.to_string().contains("on PATH"), "says what to do: {e}");
    }
}
