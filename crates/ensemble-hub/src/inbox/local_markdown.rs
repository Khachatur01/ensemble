//! A tracker that is a folder of markdown files, one issue per `<id>.md`:
//!
//! ```markdown
//! ---
//! title: Fix the login redirect
//! assignee: ana
//! labels: [ensemble, ensemble:coder]
//! project: shop
//! status: open
//! ---
//! What is wrong and what is wanted.
//!
//! ## Comment by ensemble at 2026-11-10T12:00:00Z
//!
//! Picked up as task 3.
//! ```
//!
//! People write the issues, so Ensemble changes as little as it can: the
//! `status:` line, and comments appended at the end. Everything else is kept
//! byte for byte. The folder is often a git repository a team shares; pulling
//! and pushing it is theirs to do.

use super::tracker::{Comment, Issue, IssueTracker, Status};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

/// What starts a comment: `## Comment by <author> at <utc>`.
const COMMENT: &str = "## Comment by ";

pub struct LocalMarkdown {
    dir: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Head {
    title: String,
    assignee: String,
    labels: Vec<String>,
    project: Option<String>,
    status: Status,
}

impl LocalMarkdown {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        LocalMarkdown { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, id: &str) -> Result<PathBuf, String> {
        let safe = !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
            && !id.starts_with('.');
        if safe {
            Ok(self.dir.join(format!("{id}.md")))
        } else {
            Err(format!("'{id}' is not an issue id"))
        }
    }

    /// Every issue in the folder, and the files that could not be read.
    fn all(&self) -> (Vec<Issue>, Vec<(String, String)>) {
        let mut files: Vec<PathBuf> = fs::read_dir(&self.dir)
            .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
            .unwrap_or_default();
        files.sort();
        let (mut issues, mut bad) = (Vec::new(), Vec::new());
        for file in files {
            if file.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let id = file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            match fs::read_to_string(&file)
                .map_err(|e| e.to_string())
                .and_then(|text| parse(&id, &text))
            {
                Ok(issue) => issues.push(issue),
                Err(why) => bad.push((id, why)),
            }
        }
        (issues, bad)
    }

    fn read(&self, id: &str) -> Result<String, String> {
        let path = self.path(id)?;
        fs::read_to_string(&path).map_err(|e| format!("issue {id}: {e}"))
    }

    fn write(&self, id: &str, text: &str) -> Result<(), String> {
        let path = self.path(id)?;
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        fs::write(&tmp, text)
            .and_then(|()| fs::rename(&tmp, &path))
            .map_err(|e| format!("issue {id}: {e}"))
    }
}

impl IssueTracker for LocalMarkdown {
    fn assigned(&self, user: &str, label: &str) -> Vec<Issue> {
        self.all()
            .0
            .into_iter()
            .filter(|i| i.assignee == user && i.labels.iter().any(|l| l == label))
            .collect()
    }

    fn labelled(&self, label: &str) -> Vec<Issue> {
        self.all()
            .0
            .into_iter()
            .filter(|i| i.labels.iter().any(|l| l == label))
            .collect()
    }

    fn issue(&self, id: &str) -> Result<Issue, String> {
        parse(id, &self.read(id)?)
    }

    fn set_status(&self, id: &str, status: Status) -> Result<(), String> {
        let text = self.read(id)?;
        let (head, rest) = split(&text).ok_or_else(|| format!("issue {id}: no front matter"))?;
        let mut found = false;
        let mut lines: Vec<String> = head
            .lines()
            .map(|line| {
                if !found && line.trim_start() == line && line.starts_with("status:") {
                    found = true;
                    format!("status: {}", status.as_str())
                } else {
                    line.to_string()
                }
            })
            .collect();
        if !found {
            lines.push(format!("status: {}", status.as_str()));
        }
        self.write(id, &format!("---\n{}\n---\n{rest}", lines.join("\n")))
    }

    fn comment(&self, id: &str, author: &str, text: &str) -> Result<(), String> {
        let mut file = self.read(id)?;
        if !file.ends_with('\n') {
            file.push('\n');
        }
        let at = ensemble_core::time::utc(std::time::SystemTime::now());
        file.push_str(&format!("\n{COMMENT}{author} at {at}\n\n{}\n", text.trim()));
        self.write(id, &file)
    }

    fn comments(&self, id: &str) -> Result<Vec<Comment>, String> {
        let text = self.read(id)?;
        let (_, rest) = split(&text).ok_or_else(|| format!("issue {id}: no front matter"))?;
        Ok(comments_in(rest))
    }

    fn unreadable(&self) -> Vec<(String, String)> {
        self.all().1
    }
}

/// The front matter, without its `---` lines, and what follows it.
fn split(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---\n")?;
    match rest.find("\n---\n") {
        Some(end) => Some((&rest[..end], &rest[end + 5..])),
        None => rest.strip_suffix("\n---").map(|head| (head, "")),
    }
}

fn parse(id: &str, text: &str) -> Result<Issue, String> {
    let (head, rest) = split(text).ok_or("no front matter between --- lines")?;
    let head: Head = serde_norway::from_str(head).map_err(|e| e.to_string())?;
    let body = match rest.find(&format!("\n{COMMENT}")) {
        Some(at) => &rest[..at],
        None if rest.starts_with(COMMENT) => "",
        None => rest,
    };
    Ok(Issue {
        id: id.to_string(),
        title: head.title,
        assignee: head.assignee,
        labels: head.labels,
        project: head.project,
        status: head.status,
        body: body.trim().to_string(),
    })
}

/// The comments in what follows the front matter.
fn comments_in(rest: &str) -> Vec<Comment> {
    let mut out: Vec<Comment> = Vec::new();
    for line in rest.lines() {
        if let Some(by) = line.strip_prefix(COMMENT)
            && let Some((author, at)) = by.rsplit_once(" at ")
        {
            out.push(Comment {
                author: author.trim().to_string(),
                at: at.trim().to_string(),
                text: String::new(),
            });
        } else if let Some(last) = out.last_mut() {
            last.text.push_str(line);
            last.text.push('\n');
        }
    }
    for c in &mut out {
        c.text = c.text.trim().to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_core::workspace::testing::TempDir;

    const ISSUE: &str = "---\ntitle: Fix the login redirect\nassignee: ana\nlabels: [ensemble, \"ensemble:coder\"]\nproject: shop\nstatus: open\n# a note the person keeps\n---\nIt redirects to /home.\n\nWanted: back to where you were.\n";

    fn tracker(tmp: &TempDir, files: &[(&str, &str)]) -> LocalMarkdown {
        let dir = tmp.0.join("issues");
        fs::create_dir_all(&dir).unwrap();
        for (name, text) in files {
            fs::write(dir.join(name), text).unwrap();
        }
        LocalMarkdown::new(dir)
    }

    #[test]
    fn an_issue_is_read_from_its_front_matter_and_body() {
        let tmp = TempDir::new();
        let t = tracker(&tmp, &[("12.md", ISSUE)]);
        let issue = t.issue("12").unwrap();
        assert_eq!(issue.title, "Fix the login redirect");
        assert_eq!(issue.assignee, "ana");
        assert_eq!(issue.labels, vec!["ensemble", "ensemble:coder"]);
        assert_eq!(issue.project.as_deref(), Some("shop"));
        assert_eq!(issue.status, Status::Open);
        assert_eq!(
            issue.body,
            "It redirects to /home.\n\nWanted: back to where you were."
        );
        assert!(
            t.issue("../12").is_err(),
            "an id is a file name, nothing else"
        );
    }

    #[test]
    fn only_the_status_line_changes_and_comments_are_appended() {
        let tmp = TempDir::new();
        let t = tracker(&tmp, &[("12.md", ISSUE)]);
        t.set_status("12", Status::InProgress).unwrap();
        let text = fs::read_to_string(t.dir().join("12.md")).unwrap();
        assert_eq!(text, ISSUE.replace("status: open", "status: in-progress"));

        t.comment("12", "ensemble", "Picked up as task 3.").unwrap();
        t.comment(
            "12",
            "ana",
            "Thanks.\n\n## Not a comment heading\nstill mine",
        )
        .unwrap();
        let comments = t.comments("12").unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].author, "ensemble");
        assert_eq!(comments[0].text, "Picked up as task 3.");
        assert_eq!(comments[1].author, "ana");
        assert!(
            comments[1].text.ends_with("still mine"),
            "{:?}",
            comments[1]
        );
        let issue = t.issue("12").unwrap();
        assert_eq!(issue.status, Status::InProgress);
        assert!(
            !issue.body.contains("Picked up"),
            "comments are not the body"
        );
        let text = fs::read_to_string(t.dir().join("12.md")).unwrap();
        assert!(text.starts_with(&ISSUE.replace("status: open", "status: in-progress")));
    }

    #[test]
    fn assigned_issues_are_the_users_with_the_label_and_bad_files_are_named() {
        let tmp = TempDir::new();
        let t = tracker(
            &tmp,
            &[
                ("1.md", ISSUE),
                ("2.md", &ISSUE.replace("assignee: ana", "assignee: bob")),
                (
                    "3.md",
                    &ISSUE.replace("[ensemble, \"ensemble:coder\"]", "[bug]"),
                ),
                ("4.md", "no front matter"),
                ("5.md", &ISSUE.replace("status: open", "status: someday")),
                ("notes.txt", "not an issue"),
            ],
        );
        let ids: Vec<String> = t
            .assigned("ana", "ensemble")
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(ids, vec!["1"]);
        let bad: Vec<String> = t.unreadable().into_iter().map(|(id, _)| id).collect();
        assert_eq!(bad, vec!["4", "5"]);
    }
}
