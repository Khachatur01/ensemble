//! What the inbox needs from a tracker, whatever keeps the issues: the issues
//! assigned to a person, and a way to write their status and comments back.
//! Each tracker (local markdown files now; GitLab, GitHub, Jira later) is one
//! implementation.

use serde::{Deserialize, Serialize};

/// Where an issue stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Waiting for someone to start it.
    Open,
    /// Being worked on.
    InProgress,
    /// Stopped: a question waits for an answer, or the work failed.
    Blocked,
    /// The work is done and waits for the person to look at it.
    Review,
    /// Closed, by the person. Ensemble never sets it.
    Done,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::InProgress => "in-progress",
            Status::Blocked => "blocked",
            Status::Review => "review",
            Status::Done => "done",
        }
    }
}

/// One issue, as the tracker has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub id: String,
    pub title: String,
    /// The person it is assigned to, as the tracker names them.
    pub assignee: String,
    pub labels: Vec<String>,
    /// Which project it belongs to; the hub maps it to a folder.
    pub project: Option<String>,
    pub status: Status,
    /// What the person wrote, without the comments.
    pub body: String,
}

/// One comment on an issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub author: String,
    /// `2026-11-10T12:00:00Z`.
    pub at: String,
    pub text: String,
}

/// A tracker, as the inbox uses it.
pub trait IssueTracker: Sync {
    /// The issues assigned to `user` that carry `label`.
    fn assigned(&self, user: &str, label: &str) -> Vec<Issue>;
    fn issue(&self, id: &str) -> Result<Issue, String>;
    fn set_status(&self, id: &str, status: Status) -> Result<(), String>;
    fn comment(&self, id: &str, author: &str, text: &str) -> Result<(), String>;
    /// Its comments, oldest first.
    fn comments(&self, id: &str) -> Result<Vec<Comment>, String>;
    /// Issues that could not be read, and why, for a person to fix.
    fn unreadable(&self) -> Vec<(String, String)>;
}
