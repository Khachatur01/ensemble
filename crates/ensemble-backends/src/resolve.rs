//! Choosing the model a run uses from an agent's `try` list: the first whose
//! backend answers and serves it. What each backend serves is remembered for a
//! while, so a queue of ten tasks asks a backend once, not ten times.

use crate::models::models;
use ensemble_config::LlmBackend;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long what a backend answered is believed.
pub const REMEMBER_FOR: Duration = Duration::from_secs(60);

/// Why one model of a `try` list could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub backend: String,
    pub model: String,
    pub why: String,
}

/// What a backend answered when asked which models it serves, and when.
type Seen = (Instant, Result<Vec<String>, String>);

/// Picks models, remembering what each backend served.
pub struct Resolver {
    /// By backend name.
    seen: Mutex<HashMap<String, Seen>>,
    remember_for: Duration,
}

impl Default for Resolver {
    fn default() -> Self {
        Resolver::remembering_for(REMEMBER_FOR)
    }
}

impl Resolver {
    /// A resolver that believes an answer for `remember_for`.
    pub fn remembering_for(remember_for: Duration) -> Self {
        Resolver {
            seen: Mutex::new(HashMap::new()),
            remember_for,
        }
    }

    /// The index of the first model in `candidates` whose backend answers and
    /// serves it, or why each one cannot be used. A list of one is used as it
    /// is, without asking: there is nothing to choose, and the run itself will
    /// say what is wrong.
    pub fn choose(&self, candidates: &[(&LlmBackend, &str)]) -> Result<usize, Vec<Refusal>> {
        if candidates.len() == 1 {
            return Ok(0);
        }
        let mut refusals = Vec::new();
        for (i, (backend, model)) in candidates.iter().enumerate() {
            match self.served(backend) {
                Ok(served) if served.iter().any(|s| s == model) => return Ok(i),
                Ok(served) => refusals.push(refusal(
                    backend,
                    model,
                    format!("does not serve it; it serves {}", served.join(", ")),
                )),
                Err(why) => refusals.push(refusal(backend, model, why)),
            }
        }
        Err(refusals)
    }

    /// What [`Self::choose`] would say from what is already known, asking no
    /// backend: `None` when some backend it would have to ask is not known.
    pub fn known(&self, candidates: &[(&LlmBackend, &str)]) -> Option<Result<usize, ()>> {
        if candidates.len() == 1 {
            return Some(Ok(0));
        }
        let seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        for (i, (backend, model)) in candidates.iter().enumerate() {
            let (when, answer) = seen.get(&backend.name)?;
            if when.elapsed() >= self.remember_for {
                return None;
            }
            if answer.as_ref().is_ok_and(|s| s.iter().any(|m| m == model)) {
                return Some(Ok(i));
            }
        }
        Some(Err(()))
    }

    /// What `backend` serves, asked at most once per `remember_for`.
    fn served(&self, backend: &LlmBackend) -> Result<Vec<String>, String> {
        if let Some((when, answer)) = self
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&backend.name)
            && when.elapsed() < self.remember_for
        {
            return answer.clone();
        }
        // Asked without the lock held: a backend that takes its time does not
        // hold up a resolver asking about another.
        let answer = models(backend).map_err(|e| e.to_string());
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(backend.name.clone(), (Instant::now(), answer.clone()));
        answer
    }
}

fn refusal(backend: &LlmBackend, model: &str, why: String) -> Refusal {
    Refusal {
        backend: backend.name.clone(),
        model: model.to_string(),
        why,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_config::BackendSettings;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn backend(name: &str, base_url: String) -> LlmBackend {
        LlmBackend {
            name: name.into(),
            timeout_seconds: 5,
            settings: BackendSettings::OpenAiCompatible {
                base_url,
                api_key: None,
            },
        }
    }

    /// A backend that serves `served`, and how many times it was asked.
    fn serving(name: &str, served: &[&str]) -> (LlmBackend, Arc<AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let data: Vec<String> = served
            .iter()
            .map(|m| format!("{{\"id\":\"{m}\"}}"))
            .collect();
        let body = format!("{{\"data\":[{}]}}", data.join(","));
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = asked.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let mut reader = BufReader::new(&stream);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                        break;
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let mut stream = &stream;
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (backend(name, format!("http://127.0.0.1:{port}/v1")), asked)
    }

    /// A backend nothing listens on.
    fn down(name: &str) -> LlmBackend {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        backend(name, format!("http://127.0.0.1:{port}/v1"))
    }

    #[test]
    fn the_first_model_whose_backend_serves_it_is_chosen() {
        let (home, _) = serving("home", &["small"]);
        let (central, _) = serving("central", &["big"]);
        let r = Resolver::default();
        assert_eq!(r.choose(&[(&home, "small"), (&central, "big")]), Ok(0));
        // Answers are remembered by backend name; a fresh resolver asks again.
        let off = down("home");
        assert_eq!(
            Resolver::default().choose(&[(&off, "small"), (&central, "big")]),
            Ok(1)
        );
    }

    #[test]
    fn a_backend_that_does_not_serve_the_model_is_passed_over() {
        let (home, _) = serving("home", &["other"]);
        let (central, _) = serving("central", &["big"]);
        assert_eq!(
            Resolver::default().choose(&[(&home, "small"), (&central, "big")]),
            Ok(1)
        );
    }

    #[test]
    fn when_none_works_each_says_why() {
        let off = down("home");
        let (central, _) = serving("central", &["other"]);
        let refusals = Resolver::default()
            .choose(&[(&off, "small"), (&central, "big")])
            .unwrap_err();
        assert_eq!(refusals.len(), 2);
        assert_eq!(
            (refusals[0].backend.as_str(), refusals[0].model.as_str()),
            ("home", "small")
        );
        assert!(
            refusals[1].why.contains("does not serve it") && refusals[1].why.contains("other"),
            "{refusals:?}"
        );
    }

    #[test]
    fn a_list_of_one_is_used_without_asking() {
        let off = down("home");
        let r = Resolver::default();
        assert_eq!(r.choose(&[(&off, "small")]), Ok(0));
        assert_eq!(r.known(&[(&off, "small")]), Some(Ok(0)));
    }

    #[test]
    fn a_backend_is_asked_once_while_its_answer_is_remembered() {
        let (home, asked) = serving("home", &["small"]);
        let (central, _) = serving("central", &["big"]);
        let r = Resolver::default();
        assert_eq!(r.known(&[(&home, "small"), (&central, "big")]), None);
        for _ in 0..3 {
            assert_eq!(r.choose(&[(&home, "small"), (&central, "big")]), Ok(0));
        }
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        assert_eq!(r.known(&[(&home, "small"), (&central, "big")]), Some(Ok(0)));

        let forgetful = Resolver::remembering_for(Duration::ZERO);
        forgetful
            .choose(&[(&home, "small"), (&central, "big")])
            .unwrap();
        forgetful
            .choose(&[(&home, "small"), (&central, "big")])
            .unwrap();
        assert_eq!(asked.load(Ordering::SeqCst), 3);
    }
}
