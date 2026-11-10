//! Knowledge, kept by the hub: what a person's agents share becomes notes in a
//! vault, and later work recalls them.
//!
//! Capture is explicit only. The person tells their agent to share; the agent
//! picks what is worth keeping and calls the `share` tool. Nothing reads CLI
//! transcripts and nothing proxies CLI traffic. Only Ensemble writes notes: a
//! person corrects one by sharing the correction.
//!
//! The vault (`vault`) keeps each share as it arrived. A share is checked for
//! secrets first (`secrets`), because the vault is a git repository and a key
//! in it would stay in the history. The pipeline (`pipeline`) turns shares into
//! notes, in the background, finding related notes by their words (`search`).

use ensemble_backends::resolve::Resolver;
use ensemble_config::{Hub, Knowledge, LlmBackend};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub mod pipeline;
pub mod search;
pub mod secrets;
pub mod vault;

pub use pipeline::{PIPELINE_VERSION, Step};
pub use vault::{Note, Processed, Share, Vault};

/// How often the worker looks for new shares.
const POLL: Duration = Duration::from_secs(5);
/// How long it waits before asking again when no model could be used.
const RETRY: Duration = Duration::from_secs(60);

pub const SHARE: &str = "share";

/// One tool: its name, what it is for, and the schema of its arguments.
pub struct Tool {
    pub name: &'static str,
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: Value,
}

/// The knowledge of one hub, as one session uses it.
pub struct Memory {
    owner: String,
    knowledge: Knowledge,
    /// The working folder's name, never its path.
    project: Option<String>,
    /// The agent CLI on the other end, once it has said its name.
    client: Mutex<Option<String>>,
    /// The hub's own model endpoints.
    backends: BTreeMap<String, LlmBackend>,
    resolver: Resolver,
}

/// What a run of the pipeline over the pending shares did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Run {
    /// Shares that became notes, were skipped, or went to review, by outcome.
    pub done: Vec<Processed>,
    /// Why it stopped with shares still pending, when no model could be used.
    pub waiting: Option<String>,
}

impl Memory {
    /// The knowledge of `hub`, for work in the folder called `project`. None
    /// when the hub keeps no knowledge.
    pub fn new(hub: &Hub, project: Option<String>) -> Option<Memory> {
        Some(Memory {
            owner: hub.owner.clone(),
            knowledge: hub.knowledge.clone()?,
            project,
            client: Mutex::new(None),
            backends: hub.llm_backends.clone(),
            resolver: Resolver::default(),
        })
    }

    pub(crate) fn backends(&self) -> &BTreeMap<String, LlmBackend> {
        &self.backends
    }

    pub(crate) fn resolver(&self) -> &Resolver {
        &self.resolver
    }

    /// Process every pending share, oldest first, until none is left or no
    /// model can be used. Fails when another process holds the vault: it does
    /// this work already.
    pub fn distill(&self) -> Result<Run, String> {
        let vault = self.vault();
        let _lock = self.lock()?;
        self.distill_held(&vault, true)
    }

    fn distill_held(&self, vault: &Vault, commit: bool) -> Result<Run, String> {
        let mut run = Run::default();
        for share in pipeline::pending(vault) {
            match self.process(&share, commit)? {
                Step::Done(line) => run.done.push(line),
                Step::Waiting(why) => {
                    run.waiting = Some(why);
                    break;
                }
            }
        }
        Ok(run)
    }

    /// Throw the notes away and make them again from every share, in one
    /// commit: what a new pipeline version or a better model is for.
    pub fn rebuild(&self) -> Result<Run, String> {
        let vault = self.vault();
        let _lock = self.lock()?;
        vault.clear_products()?;
        let run = self.distill_held(&vault, false)?;
        vault.commit(&format!(
            "rebuild the notes from {} shares, pipeline {PIPELINE_VERSION}",
            run.done.len()
        ))?;
        Ok(run)
    }

    fn lock(&self) -> Result<ensemble_core::lock::WorkerLock, String> {
        let vault = self.vault();
        if !vault.exists() {
            return Err(format!(
                "the knowledge vault {} is not a git repository yet; `ensemble hub init` makes it",
                vault.root().display()
            ));
        }
        ensemble_core::lock::acquire(&vault.lock_dir()).map_err(|pid| {
            format!("process {pid} is distilling this vault already; it gets to every share")
        })
    }

    /// Distill shares as they arrive, until `stop` is set. Another process
    /// that holds the vault does this already, and then this returns at once.
    /// `say` gets one line per share, and one when it starts waiting for a
    /// model.
    pub fn work(&self, stop: &AtomicBool, say: &(dyn Fn(&str) + Sync)) {
        let vault = self.vault();
        let Ok(_lock) = self.lock() else { return };
        let mut said_waiting = false;
        while !stop.load(Ordering::Relaxed) {
            let mut pause = POLL;
            for share in pipeline::pending(&vault) {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                match self.process(&share, true) {
                    Ok(Step::Done(line)) => {
                        said_waiting = false;
                        say(&format!(
                            "knowledge: share {} -> {}{}",
                            line.share,
                            line.outcome,
                            if line.notes.is_empty() {
                                String::new()
                            } else {
                                format!(" ({})", line.notes.join(", "))
                            }
                        ));
                    }
                    Ok(Step::Waiting(why)) => {
                        if !said_waiting {
                            say(&format!("knowledge: waiting for a model: {why}"));
                            said_waiting = true;
                        }
                        pause = RETRY;
                        break;
                    }
                    Err(e) => {
                        say(&format!("knowledge: share {}: {e}", share.id));
                        pause = RETRY;
                        break;
                    }
                }
            }
            let until = Instant::now() + pause;
            while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    pub fn vault(&self) -> Vault {
        Vault::new(&self.knowledge.vault)
    }

    pub fn knowledge(&self) -> &Knowledge {
        &self.knowledge
    }

    /// Remember which agent CLI this session talks to, as it named itself.
    pub fn set_client(&self, name: &str) {
        let name = name.trim();
        if !name.is_empty() {
            *self.client.lock().unwrap_or_else(|e| e.into_inner()) = Some(name.to_string());
        }
    }

    /// The tools knowledge adds.
    pub fn tools(&self) -> Vec<Tool> {
        vec![Tool {
            name: SHARE,
            description: "Keep knowledge from this conversation in the user's knowledge vault, \
                 when the user asks you to share it. You decide what is worth keeping: decisions \
                 and why, facts about their systems, how-tos, lessons, commitments, preferences, \
                 who owns what. Leave out chatter, and anything the user wants kept private. \
                 Write it self-contained, in plain words: whoever reads it later cannot see this \
                 conversation. Never include secrets such as keys or passwords. Ensemble turns \
                 shares into notes in the background."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "What it is about, in a few words."
                    },
                    "text": {
                        "type": "string",
                        "description": "The knowledge, self-contained."
                    }
                },
                "required": ["title", "text"]
            }),
        }]
    }

    /// What the server says about knowledge when an agent connects.
    pub fn instructions(&self) -> &'static str {
        "\n\nThe user keeps a knowledge vault. When they ask you to share or remember something \
         from this conversation, call `share` with what is worth keeping, written so it stands \
         on its own; never share unless they ask."
    }

    /// Run one of the knowledge tools: `None` when `name` is not one of them,
    /// else the text for the caller, or why it failed.
    pub fn call(&self, name: &str, args: &Value) -> Option<Result<String, String>> {
        match name {
            SHARE => Some(self.share_tool(args)),
            _ => None,
        }
    }

    fn share_tool(&self, args: &Value) -> Result<String, String> {
        let text_of = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| format!("{SHARE}: `{key}` is required and must not be empty"))
        };
        let (title, text) = (text_of("title")?, text_of("text")?);
        let client = self
            .client
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let share = self.share(client.as_deref(), title, text)?;
        Ok(format!(
            "Shared as {}. Ensemble turns it into notes in the background.",
            share.id
        ))
    }

    /// Keep `text`, checked for secrets first.
    pub fn share(&self, client: Option<&str>, title: &str, text: &str) -> Result<Share, String> {
        if let Some(kind) = secrets::find_secret(&format!("{title}\n{text}")) {
            return Err(format!(
                "{SHARE}: refused: the text contains what looks like {kind}. The vault is a git \
                 repository and keeps everything it is given, so share it again without the \
                 secret."
            ));
        }
        let vault = self.vault();
        if !vault.exists() {
            return Err(format!(
                "{SHARE}: the knowledge vault {} is not a git repository yet; `ensemble hub init` makes it",
                vault.root().display()
            ));
        }
        vault.add_share(&self.owner, client, self.project.as_deref(), title, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_core::workspace::testing::TempDir;
    use std::path::Path;

    pub(crate) fn memory(tmp: &TempDir) -> Memory {
        let yaml = format!(
            "version: 1\nowner: Ana\nllm_backends:\n  local: {{ type: openai-compatible-api, base_url: \"http://127.0.0.1:1/v1\", timeout_seconds: 5 }}\nknowledge:\n  vault: \"{}\"\n  distill:\n    try:\n      - {{ backend: local, model: small }}\n    when_none: wait\n    max_tokens: 100\n  recall: {{ max_notes: 3, max_chars: 1000 }}\n",
            tmp.0.join("vault").display()
        );
        let hub = ensemble_config::parse_hub(
            &yaml,
            Path::new("hub.yaml"),
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
        let memory = Memory::new(&hub, Some("shop".into())).unwrap();
        memory.vault().init().unwrap();
        memory
    }

    #[test]
    fn the_share_tool_keeps_what_the_agent_chose_and_names_who_and_where() {
        let tmp = TempDir::new();
        let m = memory(&tmp);
        m.set_client("claude-code");
        let said = m
            .call(
                SHARE,
                &json!({"title": "Ports", "text": "The shop listens on 8080."}),
            )
            .unwrap()
            .unwrap();
        assert!(said.starts_with("Shared as "), "{said}");
        let shares = m.vault().shares();
        assert_eq!(shares.len(), 1);
        let s = &shares[0];
        assert_eq!(
            (
                s.shared_by.as_str(),
                s.client.as_deref(),
                s.project.as_deref()
            ),
            ("Ana", Some("claude-code"), Some("shop"))
        );
        assert_eq!(s.text, "The shop listens on 8080.");
        assert!(
            m.call("delegate", &json!({})).is_none(),
            "not a knowledge tool"
        );
    }

    #[test]
    fn a_share_with_a_secret_or_nothing_in_it_is_refused() {
        let tmp = TempDir::new();
        let m = memory(&tmp);
        let key = format!("the key is sk-{}", "a".repeat(40));
        let err = m
            .call(SHARE, &json!({"title": "Keys", "text": key}))
            .unwrap()
            .unwrap_err();
        assert!(err.contains("looks like an OpenAI-style API key"), "{err}");
        let err = m
            .call(SHARE, &json!({"title": " ", "text": "x"}))
            .unwrap()
            .unwrap_err();
        assert!(err.contains("`title` is required"), "{err}");
        assert!(m.vault().shares().is_empty());
    }

    #[test]
    fn a_hub_without_knowledge_has_no_memory() {
        let hub = ensemble_config::parse_hub(
            "version: 1\nowner: Ana\nllm_backends: {}\n",
            Path::new("hub.yaml"),
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
        assert!(Memory::new(&hub, None).is_none());
    }
}
