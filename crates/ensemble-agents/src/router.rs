//! Where a task's work goes: the run of the pair taken for it (`means`),
//! started in the task's worktree with the task, and what it did; or, for a
//! task that only needs an answer, one request to a model of the pool alone
//! ([`Ensemble::answer`]). The hub's task picker chooses which, and the
//! result reports it.

use crate::external::AgentRun;
use crate::means::{ALONE, Means, Taken, TakenAlone};
use crate::spec::{Adapter, RunSpec};
use ensemble_backends::chat;
use ensemble_core::port::Error;
use ensemble_core::workspace::Changes;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The most tokens of a model's answer when it answers a task alone. A
/// model whose context window is known gets less when the task leaves less
/// room.
pub const ANSWER_MAX_TOKENS: u32 = 8192;

/// The most tokens of answer to ask for of a model whose context window is
/// `window`, the request being `task`: [`ANSWER_MAX_TOKENS`], or what the
/// window leaves, the request taken as a token per three bytes, which is
/// more than it is. None when it leaves too little to answer in.
fn answer_budget(window: Option<u32>, task: &str) -> Option<u32> {
    let Some(window) = window else {
        return Some(ANSWER_MAX_TOKENS);
    };
    let asked = task.len() / 3 + 64;
    let left = (window as usize).saturating_sub(asked);
    (left >= 256).then(|| ANSWER_MAX_TOKENS.min(left as u32))
}

/// What a run produced, with enough provenance to audit it.
///
/// Where it ran and the model are *reported*, not accepted as input. A caller
/// cannot steer the work onto a particular model; a reader of the record can
/// still see which one did it.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The harness and the model: `opencode on local:qwen3.5:9b`.
    pub name: String,
    /// The harness, and the provider when there is one.
    pub runs_on: String,
    pub model: String,
    pub text: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub stats: AgentStats,
}

/// How much the run did to get its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStats {
    pub steps: u32,
    pub tool_calls: u32,
    /// Files it changed, so the caller can review them.
    pub changes: Changes,
    /// Shell commands it ran.
    pub commands: Vec<String>,
}

/// The runs of one repository, on the means of the runner.
pub struct Ensemble {
    means: Arc<Means>,
    workdir: Option<PathBuf>,
    /// Where tasks, run logs and worktrees go; the per-user state directory
    /// of the working directory unless set.
    state: Option<PathBuf>,
    /// Where the record of runs is kept, when it is.
    records: Option<PathBuf>,
}

impl Ensemble {
    pub fn new(means: Arc<Means>, workdir: Option<PathBuf>) -> Self {
        Ensemble {
            means,
            workdir,
            state: None,
            records: None,
        }
    }

    /// Keep state in `dir` instead of the per-user state directory.
    pub fn with_state(mut self, dir: PathBuf) -> Self {
        self.state = Some(dir);
        self
    }

    /// Record each run in `dir`'s `records.jsonl`.
    pub fn with_records(mut self, dir: PathBuf) -> Self {
        self.records = Some(dir);
        self
    }

    /// What the runner has to run tasks on.
    pub fn means(&self) -> &Arc<Means> {
        &self.means
    }

    /// Where tasks, run logs and worktrees are kept.
    pub fn state_dir(&self) -> std::io::Result<PathBuf> {
        match &self.state {
            Some(dir) => Ok(dir.clone()),
            None => ensemble_core::state::dir_for(&self.workdir()?).map_err(std::io::Error::other),
        }
    }

    /// The project directory runs work in.
    pub fn workdir(&self) -> std::io::Result<PathBuf> {
        match &self.workdir {
            Some(dir) => Ok(dir.clone()),
            None => std::env::current_dir(),
        }
    }

    /// Run what `taken` set up on `task`, in `workdir`, telling `progress`
    /// what it is doing as it goes. Setting `cancel` stops it.
    pub async fn run(
        &self,
        taken: &Taken,
        task: &str,
        context: Option<&str>,
        workdir: &Path,
        progress: &(dyn Fn(&str) + Sync),
        cancel: &AtomicBool,
    ) -> Result<Outcome, Error> {
        use crate::external::{builtin, claude_code, codex, opencode};
        let spec = &taken.spec;
        let backend = |provider: &str| {
            taken
                .means()
                .backend(provider)
                .ok_or_else(|| Error::Unsupported {
                    backend: provider.to_string(),
                    detail: format!(
                        "the provider of {} is not in the runner's ANN pool",
                        spec.name
                    ),
                })
        };
        let run = |run| self.recorded(spec, run);
        match &spec.adapter {
            Adapter::ClaudeCode(cc) => {
                run(Box::pin(claude_code::run(
                    spec, cc, task, context, workdir, progress, cancel,
                )))
                .await
            }
            Adapter::Codex(cx) => {
                run(Box::pin(codex::run(
                    spec, cx, task, context, workdir, progress, cancel,
                )))
                .await
            }
            Adapter::Builtin(ba) => {
                let backend = backend(&ba.provider)?;
                run(Box::pin(builtin::run(
                    spec, ba, backend, task, context, workdir, progress, cancel,
                )))
                .await
            }
            Adapter::OpenCode(oc) => {
                let backend = backend(&oc.provider)?;
                run(Box::pin(opencode::run(
                    spec, oc, backend, task, context, workdir, progress, cancel,
                )))
                .await
            }
        }
    }

    /// Ask the model `taken` holds to answer `task` alone, in one request,
    /// with no tools: its answer, recorded as a run of [`ALONE`]. Setting
    /// `cancel` stops waiting for it.
    pub async fn answer(
        &self,
        taken: &TakenAlone,
        task: &str,
        cancel: &AtomicBool,
    ) -> Result<Outcome, Error> {
        let alone = taken.alone.clone();
        let backend = taken
            .means()
            .backend(&alone.provider)
            .cloned()
            .ok_or_else(|| Error::Unsupported {
                backend: alone.provider.clone(),
                detail: format!(
                    "the provider of {} is not in the runner's ANN pool",
                    alone.name()
                ),
            })?;
        let max_tokens =
            answer_budget(alone.context_window, task).ok_or_else(|| Error::Unsupported {
                backend: alone.provider.clone(),
                detail: format!(
                    "the task is too long for {}'s context window of {} tokens",
                    alone.name(),
                    alone.context_window.unwrap_or_default()
                ),
            })?;
        let started = std::time::SystemTime::now();
        let clock = std::time::Instant::now();
        let (model, user) = (alone.model.clone(), task.to_string());
        let call = tokio::task::spawn_blocking(move || {
            let ask = chat::Ask {
                model: &model,
                system: None,
                user: &user,
                max_tokens,
            };
            let (path, body) = chat::request(&backend, &ask);
            chat::send(&backend, path, &body)
        });
        let stopped = async {
            while !cancel.load(Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        };
        let answered = tokio::select! {
            joined = call => match joined {
                Ok(answered) => Some(answered),
                Err(e) => Some(Err(Error::Protocol {
                    backend: alone.provider.clone(),
                    detail: format!("the call stopped: {e}"),
                })),
            },
            () = stopped => None,
        };
        let seconds = clock.elapsed().as_secs_f64();
        let started = started
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let record = |answer: Option<&chat::Answer>, error: Option<String>| {
            let usage = answer.map(|a| a.usage).unwrap_or_default();
            let reported = usage.prompt_tokens > 0 || usage.completion_tokens > 0;
            self.record(ensemble_core::records::Record {
                harness: ALONE.to_string(),
                model: alone.model_name(),
                started,
                seconds,
                prompt_tokens: reported.then_some(u64::from(usage.prompt_tokens)),
                completion_tokens: reported.then_some(u64::from(usage.completion_tokens)),
                steps: u32::from(answer.is_some()),
                tool_calls: 0,
                ok: error.is_none(),
                error,
            });
        };
        let outcome = |text: String, usage: ensemble_core::port::Usage| Outcome {
            name: alone.name(),
            runs_on: format!("{ALONE} on {}", alone.provider),
            model: alone.model_name(),
            text,
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            stats: AgentStats {
                steps: 1,
                tool_calls: 0,
                changes: Changes::default(),
                commands: Vec::new(),
            },
        };
        match answered {
            None => {
                record(None, Some("stopped before it answered".into()));
                Ok(outcome(
                    "[stopped before the model answered]".into(),
                    Default::default(),
                ))
            }
            Some(Err(e)) => {
                record(None, Some(e.to_string().chars().take(200).collect()));
                Err(e)
            }
            Some(Ok(answer)) => {
                record(Some(&answer), None);
                Ok(outcome(answer.text.trim().to_string(), answer.usage))
            }
        }
    }

    /// `run`, a run of `spec`, recorded as it ended.
    async fn recorded(
        &self,
        spec: &RunSpec,
        run: Pin<Box<dyn Future<Output = Result<AgentRun, Error>> + Send + '_>>,
    ) -> Result<Outcome, Error> {
        let started = std::time::SystemTime::now();
        let clock = std::time::Instant::now();
        let run = run.await;
        let seconds = clock.elapsed().as_secs_f64();
        let started = started
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let run = match run {
            Ok(run) => run,
            Err(e) => {
                self.record(ensemble_core::records::Record {
                    harness: spec.kind().to_string(),
                    model: spec.model().to_string(),
                    started,
                    seconds,
                    prompt_tokens: None,
                    completion_tokens: None,
                    steps: 0,
                    tool_calls: 0,
                    ok: false,
                    error: Some(e.to_string().chars().take(200).collect()),
                });
                return Err(e);
            }
        };
        // No tokens reported is not the same as none spent.
        let reported = run.usage.prompt_tokens > 0 || run.usage.completion_tokens > 0;
        self.record(ensemble_core::records::Record {
            harness: spec.kind().to_string(),
            model: spec.model().to_string(),
            started,
            seconds,
            prompt_tokens: reported.then_some(u64::from(run.usage.prompt_tokens)),
            completion_tokens: reported.then_some(u64::from(run.usage.completion_tokens)),
            steps: run.steps,
            tool_calls: run.tool_calls,
            ok: true,
            error: None,
        });
        Ok(Outcome {
            name: spec.name.clone(),
            runs_on: spec.runs_on(),
            model: Some(run.model)
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| spec.model().to_string()),
            text: run.text,
            prompt_tokens: run.usage.prompt_tokens,
            completion_tokens: run.usage.completion_tokens,
            stats: AgentStats {
                steps: run.steps,
                tool_calls: run.tool_calls,
                changes: run.changes,
                commands: run.commands,
            },
        })
    }

    fn record(&self, record: ensemble_core::records::Record) {
        if let Some(dir) = &self.records {
            ensemble_core::records::append(dir, &record);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_config::{AnnPool, Harness, HarnessKind, HarnessModel, Tier};
    use ensemble_core::workspace::testing::TempDir;

    /// A runner whose one harness is Claude Code, run as `program`, on
    /// sonnet.
    fn ensemble(program: &str, records: &Path) -> (Ensemble, Arc<Means>) {
        let claude = Harness {
            kind: HarnessKind::ClaudeCode,
            missing: false,
            command: Some(program.into()),
            max_seconds: 60,
            max_steps: 5,
            max_concurrent: 1,
            max_tokens: None,
            models: vec![HarnessModel {
                id: "sonnet".into(),
                tier: Tier::Strong,
                abilities: None,
            }],
        };
        let means = Arc::new(Means::new(
            vec![claude],
            AnnPool::default(),
            Default::default(),
        ));
        let e = Ensemble::new(means.clone(), None).with_records(records.to_path_buf());
        (e, means)
    }

    #[test]
    fn a_run_is_recorded_with_its_harness_and_model() {
        let tmp = TempDir::new();
        let script = tmp.0.join("claude.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\",\"num_turns\":1,\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (e, means) = ensemble(&script.display().to_string(), &tmp.0);
        let taken = means.take("claude-code", "sonnet").unwrap().unwrap();
        let out = crate::runtime::block_on(e.run(
            &taken,
            "do it",
            None,
            &tmp.0,
            &|_| {},
            &AtomicBool::new(false),
        ))
        .unwrap();
        assert_eq!(out.text, "done");
        assert_eq!(out.name, "claude-code on sonnet");
        let records = ensemble_core::records::read(&tmp.0);
        assert_eq!(records.len(), 1);
        assert_eq!(
            (records[0].harness.as_str(), records[0].model.as_str()),
            ("claude-code", "sonnet")
        );
        assert!(records[0].ok);
    }

    #[test]
    fn a_model_alone_answers_in_one_request_with_no_tools_and_is_recorded() {
        let tmp = TempDir::new();
        let server = ensemble_backends::testing::model_server(&["small"], &["It is 42."]);
        let mut pool = AnnPool::default();
        let local = ensemble_config::LlmBackend::openai("local", &server.url, 5);
        pool.direct.insert("local".into(), local);
        pool.models = vec![ensemble_config::PoolModel {
            provider: "local".into(),
            id: "small".into(),
            kind: ensemble_config::ModelKind::Llm {
                tier: Tier::Small,
                context_window: Some(32768),
                tools: Some(true),
            },
            missing: false,
            shared: false,
            abilities: None,
        }];
        let backends = pool.direct.clone();
        let means = Arc::new(Means::new(Vec::new(), pool, backends));
        let e = Ensemble::new(means.clone(), None).with_records(tmp.0.clone());
        let taken = means.take_alone("local:small").unwrap().unwrap();
        let out = crate::runtime::block_on(e.answer(
            &taken,
            "What is six times seven?",
            &AtomicBool::new(false),
        ))
        .unwrap();
        assert_eq!(out.text, "It is 42.");
        assert_eq!(out.name, "local:small alone");
        let asked: serde_json::Value =
            serde_json::from_str(&server.requests.lock().unwrap()[0]).unwrap();
        assert_eq!(asked["model"], "small");
        assert_eq!(asked["max_tokens"], ANSWER_MAX_TOKENS);
        assert!(asked.get("tools").is_none(), "{asked}");
        assert_eq!(
            asked["messages"],
            serde_json::json!([{"role": "user", "content": "What is six times seven?"}]),
            "the task alone, with no prompt of Ensemble's"
        );
        let records = ensemble_core::records::read(&tmp.0);
        assert_eq!(
            (records[0].harness.as_str(), records[0].model.as_str()),
            ("alone", "local:small")
        );
        assert!(records[0].ok);
    }

    #[test]
    fn an_answer_takes_what_the_context_window_leaves() {
        assert_eq!(answer_budget(None, "hi"), Some(ANSWER_MAX_TOKENS));
        assert_eq!(answer_budget(Some(200_000), "hi"), Some(ANSWER_MAX_TOKENS));
        let left = answer_budget(Some(4096), "hi").unwrap();
        assert!(left < 4096 && left > 3000, "{left}");
        assert_eq!(answer_budget(Some(1024), &"x".repeat(3000)), None);
    }

    #[test]
    fn a_harness_that_is_not_installed_is_reported_by_name_and_recorded_failed() {
        let tmp = TempDir::new();
        let (e, means) = ensemble("/nonexistent/claude", &tmp.0);
        let taken = means.take("claude-code", "sonnet").unwrap().unwrap();
        let err = crate::runtime::block_on(e.run(
            &taken,
            "do it",
            None,
            &tmp.0,
            &|_| {},
            &AtomicBool::new(false),
        ))
        .unwrap_err();
        assert!(err.to_string().contains("claude"), "{err}");
        assert!(!ensemble_core::records::read(&tmp.0)[0].ok);
    }
}
