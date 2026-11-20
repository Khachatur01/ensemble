//! The task inbox on a hub server. It watches the tracker, and keeps a task on
//! the hub for each issue it takes:
//!
//! 1. An open issue with the label, assigned to someone who is an account's
//!    tracker name and whose role may receive `tasks`, and naming its project
//!    (a repository's name), becomes a **task** of that account, for the
//!    agent its `<label>:<team>/<agent>` label names or in the backlog. The
//!    issue says so. With no such account, or no project, the issue is
//!    blocked, saying why, once.
//! 2. The task waits on the hub until one of the account's runners with the
//!    agent claims it, in a repository of the project's name in its
//!    workspace, cloned there first when it had none; the issue goes to
//!    `in-progress`, naming the runner. A runner with no copy of the
//!    project and no repository to clone asks where it is, as a question.
//! 3. What the task says comes back: a question (the issue goes to `blocked`,
//!    and the assignee's next comment is the answer), then its result
//!    (`review` or `blocked`). Ensemble never closes an issue.
//! 4. An issue closed, or without the label, cancels a task that has not
//!    ended, and is forgotten.
//!
//! Which issue became which task is kept in the hub's database
//! (`ensemble_store`), as **deliveries**, by the tracker's name and the
//! issue's id: a hub watches each of its trackers with an inbox of its own.

use super::tracker::{Issue, IssueTracker, Status};
use super::work::{self, AUTHOR, Update, reply_after};
use crate::board::{DbBoard, Recall, recall_query};
use crate::users::Users;
use ensemble_config::{Permission, Tracker};
use ensemble_core::state::HubDir;
use ensemble_store as store;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Where a delivery stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    /// No task: no account takes it, or it names no project; the issue says
    /// why.
    Unassigned,
    /// A task, waiting for a runner.
    Queued,
    /// A runner runs it.
    Running,
    /// The task asked a question, posted on the issue.
    Asking,
    /// The task ended, and its result is on the issue.
    Done,
}

/// One issue's task, and what was said about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub issue: String,
    /// The account the task is for; empty while unassigned.
    pub account: String,
    /// The project the task is in.
    pub project: String,
    pub stage: Stage,
    /// The task it became.
    pub task: Option<String>,
    /// How many comments the issue had after the hub's last one.
    pub comments: usize,
    /// The question last posted, so it is posted once.
    pub asked: Option<String>,
}

/// Which issue of which tracker became which task, in the trackers'
/// database (`<config-dir>/hub/trackers/`).
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tracker_deliveries (
    tracker TEXT NOT NULL,
    issue TEXT NOT NULL,
    data TEXT NOT NULL,
    PRIMARY KEY (tracker, issue)
);
";

/// The deliveries of the tracker named `tracker` kept in `dir`, by issue.
fn load(dir: &std::path::Path, tracker: &str) -> Result<BTreeMap<String, Delivery>, String> {
    store::with(dir, SCHEMA, async |c| {
        let mut out = BTreeMap::new();
        let rows = store::rows(
            c,
            "SELECT issue, data FROM tracker_deliveries WHERE tracker = ?1",
            vec![store::Value::Text(tracker.into())],
        )
        .await?;
        for row in rows {
            let (Some(issue), Some(data)) = (store::text(&row[0]), store::text(&row[1])) else {
                continue;
            };
            let delivery = serde_json::from_str(&data)
                .map_err(|e| format!("the delivery of issue {issue} is unreadable: {e}"))?;
            out.insert(issue, delivery);
        }
        Ok(out)
    })
}

/// A hub server's task inbox, for one tracker.
pub struct ServerInbox {
    /// The tracker's name in `connectors.tasks.trackers`.
    name: String,
    /// The tracker's settings: its label, and how often it is looked at.
    settings: Tracker,
    tracker: Box<dyn IssueTracker + Send>,
    roles: BTreeMap<String, Vec<Permission>>,
    /// The hub's folder: its users in its core, their tasks, and the
    /// deliveries in the trackers'.
    dir: HubDir,
    /// Held for every pass, so the tracker and the deliveries change one step
    /// at a time.
    deliveries: Mutex<BTreeMap<String, Delivery>>,
    /// The notes related to a task it makes, kept with the task.
    notes: Option<Box<Recall>>,
}

impl ServerInbox {
    pub fn new(
        name: &str,
        settings: &Tracker,
        roles: &BTreeMap<String, Vec<Permission>>,
        dir: &HubDir,
    ) -> Result<Self, String> {
        let deliveries = load(&dir.trackers(), name)?;
        Ok(ServerInbox {
            name: name.to_string(),
            tracker: super::tracker_for(settings),
            settings: settings.clone(),
            roles: roles.clone(),
            dir: dir.clone(),
            deliveries: Mutex::new(deliveries),
            notes: None,
        })
    }

    /// Keep in each task it makes the notes `notes` recalls for it.
    pub fn with_notes(
        mut self,
        notes: impl Fn(&str, &str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.notes = Some(Box::new(notes));
        self
    }

    pub fn tracker(&self) -> &dyn IssueTracker {
        self.tracker.as_ref()
    }

    /// The deliveries, by issue.
    pub fn deliveries(&self) -> BTreeMap<String, Delivery> {
        self.deliveries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Keep `deliveries`, in one transaction.
    fn save(&self, deliveries: &BTreeMap<String, Delivery>) -> Result<(), String> {
        let tracker = store::Value::Text(self.name.clone());
        let mut statements = vec![(
            "DELETE FROM tracker_deliveries WHERE tracker = ?1",
            vec![tracker.clone()],
        )];
        for (issue, delivery) in deliveries {
            let data = serde_json::to_string(delivery).map_err(|e| e.to_string())?;
            statements.push((
                "INSERT INTO tracker_deliveries (tracker, issue, data) VALUES (?1, ?2, ?3)",
                vec![
                    tracker.clone(),
                    store::Value::Text(issue.clone()),
                    store::Value::Text(data),
                ],
            ));
        }
        store::with(&self.dir.trackers(), SCHEMA, async |c| {
            store::transaction(c, statements).await
        })
    }

    /// The account the issue is for: active, `tracker_name` its assignee, with
    /// a role that may receive tasks.
    fn account_for(&self, users: &Users, issue: &Issue) -> Option<String> {
        let user = users.assigned_to(&issue.assignee)?;
        self.roles
            .get(&user.role)
            .is_some_and(|p| p.contains(&Permission::Tasks))
            .then(|| user.name.clone())
    }

    fn board(&self, d: &Delivery) -> DbBoard {
        DbBoard::new(&self.dir.tasks(), &d.account, &d.project)
    }

    /// One look at the tracker and at the tasks. Returns a line for each
    /// thing it did.
    pub fn pass(&self) -> Result<Vec<String>, String> {
        let users = Users::load(&self.dir.core()).map_err(String::from)?;
        let mut deliveries = self.deliveries.lock().unwrap_or_else(|e| e.into_inner());
        let mut said = Vec::new();
        let issues = self.tracker.labelled(&self.settings.label);
        for issue in &issues {
            if issue.status == Status::Done {
                continue;
            }
            let fresh = match deliveries.get(&issue.id) {
                None => issue.status == Status::Open,
                // Unassigned, then set open again: look for an account anew.
                Some(d) => d.stage == Stage::Unassigned && issue.status == Status::Open,
            };
            if fresh {
                let d = self.take(&users, issue, &mut said)?;
                deliveries.insert(issue.id.clone(), d);
                continue;
            }
            if let Some(d) = deliveries.get_mut(&issue.id) {
                self.follow(issue, d, &mut said)?;
            }
        }
        // Closed, or without the label: an unfinished task stops.
        let open: Vec<&str> = issues
            .iter()
            .filter(|i| i.status != Status::Done)
            .map(|i| i.id.as_str())
            .collect();
        for d in deliveries
            .values()
            .filter(|d| !open.contains(&d.issue.as_str()))
        {
            if let (Some(task), Stage::Queued | Stage::Running | Stage::Asking) = (&d.task, d.stage)
            {
                self.board(d).stop(task)?;
                said.push(format!("issue {} closed: task {task} cancelled", d.issue));
            }
        }
        deliveries.retain(|id, _| open.contains(&id.as_str()));
        self.save(&deliveries)?;
        Ok(said)
    }

    /// Make `issue` a task of the account it is for, or say on it why not.
    fn take(
        &self,
        users: &Users,
        issue: &Issue,
        said: &mut Vec<String>,
    ) -> Result<Delivery, String> {
        let mut d = Delivery {
            issue: issue.id.clone(),
            account: String::new(),
            project: String::new(),
            stage: Stage::Unassigned,
            task: None,
            comments: 0,
            asked: None,
        };
        let Some(account) = self.account_for(users, issue) else {
            self.say(
                issue,
                &format!(
                    "Ensemble cannot take this issue: no account on the hub is {} in the tracker with a role that may receive tasks. Set the account's tracker name, then set the status back to open.",
                    issue.assignee
                ),
                Status::Blocked,
                &mut d,
            )?;
            said.push(format!("issue {}: no account", issue.id));
            return Ok(d);
        };
        let Some(project) = issue
            .project
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty() && !p.contains(['/', '\\']))
        else {
            self.say(
                issue,
                "Ensemble cannot take this issue: it names no project, the name of the folder its work is in. Set its project, then set the status back to open.",
                Status::Blocked,
                &mut d,
            )?;
            said.push(format!("issue {}: no project", issue.id));
            return Ok(d);
        };
        d.account = account.clone();
        d.project = project.to_string();
        let mut ticket = work::ticket_for(issue, &self.settings.label);
        ticket.notes = self
            .notes
            .as_ref()
            .and_then(|recall| recall(&account, &recall_query(&ticket)));
        let task = self.board(&d).add(&ticket)?;
        let whom = if ticket.agent.is_empty() {
            "in the backlog, for the helper team to assign".to_string()
        } else {
            format!("for {}", ticket.agent)
        };
        self.say(
            issue,
            &format!(
                "Picked up as task {task}, {whom}. It runs on the first of {account}'s runners that has the agent, in its folder named {project}; a runner without one asks here which repository to clone."
            ),
            Status::Open,
            &mut d,
        )?;
        said.push(format!("issue {} -> task {task} of {account}", issue.id));
        d.stage = Stage::Queued;
        d.task = Some(task);
        Ok(d)
    }

    /// Say on `issue` what its task did since the last look, and pass the
    /// assignee's answer to a question on.
    fn follow(
        &self,
        issue: &Issue,
        d: &mut Delivery,
        said: &mut Vec<String>,
    ) -> Result<(), String> {
        let Some(task) = d.task.clone() else {
            return Ok(());
        };
        if d.stage == Stage::Done {
            return Ok(());
        }
        let Some(detail) = self.board(d).detail(&task)? else {
            return Ok(());
        };
        if let Some(outcome) = &detail.outcome {
            let update = Update::Result {
                status: outcome.status,
                reply: outcome.reply().trim().to_string(),
            };
            let (comment, status) = work::on_issue(&task, &update);
            self.say(issue, &comment, status, d)?;
            d.stage = Stage::Done;
            said.push(format!("task {task} {}", outcome.status.as_str()));
            return Ok(());
        }
        if let Some(q) = &detail.task.question {
            if d.asked.as_ref() != Some(q) {
                let (comment, status) = work::on_issue(&task, &Update::Question(q.clone()));
                self.say(issue, &comment, status, d)?;
                d.asked = Some(q.clone());
                d.stage = Stage::Asking;
                said.push(format!("task {task} asked a question"));
                return Ok(());
            }
            let comments = self.tracker.comments(&issue.id)?;
            if let Some(answer) = reply_after(&comments, d.comments, &issue.assignee) {
                self.board(d).reply(&task, &answer)?;
                d.comments = comments.len();
                d.stage = Stage::Queued;
                if issue.status != Status::InProgress {
                    self.tracker.set_status(&issue.id, Status::InProgress)?;
                }
                said.push(format!("issue {}: answered", issue.id));
            }
            return Ok(());
        }
        if let Some(node) = &detail.task.running_on
            && d.stage != Stage::Running
        {
            self.say(
                issue,
                &format!("Task {task} runs on {}'s computer {node}.", d.account),
                Status::InProgress,
                d,
            )?;
            d.stage = Stage::Running;
            said.push(format!("task {task} runs on {node}"));
        }
        Ok(())
    }

    /// Comment on an issue and set its status, then note how many comments it
    /// has, so a reply is one that comes after.
    fn say(
        &self,
        issue: &Issue,
        text: &str,
        status: Status,
        d: &mut Delivery,
    ) -> Result<(), String> {
        self.tracker.comment(&issue.id, AUTHOR, text)?;
        if issue.status != status {
            self.tracker.set_status(&issue.id, status)?;
        }
        d.comments = self.tracker.comments(&issue.id)?.len();
        Ok(())
    }

    /// Look at the tracker every `poll_seconds` until `stop` is set.
    pub fn work(&self, stop: &AtomicBool, say: &(dyn Fn(&str) + Sync)) {
        let every = Duration::from_secs(u64::from(self.settings.poll_seconds));
        while !stop.load(Ordering::Relaxed) {
            match self.pass() {
                Ok(lines) => lines
                    .iter()
                    .for_each(|l| say(&format!("inbox {}: {l}", self.name))),
                Err(e) => say(&format!("inbox {}: {e}", self.name)),
            }
            let until = Instant::now() + every;
            while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_config::IssueTracker as Kind;
    use ensemble_core::workspace::testing::TempDir;
    use ensemble_tasks::board::{Claim, End};
    use ensemble_tasks::tasks::Status as TaskStatus;

    struct World {
        tmp: TempDir,
        inbox: ServerInbox,
    }

    fn world() -> World {
        let tmp = TempDir::new();
        std::fs::create_dir_all(tmp.0.join("issues")).unwrap();
        let roles = BTreeMap::from([
            (
                "member".to_string(),
                vec![Permission::Recall, Permission::Tasks],
            ),
            ("reader".to_string(), vec![Permission::Recall]),
        ]);
        let state = HubDir::new(tmp.0.join("state"));
        let mut users = Users::load(&state.core()).unwrap();
        users.add("ana", "member", &roles).unwrap();
        users.set_tracker_user("ana", Some("ana.p")).unwrap();
        users.add("rex", "reader", &roles).unwrap();
        users.save().unwrap();
        let tracker = Tracker {
            kind: Kind::LocalMarkdown {
                dir: tmp.0.join("issues"),
            },
            label: "ensemble".into(),
            poll_seconds: 1,
        };
        let inbox = ServerInbox::new("issues", &tracker, &roles, &state).unwrap();
        World { tmp, inbox }
    }

    impl World {
        fn issue(&self, id: &str, assignee: &str, project: &str) {
            std::fs::write(
                self.tmp.0.join("issues").join(format!("{id}.md")),
                format!("---\ntitle: Fix the login redirect\nassignee: {assignee}\nlabels: [ensemble, \"ensemble:web/coder\"]\nproject: {project}\nstatus: open\n---\nIt redirects to /home.\n"),
            )
            .unwrap();
        }

        fn status(&self, id: &str) -> Status {
            self.inbox.tracker().issue(id).unwrap().status
        }

        fn last_comment(&self, id: &str) -> String {
            let comments = self.inbox.tracker().comments(id).unwrap();
            comments.last().map(|c| c.text.clone()).unwrap_or_default()
        }

        /// Ana's tasks in the project shop.
        fn board(&self) -> DbBoard {
            DbBoard::new(&self.inbox.dir.tasks(), "ana", "shop")
        }
    }

    #[test]
    fn an_issue_becomes_a_task_of_its_assignee_that_their_node_takes() {
        let w = world();
        w.issue("1", "ana.p", "shop");
        w.inbox.pass().unwrap();
        assert!(
            w.last_comment("1")
                .contains("Picked up as task 0001-fix-the-login-redirect, for web/coder"),
            "{}",
            w.last_comment("1")
        );
        assert_eq!(w.status("1"), Status::Open);
        let tasks = w.board().list().unwrap();
        assert_eq!(tasks.len(), 1);
        let ticket = &tasks[0].ticket;
        assert_eq!(
            (ticket.agent.as_str(), ticket.text.as_str()),
            ("web/coder", "It redirects to /home.")
        );
        assert!(w.inbox.pass().unwrap().is_empty(), "said once");
        assert_eq!(w.board().list().unwrap().len(), 1, "made once");

        assert_eq!(
            w.board().take(&ticket.id, "laptop", 60, true).unwrap(),
            Claim::Taken
        );
        w.inbox.pass().unwrap();
        assert_eq!(w.status("1"), Status::InProgress);
        assert!(
            w.last_comment("1")
                .contains("runs on ana's computer laptop")
        );
        assert!(w.inbox.deliveries().contains_key("1"));
    }

    #[test]
    fn a_question_goes_on_the_issue_and_the_assignees_comment_comes_back() {
        let w = world();
        w.issue("1", "ana.p", "shop");
        w.inbox.pass().unwrap();
        let id = w.board().list().unwrap()[0].ticket.id.clone();
        w.board().take(&id, "laptop", 60, true).unwrap();
        w.board()
            .end(
                &id,
                "laptop",
                End::Ask {
                    question: "Which page?".into(),
                    reply: "r".into(),
                    repository: false,
                },
                None,
            )
            .unwrap();
        w.inbox.pass().unwrap();
        assert_eq!(w.status("1"), Status::Blocked);
        assert!(w.last_comment("1").contains("Which page?"));
        w.inbox
            .tracker()
            .comment("1", "bob", "not mine to say")
            .unwrap();
        w.inbox.pass().unwrap();
        assert!(
            w.board().list().unwrap()[0].question.is_some(),
            "only the assignee answers"
        );
        w.inbox
            .tracker()
            .comment("1", "ana.p", "The one they came from.")
            .unwrap();
        w.inbox.pass().unwrap();
        assert_eq!(w.status("1"), Status::InProgress);
        let (questions, _) = w
            .board()
            .detail(&id)
            .unwrap()
            .unwrap()
            .conversation
            .unwrap();
        assert!(
            questions.contains("### Answer\nThe one they came from."),
            "{questions}"
        );

        w.board().take(&id, "laptop", 60, true).unwrap();
        w.board()
            .end(
                &id,
                "laptop",
                End::Result {
                    status: TaskStatus::Done,
                    agent: "web/coder".into(),
                    started: "2026-11-17T00:00:00Z".into(),
                    extra: Vec::new(),
                    text: "Fixed in auth.rs.".into(),
                },
                None,
            )
            .unwrap();
        w.inbox.pass().unwrap();
        assert_eq!(w.status("1"), Status::Review);
        assert!(
            w.last_comment("1")
                .contains(&format!("Task {id} is done:\n\nFixed in auth.rs."))
        );
    }

    #[test]
    fn nobody_or_no_project_is_said_once_and_a_closed_issue_cancels_its_task() {
        let w = world();
        w.issue("1", "someone.else", "shop");
        w.issue("2", "rex", "shop");
        w.issue("3", "ana.p", "a/b");
        w.inbox.pass().unwrap();
        assert_eq!(w.status("1"), Status::Blocked);
        assert!(
            w.last_comment("1")
                .contains("no account on the hub is someone.else")
        );
        assert_eq!(w.status("2"), Status::Blocked, "rex may not receive tasks");
        assert!(w.last_comment("3").contains("names no project"));
        assert!(w.inbox.pass().unwrap().is_empty(), "said once");
        assert!(w.board().list().unwrap().is_empty());

        w.issue("4", "ana.p", "shop");
        w.inbox.pass().unwrap();
        w.inbox.tracker().set_status("4", Status::Done).unwrap();
        w.inbox.pass().unwrap();
        assert!(!w.inbox.deliveries().contains_key("4"), "closed, forgotten");
        assert_eq!(
            w.board().list().unwrap()[0].status,
            Some(TaskStatus::Cancelled)
        );
    }

    #[test]
    fn deliveries_are_kept_in_the_database() {
        let w = world();
        w.issue("1", "ana.p", "shop");
        w.inbox.pass().unwrap();
        let roles = w.inbox.roles.clone();
        let again = ServerInbox::new("issues", &w.inbox.settings, &roles, &w.inbox.dir).unwrap();
        assert_eq!(again.deliveries(), w.inbox.deliveries());
        assert_eq!(again.deliveries()["1"].stage, Stage::Queued);
    }

    #[test]
    fn two_trackers_keep_their_issues_apart_though_their_ids_are_the_same() {
        let w = world();
        std::fs::create_dir_all(w.tmp.0.join("work")).unwrap();
        let work = Tracker {
            kind: Kind::LocalMarkdown {
                dir: w.tmp.0.join("work"),
            },
            ..w.inbox.settings.clone()
        };
        let roles = w.inbox.roles.clone();
        let other = ServerInbox::new("work", &work, &roles, &w.inbox.dir).unwrap();
        w.issue("1", "ana.p", "shop");
        std::fs::write(
            w.tmp.0.join("work").join("1.md"),
            "---\ntitle: Add a page\nassignee: ana.p\nlabels: [ensemble]\nproject: shop\nstatus: open\n---\nAbout us.\n",
        )
        .unwrap();
        w.inbox.pass().unwrap();
        other.pass().unwrap();
        assert_eq!(w.board().list().unwrap().len(), 2, "one task of each");
        // Each keeps its own, and a pass of one leaves the other's alone.
        w.inbox.pass().unwrap();
        let again = ServerInbox::new("work", &work, &roles, &w.inbox.dir).unwrap();
        assert_eq!(again.deliveries(), other.deliveries());
        assert_ne!(other.deliveries()["1"].task, w.inbox.deliveries()["1"].task);
        other.pass().unwrap();
        assert_eq!(w.board().list().unwrap().len(), 2, "made once each");
    }
}
