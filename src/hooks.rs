//! Session hooks: notes from the hub's knowledge vault put into the person's
//! own CLI session as it goes, with no `recall` call needed.
//!
//! Claude Code and Codex run a command on `UserPromptSubmit` and
//! `SessionStart`, give it the event as JSON on stdin, and add what it prints
//! to the conversation as context. `ensemble hook` is that command: on a
//! prompt it recalls the notes about the prompt, and at the start of a session
//! the notes about the project's folder. A note already given in the session
//! is not given again, until the context is cleared or compacted.
//!
//! OpenCode has no command hooks, only plugins in JavaScript; its sessions get
//! knowledge through the MCP tools alone.

use ensemble_config::{CLAUDE_CODE_AGENT, CODEX_AGENT};
use ensemble_knowledge::RECALL_HEADER;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The CLIs whose hooks `connect --hooks` can set up.
pub const CLIS: [&str; 2] = [CLAUDE_CODE_AGENT, CODEX_AGENT];

pub const PROMPT_EVENT: &str = "UserPromptSubmit";
pub const START_EVENT: &str = "SessionStart";

/// What a hook may print: Claude Code keeps up to 10 000 characters of it.
pub const MAX_PRINTED: usize = 9_000;
/// A prompt with fewer words than this recalls nothing: "yes", "go on".
const MIN_PROMPT_WORDS: usize = 3;
/// Seconds a CLI waits for the hook.
const TIMEOUT_SECONDS: u64 = 15;
/// How long what a session was given is remembered.
const KEPT: Duration = Duration::from_secs(7 * 24 * 3600);

/// One hook event, as a CLI sends it on stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub name: String,
    pub session: String,
    pub cwd: Option<PathBuf>,
    pub prompt: Option<String>,
    /// Why a session started: `startup`, `resume`, `clear`, `compact`, ...
    pub source: Option<String>,
}

pub fn read_event(json: &str) -> Result<Event, String> {
    let v: Value =
        serde_json::from_str(json).map_err(|e| format!("the hook event is not JSON: {e}"))?;
    let text = |k: &str| v[k].as_str().map(str::to_string);
    Ok(Event {
        name: text("hook_event_name").ok_or("the hook event has no `hook_event_name`")?,
        session: text("session_id").unwrap_or_default(),
        cwd: text("cwd").map(PathBuf::from),
        prompt: text("prompt"),
        source: text("source"),
    })
}

impl Event {
    /// What to recall for it: the prompt, or the project folder's name at the
    /// start of a session. None for other events and for short prompts.
    pub fn topic(&self) -> Option<String> {
        match self.name.as_str() {
            PROMPT_EVENT => self
                .prompt
                .as_deref()
                .map(str::trim)
                .filter(|p| p.split_whitespace().count() >= MIN_PROMPT_WORDS)
                .map(|p| p.chars().take(1000).collect()),
            START_EVENT => self
                .cwd
                .as_deref()
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().replace(['-', '_'], " ")),
            _ => None,
        }
    }

    /// Whether the session's context lost what earlier hooks gave it.
    pub fn forgets(&self) -> bool {
        self.name == START_EVENT
            && matches!(
                self.source.as_deref(),
                Some("clear" | "compact" | "startup")
            )
    }
}

/// The notes of a recall's answer that `given` does not hold yet, under the
/// answer's header, with each one added to `given`. None when no note is new,
/// or the answer has none.
pub fn fresh_notes(answer: &str, given: &mut BTreeSet<String>) -> Option<String> {
    let notes = answer.strip_prefix(RECALL_HEADER)?;
    let mut out = String::new();
    for note in notes.split("\n## ").skip(1) {
        let heading = note.lines().next().unwrap_or("").to_string();
        if given.insert(heading) {
            out.push_str("\n## ");
            out.push_str(note.trim_end());
            out.push('\n');
        }
    }
    (!out.is_empty()).then(|| format!("{RECALL_HEADER}\n{out}"))
}

/// What a session was given, kept in a file of its own so that the next
/// hook of the session knows.
pub struct Given {
    path: PathBuf,
    pub notes: BTreeSet<String>,
}

impl Given {
    /// What `session` was given, from the files under `dir`. Files of
    /// sessions not heard from for a week are removed on the way.
    pub fn load(dir: &Path, session: &str) -> Given {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let old = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| SystemTime::now().duration_since(t).ok())
                    .is_some_and(|age| age > KEPT);
                if old {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        let path = dir.join(format!(
            "{}.json",
            ensemble_core::state::labelled_text(session)
        ));
        let notes = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Given { path, notes }
    }

    pub fn save(&self) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let json = serde_json::to_string(&self.notes).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, json)
            .map_err(|e| format!("cannot write {}: {e}", self.path.display()))
    }
}

/// The file `cli` reads its hooks from: the user's own for every project
/// (`global`), else the one of the project in `dir` that is not committed
/// where the CLI has one.
pub fn settings_path(cli: &str, global: bool, home: &Path, dir: &Path) -> Result<PathBuf, String> {
    match (cli, global) {
        (CLAUDE_CODE_AGENT, true) => Ok(home.join(".claude/settings.json")),
        (CLAUDE_CODE_AGENT, false) => Ok(dir.join(".claude/settings.local.json")),
        (CODEX_AGENT, true) => Ok(home.join(".codex/hooks.json")),
        (CODEX_AGENT, false) => Ok(dir.join(".codex/hooks.json")),
        (other, _) => Err(format!(
            "{other} has no command hooks Ensemble can set up; its sessions get knowledge through the MCP tools. Hooks: {}.",
            CLIS.join(", ")
        )),
    }
}

/// Whether a hook handler is one Ensemble put there.
fn ours(handler: &Value) -> bool {
    handler["command"]
        .as_str()
        .is_some_and(|c| c.contains(" hook --hub "))
}

/// `settings` with `command` as the hook of both events, in place of any
/// hook of Ensemble's already there; every other setting and hook is kept.
pub fn install(mut settings: Value, command: &str) -> Result<Value, String> {
    if settings.is_null() {
        settings = json!({});
    }
    let hooks = settings
        .as_object_mut()
        .ok_or("the settings file is not a JSON object")?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("its `hooks` is not a JSON object")?;
    for event in [PROMPT_EVENT, START_EVENT] {
        let groups = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| format!("its `hooks.{event}` is not a list"))?;
        for group in groups.iter_mut() {
            if let Some(handlers) = group["hooks"].as_array_mut() {
                handlers.retain(|h| !ours(h));
            }
        }
        groups.retain(|g| g["hooks"].as_array().is_none_or(|h| !h.is_empty()));
        let handler = json!({"type": "command", "command": command, "timeout": TIMEOUT_SECONDS});
        groups.push(if event == START_EVENT {
            json!({"matcher": "startup|resume|clear|compact", "hooks": [handler]})
        } else {
            json!({"hooks": [handler]})
        });
    }
    Ok(settings)
}

/// Write Ensemble's hooks into the settings file at `path`, keeping what else
/// it holds.
pub fn install_into(path: &Path, command: &str) -> Result<(), String> {
    let settings = match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Value::Null,
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            format!(
                "{} is not JSON ({e}); fix it, then run this again",
                path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let settings = install(settings, command).map_err(|e| format!("{}: {e}", path.display()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    std::fs::write(path, format!("{text}\n"))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// The command a CLI runs for each hook event.
pub fn hook_command(ensemble: &str, hub: &Path, env_file: Option<&Path>) -> String {
    let word = |s: &str| crate::setup::shell_word(s);
    let mut command = format!(
        "{} hook --hub {}",
        word(ensemble),
        word(&hub.display().to_string())
    );
    if let Some(env_file) = env_file {
        command.push_str(&format!(
            " --env-file {}",
            word(&env_file.display().to_string())
        ));
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_core::workspace::testing::TempDir;

    fn prompt(text: &str) -> Event {
        read_event(
            &json!({"hook_event_name": PROMPT_EVENT, "session_id": "s1", "cwd": "/w/shop", "prompt": text})
                .to_string(),
        )
        .unwrap()
    }

    #[test]
    fn a_prompt_is_the_topic_unless_it_is_a_word_or_two() {
        assert_eq!(
            prompt("move the deploy to Monday").topic().as_deref(),
            Some("move the deploy to Monday")
        );
        assert_eq!(prompt("yes go").topic(), None);
        let start = read_event(
            &json!({"hook_event_name": START_EVENT, "session_id": "s1", "cwd": "/w/web-shop", "source": "compact"})
                .to_string(),
        )
        .unwrap();
        assert_eq!(start.topic().as_deref(), Some("web shop"));
        assert!(start.forgets());
        assert!(!prompt("move the deploy to Monday").forgets());
        assert!(read_event("{}").is_err());
    }

    #[test]
    fn a_note_is_given_once_per_session() {
        let answer = format!(
            "{RECALL_HEADER}\n\n## Deploys (decision, [[deploys]])\nFridays.\n\n## Db (fact, [[db]])\nPostgres.\n"
        );
        let mut given = BTreeSet::new();
        let first = fresh_notes(&answer, &mut given).unwrap();
        assert!(
            first.starts_with(RECALL_HEADER)
                && first.contains("Fridays.")
                && first.contains("Postgres.")
        );
        assert_eq!(fresh_notes(&answer, &mut given), None);
        given.remove("Db (fact, [[db]])");
        let again = fresh_notes(&answer, &mut given).unwrap();
        assert!(
            again.contains("Postgres.") && !again.contains("Fridays."),
            "{again}"
        );
        assert_eq!(fresh_notes("No note is about 'x'.", &mut given), None);
    }

    #[test]
    fn what_a_session_was_given_is_kept_between_hooks() {
        let tmp = TempDir::new();
        let mut given = Given::load(&tmp.0, "abc/1");
        given.notes.insert("Deploys".into());
        given.save().unwrap();
        assert!(Given::load(&tmp.0, "abc/1").notes.contains("Deploys"));
        assert!(Given::load(&tmp.0, "other").notes.is_empty());
    }

    #[test]
    fn installing_keeps_other_hooks_and_replaces_ensembles_own() {
        let settings = json!({
            "model": "opus",
            "hooks": {
                "UserPromptSubmit": [{"hooks": [
                    {"type": "command", "command": "lint-prompt"},
                    {"type": "command", "command": "ensemble hook --hub /old"}
                ]}],
                "Stop": [{"hooks": [{"type": "command", "command": "notify"}]}]
            }
        });
        let out = install(settings, "ensemble hook --hub /new").unwrap();
        assert_eq!(out["model"], "opus");
        assert_eq!(out["hooks"]["Stop"][0]["hooks"][0]["command"], "notify");
        let prompt = out["hooks"][PROMPT_EVENT].as_array().unwrap();
        assert_eq!(prompt.len(), 2, "{prompt:?}");
        assert_eq!(prompt[0]["hooks"][0]["command"], "lint-prompt");
        assert_eq!(prompt[0]["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(prompt[1]["hooks"][0]["command"], "ensemble hook --hub /new");
        assert_eq!(prompt[1]["hooks"][0]["timeout"], TIMEOUT_SECONDS);
        let start = &out["hooks"][START_EVENT][0];
        assert_eq!(start["matcher"], "startup|resume|clear|compact");
        // Installing twice leaves one.
        let twice = install(out.clone(), "ensemble hook --hub /new").unwrap();
        assert_eq!(twice, out);
        assert!(install(json!([1]), "x").is_err());
    }

    #[test]
    fn hooks_go_in_the_clis_own_files() {
        let (home, dir) = (Path::new("/h"), Path::new("/w/shop"));
        assert_eq!(
            settings_path(CLAUDE_CODE_AGENT, false, home, dir).unwrap(),
            Path::new("/w/shop/.claude/settings.local.json")
        );
        assert_eq!(
            settings_path(CODEX_AGENT, true, home, dir).unwrap(),
            Path::new("/h/.codex/hooks.json")
        );
        assert!(settings_path("opencode", false, home, dir).is_err());
        let tmp = TempDir::new();
        let path = tmp.0.join("a/settings.json");
        install_into(&path, "ensemble hook --hub '/my hub'").unwrap();
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            written["hooks"][PROMPT_EVENT][0]["hooks"][0]["command"],
            "ensemble hook --hub '/my hub'"
        );
        assert_eq!(
            hook_command("ensemble", Path::new("/my hub"), None),
            "ensemble hook --hub '/my hub'"
        );
    }
}
