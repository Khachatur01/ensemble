//! What every adapter and probe shares: the error type and token usage.
//!
//! The error type is shared rather than per-adapter on purpose. An agent failing
//! because its program would not start and an agent failing because an endpoint
//! returned 503 are the same event to the caller: this agent did not answer, and
//! here is why in one sentence.

use std::fmt;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
}

/// Everything that can go wrong between asking an agent for work and getting it.
#[derive(Debug)]
pub enum Error {
    /// An agent's program could not be started at all.
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
    /// The deadline passed before the endpoint answered.
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
            Error::Spawn { .. }
            | Error::ProcessFailed { .. }
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
                 Raise max_seconds if this is normal for the task."
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

#[cfg(test)]
mod tests {
    use super::*;

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
        for status in [429, 503] {
            assert!(
                Error::Api {
                    backend: b(),
                    status,
                    message: String::new()
                }
                .is_retryable()
            );
        }
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
    }

    #[test]
    fn long_payloads_are_marked_as_truncated_not_silently_cut() {
        let msg = Error::Api {
            backend: "b".into(),
            status: 500,
            message: "x".repeat(5000),
        }
        .to_string();
        assert!(msg.contains("[truncated]"), "{msg}");
        assert!(msg.len() < 1200, "still bounded: {}", msg.len());
    }

    #[test]
    fn errors_name_the_backend_they_came_from() {
        let e = Error::Transport {
            backend: "local".into(),
            detail: "refused".into(),
        };
        assert_eq!(e.backend(), "local");
        assert!(e.to_string().contains("'local'"), "{e}");
    }
}
