//! Where the helper team's tasks come from.
//!
//! The queue (`harness::queue`) asks a [`Tracker`] for tickets and tells it how
//! they went; it does not know where they are kept. One implementation exists,
//! [`LocalMarkdown`]: a folder of markdown files in the state directory, one
//! directory per task. Others (GitLab, Jira, a project
//! management service) would implement the same trait; they are not built.

use crate::tasks::{Entry, Outcome, Ticket};
use std::io;
use std::path::Path;
use std::time::SystemTime;

/// The team's tasks, and the record of how each went.
pub trait Tracker: Sync {
    /// Where its files live, for messages and for worktrees.
    fn root(&self) -> &Path;
    /// Every task, with its outcome once it has one.
    fn scan(&self) -> Vec<Entry>;
    fn exists(&self, id: &str) -> bool;
    /// One ticket. The error says what is wrong with it.
    fn read(&self, id: &str) -> Result<Ticket, String>;
    /// Write a new task and return its id.
    fn create(&self, ticket: &Ticket) -> io::Result<String>;
    /// How a task ended, once it has.
    fn outcome(&self, id: &str) -> Option<Outcome>;
    /// Record how a task ended, once; a second write leaves the first.
    fn write_result(
        &self,
        id: &str,
        agent: &str,
        status: crate::tasks::Status,
        started: SystemTime,
        extra: &[(&str, String)],
        text: &str,
    ) -> io::Result<()>;
    /// What a run does as it goes.
    fn log(&self, id: &str, line: &str);
    fn set_state(&self, id: &str, state: &str, started: SystemTime);
    fn state(&self, id: &str) -> Option<String>;
    /// A question the agent stopped to ask, with the reply it came in.
    fn ask(&self, id: &str, question: &str, reply: &str) -> io::Result<()>;
    fn open_question(&self, id: &str) -> Option<String>;
    fn answer(&self, id: &str, answer: &str) -> Result<(), String>;
    fn conversation(&self, id: &str) -> Option<(String, String)>;
    /// Give a task to `agent`, from the backlog or from whoever has it.
    fn assign(&self, id: &str, agent: &str) -> io::Result<()>;
    /// Note that `from` handed a task to `to`, with what remains and what it had
    /// said.
    fn append_handoff(
        &self,
        id: &str,
        from: &str,
        to: &str,
        note: &str,
        reply: &str,
    ) -> io::Result<()>;
    /// What earlier agents handed on, for the next one.
    fn handoffs(&self, id: &str) -> Option<String>;
}

/// Tasks as markdown files: `tasks/<id>-<slug>/task.md`, with
/// `result.md` and `questions.md` beside it.
pub type LocalMarkdown = crate::tasks::Store;

impl Tracker for crate::tasks::Store {
    fn root(&self) -> &Path {
        crate::tasks::Store::root(self)
    }
    fn scan(&self) -> Vec<Entry> {
        crate::tasks::Store::scan(self)
    }
    fn exists(&self, id: &str) -> bool {
        crate::tasks::Store::exists(self, id)
    }
    fn read(&self, id: &str) -> Result<Ticket, String> {
        crate::tasks::Store::read(self, id)
    }
    fn create(&self, ticket: &Ticket) -> io::Result<String> {
        crate::tasks::Store::create(self, ticket)
    }
    fn outcome(&self, id: &str) -> Option<Outcome> {
        crate::tasks::Store::outcome(self, id)
    }
    fn write_result(
        &self,
        id: &str,
        agent: &str,
        status: crate::tasks::Status,
        started: SystemTime,
        extra: &[(&str, String)],
        text: &str,
    ) -> io::Result<()> {
        crate::tasks::Store::write_result(self, id, agent, status, started, extra, text)
    }
    fn log(&self, id: &str, line: &str) {
        crate::tasks::Store::log(self, id, line)
    }
    fn set_state(&self, id: &str, state: &str, started: SystemTime) {
        crate::tasks::Store::set_state(self, id, state, started)
    }
    fn state(&self, id: &str) -> Option<String> {
        crate::tasks::Store::state(self, id)
    }
    fn ask(&self, id: &str, question: &str, reply: &str) -> io::Result<()> {
        crate::tasks::Store::ask(self, id, question, reply)
    }
    fn open_question(&self, id: &str) -> Option<String> {
        crate::tasks::Store::open_question(self, id)
    }
    fn answer(&self, id: &str, answer: &str) -> Result<(), String> {
        crate::tasks::Store::answer(self, id, answer)
    }
    fn conversation(&self, id: &str) -> Option<(String, String)> {
        crate::tasks::Store::conversation(self, id)
    }
    fn assign(&self, id: &str, agent: &str) -> io::Result<()> {
        crate::tasks::Store::assign(self, id, agent)
    }
    fn append_handoff(
        &self,
        id: &str,
        from: &str,
        to: &str,
        note: &str,
        reply: &str,
    ) -> io::Result<()> {
        crate::tasks::Store::append_handoff(self, id, from, to, note, reply)
    }
    fn handoffs(&self, id: &str) -> Option<String> {
        crate::tasks::Store::handoffs(self, id)
    }
}

/// The tracker of the configured type. Today there is one.
pub fn for_config(tracker: crate::config::Tracker, store: &crate::tasks::Store) -> &dyn Tracker {
    match tracker {
        crate::config::Tracker::LocalMarkdown => store,
    }
}
