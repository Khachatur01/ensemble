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
    /// The result of a tool the *caller* ran, answering one [`ToolCall`].
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// A tool the *caller* can run, offered to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments, carried verbatim. Deliberately not
    /// modelled: it is the caller's schema, and re-encoding it here could only
    /// lose detail.
    pub parameters: serde_json::Value,
}

/// A model's request to run one tool.
///
/// Both fields are passed through this crate byte for byte. The caller pairs a
/// result to its call on `id`, and a model that writes malformed `arguments`
/// has to be reported as having done so rather than quietly corrected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Arguments as the model wrote them — a JSON *string*, not a value.
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Set on an assistant turn the model ended by asking for tools. Empty
    /// otherwise.
    pub tool_calls: Vec<ToolCall>,
    /// Set on [`Role::Tool`] turns only: which call this answers.
    pub tool_call_id: Option<String>,
}

impl Message {
    fn plain(role: Role, content: impl Into<String>) -> Self {
        Message {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self::plain(Role::System, content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::plain(Role::User, content)
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::plain(Role::Assistant, content)
    }
    /// An assistant turn that ended in tool calls.
    ///
    /// `content` is usually empty here, which is not the same as the turn being
    /// absent: the calls themselves are what the model said.
    pub fn assistant_calling(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Message {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
        }
    }
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Message {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// Whether the model is free to answer instead of calling a tool.
///
/// `Auto` is what every real request asks for: a manager that has finished has
/// to be able to simply answer, and forcing a call would turn the end of a turn
/// into a loop. `Required` exists for one caller, the startup probe in
/// `serve`, which is not asking what the model would do — it is asking whether
/// a tool call from this model on this server arrives as one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolChoice {
    #[default]
    Auto,
    Required,
}

impl ToolChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolChoice::Auto => "auto",
            ToolChoice::Required => "required",
        }
    }
}

/// One completion request, in the only shape the port understands.
///
/// `tools` is what the *caller* can run, offered to the model. It is empty for
/// every delegation: a role is asked for finished work and produces it with its
/// own backend's tooling — Claude Code runs its own Read/Grep/Edit and reports
/// back. It is non-empty for exactly one caller, `ensemble serve`, where a
/// harness drives the manager over the chat API, so the harness's tools have to
/// reach the model and their results have to come back.
///
/// The objection that once kept this field out still stands; it is now a rule
/// rather than an absence. A silently dropped `tools` array is worse than no
/// field at all, because the caller believes it was honoured. So no adapter may
/// drop it: one whose [`Capabilities::tool_calling`] is false must *fail* a
/// request that offers tools, naming the backend and the fix.
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
    /// Tools the *caller* can run. See the note on this struct.
    pub tools: Vec<ToolDef>,
    /// Ignored when `tools` is empty: there is nothing to choose from.
    pub tool_choice: ToolChoice,
}

impl Request {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Request {
            model: model.into(),
            messages,
            max_tokens: None,
            temperature: None,
            workdir: None,
            tools: Vec::new(),
            tool_choice: ToolChoice::default(),
        }
    }

    /// Does this request ask the model to drive a caller-supplied tool schema?
    pub fn offers_tools(&self) -> bool {
        !self.tools.is_empty()
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
    /// What the model asked the *caller* to run. Empty unless the request
    /// offered tools; an adapter never invents one.
    pub tool_calls: Vec<ToolCall>,
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
    /// Not consulted anywhere today. `ensemble serve` synthesises SSE from a
    /// completed response, so streaming is a property of the transport rather
    /// than of the backend, and enforcing this flag would refuse requests that
    /// work. It stays declared because the only thing that would read it — a
    /// streaming port method — is the known next step, and removing then
    /// re-adding it would churn both adapters and the fake.
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
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// One scripted answer, for a test that needs several turns in a row.
    ///
    /// A manager loop is several model calls, and each one has to be able to
    /// say something different; a single canned reply cannot express
    /// "delegate, then answer".
    #[derive(Debug, Clone)]
    pub enum Scripted {
        Text(String),
        /// Tool calls, with optional leading prose.
        Calls(String, Vec<ToolCall>),
        /// Text the backend reports as cut off at the token limit. Says
        /// nothing about what the model would have produced.
        Truncated(String),
        Fail(String),
    }

    impl Scripted {
        pub fn text(s: &str) -> Self {
            Scripted::Text(s.into())
        }
        pub fn truncated(s: &str) -> Self {
            Scripted::Truncated(s.into())
        }
        /// One call to `name` with the given raw JSON arguments.
        pub fn call(id: &str, name: &str, arguments: &str) -> Self {
            Scripted::Calls(
                String::new(),
                vec![ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments: arguments.into(),
                }],
            )
        }
        pub fn calls(calls: Vec<ToolCall>) -> Self {
            Scripted::Calls(String::new(), calls)
        }
    }

    pub fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    /// Records what it was asked and returns a canned answer or a canned error.
    ///
    /// `script` is consumed one entry per call; when it runs dry the standing
    /// `reply` answers every further call. That way a test scripts only the
    /// turns it cares about.
    pub struct FakePort {
        pub name: String,
        pub caps: Capabilities,
        pub reply: Mutex<Result<String, String>>,
        pub script: Mutex<VecDeque<Scripted>>,
        pub seen: Mutex<Vec<Request>>,
    }

    impl FakePort {
        fn build(name: &str, reply: Result<String, String>) -> Self {
            FakePort {
                name: name.into(),
                caps: Capabilities {
                    tool_calling: true,
                    streaming: true,
                },
                reply: Mutex::new(reply),
                script: Mutex::new(VecDeque::new()),
                seen: Mutex::new(Vec::new()),
            }
        }

        pub fn ok(name: &str, reply: &str) -> Self {
            Self::build(name, Ok(reply.into()))
        }

        pub fn failing(name: &str, detail: &str) -> Self {
            Self::build(name, Err(detail.into()))
        }

        /// A port that answers the given turns in order, then falls back to
        /// `reply` for anything after them.
        pub fn scripted(name: &str, reply: &str, script: Vec<Scripted>) -> Self {
            let p = Self::build(name, Ok(reply.into()));
            *p.script.lock().unwrap() = script.into();
            p
        }

        /// Declare a port that cannot drive a caller-supplied tool schema.
        pub fn without_tool_calling(mut self) -> Self {
            self.caps.tool_calling = false;
            self
        }

        pub fn last(&self) -> Option<Request> {
            self.seen.lock().unwrap().last().cloned()
        }

        pub fn nth(&self, i: usize) -> Option<Request> {
            self.seen.lock().unwrap().get(i).cloned()
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

            let scripted = self.script.lock().unwrap().pop_front();
            let (text, tool_calls, finish) = match scripted {
                Some(Scripted::Text(t)) => (t, Vec::new(), "stop"),
                Some(Scripted::Calls(t, calls)) => (t, calls, "tool_calls"),
                Some(Scripted::Truncated(t)) => (t, Vec::new(), "length"),
                Some(Scripted::Fail(detail)) => {
                    return Err(Error::Transport {
                        backend: self.name.clone(),
                        detail,
                    });
                }
                None => match &*self.reply.lock().unwrap() {
                    Ok(text) => (text.clone(), Vec::new(), "stop"),
                    Err(detail) => {
                        return Err(Error::Transport {
                            backend: self.name.clone(),
                            detail: detail.clone(),
                        });
                    }
                },
            };

            Ok(Response {
                text,
                tool_calls,
                model: request.model.clone(),
                usage: Usage {
                    prompt_tokens: 1,
                    completion_tokens: 2,
                },
                finish_reason: Some(finish.into()),
            })
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
