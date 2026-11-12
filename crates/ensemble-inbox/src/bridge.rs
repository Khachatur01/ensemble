//! The bridge between a tracker and the helper team. One pass:
//!
//! 1. An open issue assigned to the person, with the label, that the inbox has
//!    not seen becomes a task in the folder its project maps to: for the agent
//!    a `<label>:<agent>` label names, or in the backlog. The issue gets a
//!    comment and goes to `in-progress`.
//! 2. A question the task stopped to ask is posted as a comment and the issue
//!    goes to `blocked`; the person's next comment answers it.
//! 3. A result is posted as a comment, and the issue goes to `review` when the
//!    task is done, or `blocked` when it failed. The person closes it: Ensemble
//!    never sets `done`.
//! 4. An issue the person closed, or that lost the label, is forgotten. Its
//!    task is left alone.
//!
//! What the inbox has done is kept in `inbox.json` in its state directory, with
//! the lock that keeps one inbox per hub.

use crate::tracker::{Issue, IssueTracker, Status};
use crate::work;
use ensemble_config::{Personal, Tasks};
use ensemble_tasks::tasks::Store;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Who the inbox's comments are by.
pub const AUTHOR: &str = "ensemble";
const STATE_FILE: &str = "inbox.json";

/// Where a folder's tasks are kept: the per-user state directory, unless a
/// test says otherwise.
type StateFor = dyn Fn(&Path) -> Result<PathBuf, String> + Send + Sync;

pub struct Inbox {
    tasks: Tasks,
    /// The one person this embedded inbox works for, and their folders.
    personal: Personal,
    tracker: Box<dyn IssueTracker + Send>,
    /// The helper team's agents, which `<label>:<agent>` may name.
    agents: Vec<String>,
    /// Where `inbox.json` and the lock live.
    dir: PathBuf,
    state_for: Box<StateFor>,
}

/// What the inbox remembers of one issue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    /// The folder its work happens in, once it has one.
    pub folder: Option<PathBuf>,
    /// The task it became, once it has.
    pub task: Option<String>,
    /// The question last posted, while one waits.
    pub question: Option<String>,
    /// Whether its result was posted.
    pub reported: bool,
    /// How many comments it had after the inbox's last one.
    pub comments: usize,
}

impl Inbox {
    /// The inbox of a hub's `tasks`, for a helper team with `agents`, keeping
    /// what it has done in `dir`.
    ///
    /// Only an embedded hub's `tasks` name a person; a hub server's inbox runs
    /// in `ensemble hub serve`.
    pub fn new(tasks: &Tasks, agents: Vec<String>, dir: PathBuf) -> Result<Self, String> {
        let personal = tasks.personal.clone().ok_or(
            "this is a hub server's inbox: it runs in `ensemble hub serve`, and members' computers get its tasks over the link",
        )?;
        Ok(Inbox {
            tracker: crate::tracker_for(tasks),
            tasks: tasks.clone(),
            personal,
            agents,
            dir,
            state_for: Box::new(ensemble_core::state::dir_for),
        })
    }

    /// The person this inbox works for.
    pub fn personal(&self) -> &Personal {
        &self.personal
    }

    /// Keep each folder's tasks where `state_for` says.
    pub fn with_state_for(
        mut self,
        state_for: impl Fn(&Path) -> Result<PathBuf, String> + Send + Sync + 'static,
    ) -> Self {
        self.state_for = Box::new(state_for);
        self
    }

    pub fn tracker(&self) -> &dyn IssueTracker {
        self.tracker.as_ref()
    }

    pub fn settings(&self) -> &Tasks {
        &self.tasks
    }

    /// What the inbox has done, by issue id.
    pub fn seen(&self) -> BTreeMap<String, Seen> {
        std::fs::read_to_string(self.dir.join(STATE_FILE))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self, seen: &BTreeMap<String, Seen>) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| format!("cannot create {}: {e}", self.dir.display()))?;
        let path = self.dir.join(STATE_FILE);
        let tmp = path.with_extension("tmp");
        let json = serde_json::to_string_pretty(seen).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, json)
            .and_then(|()| std::fs::rename(&tmp, &path))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    /// Take the inbox's lock, or say which process has it.
    pub fn lock(&self) -> Result<ensemble_core::lock::WorkerLock, String> {
        ensemble_core::lock::acquire(&self.dir)
            .map_err(|pid| format!("process {pid} runs this hub's inbox already"))
    }

    /// One pass over the tracker. Returns a line for each thing it did.
    pub fn pass(&self) -> Result<Vec<String>, String> {
        let mut seen = self.seen();
        let mut said = Vec::new();
        let issues = self
            .tracker
            .assigned(&self.personal.user, &self.tasks.label);
        for issue in &issues {
            let entry = seen.entry(issue.id.clone()).or_default();
            let result = match (&entry.task, issue.status) {
                (_, Status::Done) => Ok(None),
                (None, Status::Open) => self.pick_up(issue, entry),
                (None, _) => Ok(None),
                (Some(task), _) => self.follow(issue, &task.clone(), entry),
            };
            match result {
                Ok(Some(line)) => said.push(line),
                Ok(None) => {}
                Err(e) => said.push(format!("issue {}: {e}", issue.id)),
            }
        }
        // Forget what the person closed or took the label off.
        seen.retain(|id, _| {
            issues
                .iter()
                .any(|i| &i.id == id && i.status != Status::Done)
        });
        self.save(&seen)?;
        Ok(said)
    }

    /// An open issue becomes a task, or is blocked with why it cannot.
    fn pick_up(&self, issue: &Issue, entry: &mut Seen) -> Result<Option<String>, String> {
        let folder = match issue.project.as_deref() {
            None => Err("it names no `project`, so there is no folder to work in".to_string()),
            Some(p) => match self.personal.projects.get(p) {
                None => Err(format!(
                    "its project '{p}' is not in the hub's `tasks.projects`, so there is no folder to work in"
                )),
                Some(dir) if !dir.is_dir() => Err(format!(
                    "its project '{p}' maps to {}, which is not a folder",
                    dir.display()
                )),
                Some(dir) => Ok(dir.clone()),
            },
        };
        let folder = match folder {
            Ok(folder) => folder,
            Err(why) => {
                self.say(issue, &format!("Ensemble cannot take this issue: {why}. Fix it and set the status back to open."), Status::Blocked, entry)?;
                return Ok(Some(format!("issue {} blocked: {why}", issue.id)));
            }
        };
        let store = Store::new(&(self.state_for)(&folder)?);
        let (task, comment) = work::make_task(
            &store,
            issue,
            &self.tasks.label,
            &self.agents,
            &self.personal.user,
            &format!("in {}", folder.display()),
        )?;
        entry.folder = Some(folder);
        entry.task = Some(task.clone());
        self.say(issue, &comment, Status::InProgress, entry)?;
        Ok(Some(format!("issue {} -> task {task}", issue.id)))
    }

    /// A task's question, the person's answer, or its result.
    fn follow(
        &self,
        issue: &Issue,
        task: &str,
        entry: &mut Seen,
    ) -> Result<Option<String>, String> {
        if entry.reported {
            return Ok(None);
        }
        let folder = entry
            .folder
            .clone()
            .ok_or("the inbox lost the task's folder")?;
        let store = Store::new(&(self.state_for)(&folder)?);
        match work::follow(&store, task, entry.question.as_deref()) {
            Some(update) => {
                let (comment, status) = work::on_issue(task, &update);
                self.say(issue, &comment, status, entry)?;
                return Ok(Some(match update {
                    work::Update::Result { .. } => {
                        entry.question = None;
                        entry.reported = true;
                        format!("issue {} -> {}", issue.id, status.as_str())
                    }
                    work::Update::Question(q) => {
                        entry.question = Some(q);
                        format!("issue {}: asked a question", issue.id)
                    }
                }));
            }
            None if entry.question.is_none() => return Ok(None),
            None => {}
        }
        let comments = self.tracker.comments(&issue.id)?;
        let Some(answer) = reply_after(&comments, entry.comments, &self.personal.user) else {
            return Ok(None);
        };
        work::answer(&store, task, &answer)?;
        entry.question = None;
        self.tracker.set_status(&issue.id, Status::InProgress)?;
        entry.comments = comments.len();
        Ok(Some(format!("issue {}: answered", issue.id)))
    }

    /// Comment on an issue and set its status, then note how many comments it
    /// has, so a reply is one that comes after.
    fn say(
        &self,
        issue: &Issue,
        text: &str,
        status: Status,
        entry: &mut Seen,
    ) -> Result<(), String> {
        self.tracker.comment(&issue.id, AUTHOR, text)?;
        self.tracker.set_status(&issue.id, status)?;
        entry.comments = self.tracker.comments(&issue.id)?.len();
        Ok(())
    }

    /// Pass over the tracker every `poll_seconds` until `stop` is set. Another
    /// process that holds the inbox does this already, and then this returns
    /// at once.
    pub fn work(&self, stop: &AtomicBool, say: &(dyn Fn(&str) + Sync)) {
        let Ok(_lock) = self.lock() else { return };
        let every = Duration::from_secs(u64::from(self.tasks.poll_seconds));
        while !stop.load(Ordering::Relaxed) {
            match self.pass() {
                Ok(lines) => lines.iter().for_each(|l| say(&format!("inbox: {l}"))),
                Err(e) => say(&format!("inbox: {e}")),
            }
            let until = Instant::now() + every;
            while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// The first comment by `person` after the first `seen` comments: their reply
/// to a question.
pub fn reply_after(
    comments: &[crate::tracker::Comment],
    seen: usize,
    person: &str,
) -> Option<String> {
    comments
        .iter()
        .skip(seen)
        .find(|c| c.author == person)
        .map(|c| c.text.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_config::IssueTracker as Kind;
    use ensemble_core::workspace::testing::TempDir;
    use ensemble_tasks::tasks::{Status as TaskStatus, Ticket};
    use std::time::SystemTime;

    struct World {
        tmp: TempDir,
        inbox: Inbox,
    }

    impl World {
        fn new() -> World {
            let tmp = TempDir::new();
            let shop = tmp.0.join("shop");
            std::fs::create_dir_all(&shop).unwrap();
            std::fs::create_dir_all(tmp.0.join("issues")).unwrap();
            let tasks = Tasks {
                tracker: Kind::LocalMarkdown {
                    dir: tmp.0.join("issues"),
                },
                label: "ensemble".into(),
                poll_seconds: 1,
                personal: Some(Personal {
                    user: "ana".into(),
                    projects: BTreeMap::from([("shop".to_string(), shop)]),
                }),
            };
            let states = tmp.0.join("states");
            let inbox = Inbox::new(&tasks, vec!["coder".into()], tmp.0.join("inbox"))
                .unwrap()
                .with_state_for(move |folder| Ok(states.join(folder.file_name().unwrap())));
            World { tmp, inbox }
        }

        fn issue(&self, id: &str, labels: &str, project: &str) {
            std::fs::write(
                self.tmp.0.join("issues").join(format!("{id}.md")),
                format!("---\ntitle: Fix the login redirect\nassignee: ana\nlabels: [{labels}]\nproject: {project}\nstatus: open\n---\nIt redirects to /home.\n"),
            )
            .unwrap();
        }

        fn store(&self) -> Store {
            Store::new(&self.tmp.0.join("states/shop"))
        }

        fn issue_now(&self, id: &str) -> Issue {
            self.inbox.tracker().issue(id).unwrap()
        }

        fn last_comment(&self, id: &str) -> String {
            let comments = self.inbox.tracker().comments(id).unwrap();
            comments.last().map(|c| c.text.clone()).unwrap_or_default()
        }
    }

    #[test]
    fn an_issue_becomes_a_task_for_the_named_agent_or_the_backlog() {
        let w = World::new();
        w.issue("1", "ensemble, \"ensemble:coder\"", "shop");
        w.issue("2", "ensemble, \"ensemble:painter\"", "shop");
        w.issue("3", "bug", "shop");
        let said = w.inbox.pass().unwrap();
        assert_eq!(said.len(), 2, "{said:?}");
        let tasks: Vec<Ticket> = w
            .store()
            .scan()
            .into_iter()
            .filter_map(|e| match e {
                ensemble_tasks::tasks::Entry::Task(t, _) => Some(*t),
                _ => None,
            })
            .collect();
        assert_eq!(tasks.len(), 2);
        let coder = tasks
            .iter()
            .find(|t| t.agent == "coder")
            .expect("one for coder");
        assert_eq!(coder.title, "Fix the login redirect");
        assert_eq!(coder.text, "It redirects to /home.");
        assert!(coder.context.as_deref().unwrap().contains("Issue 1"));
        assert!(
            tasks.iter().any(|t| t.agent.is_empty()),
            "the other is in the backlog"
        );
        assert_eq!(w.issue_now("1").status, Status::InProgress);
        assert!(w.last_comment("2").contains("no agent called painter"));
        assert_eq!(w.issue_now("3").status, Status::Open, "no label, not ours");
        assert!(w.inbox.pass().unwrap().is_empty(), "nothing twice");
    }

    #[test]
    fn an_issue_without_a_known_project_is_blocked_once_with_why() {
        let w = World::new();
        w.issue("1", "ensemble", "garden");
        w.inbox.pass().unwrap();
        assert_eq!(w.issue_now("1").status, Status::Blocked);
        assert!(
            w.last_comment("1")
                .contains("'garden' is not in the hub's `tasks.projects`")
        );
        assert!(w.inbox.pass().unwrap().is_empty(), "said once");
        assert!(w.store().scan().is_empty());
    }

    #[test]
    fn a_question_is_asked_on_the_issue_and_the_persons_comment_answers_it() {
        let w = World::new();
        w.issue("1", "ensemble, \"ensemble:coder\"", "shop");
        w.inbox.pass().unwrap();
        let task = w.inbox.seen()["1"].task.clone().unwrap();
        w.store()
            .ask(
                &task,
                "Which page should it return to?",
                "NEEDS INPUT: which page?",
            )
            .unwrap();
        w.inbox.pass().unwrap();
        assert_eq!(w.issue_now("1").status, Status::Blocked);
        assert!(
            w.last_comment("1")
                .contains("Which page should it return to?")
        );
        assert!(w.inbox.pass().unwrap().is_empty(), "asked once");

        w.inbox
            .tracker()
            .comment("1", "bob", "Not my call.")
            .unwrap();
        assert!(
            w.inbox.pass().unwrap().is_empty(),
            "only the assignee answers"
        );
        w.inbox
            .tracker()
            .comment("1", "ana", "The one they came from.")
            .unwrap();
        assert_eq!(w.inbox.pass().unwrap(), vec!["issue 1: answered"]);
        assert_eq!(w.issue_now("1").status, Status::InProgress);
        assert_eq!(w.store().open_question(&task), None);
    }

    #[test]
    fn a_result_is_posted_once_and_a_done_task_goes_to_review() {
        let w = World::new();
        w.issue("1", "ensemble, \"ensemble:coder\"", "shop");
        w.issue("2", "ensemble, \"ensemble:coder\"", "shop");
        w.inbox.pass().unwrap();
        let seen = w.inbox.seen();
        let (one, two) = (
            seen["1"].task.clone().unwrap(),
            seen["2"].task.clone().unwrap(),
        );
        let started = SystemTime::now();
        w.store()
            .write_result(
                &one,
                "coder",
                TaskStatus::Done,
                started,
                &[],
                "Fixed in auth.rs.",
            )
            .unwrap();
        w.store()
            .write_result(
                &two,
                "coder",
                TaskStatus::Failed,
                started,
                &[],
                "Tests fail.",
            )
            .unwrap();
        w.inbox.pass().unwrap();
        assert_eq!(w.issue_now("1").status, Status::Review);
        assert!(
            w.last_comment("1")
                .contains("is done:\n\nFixed in auth.rs.")
        );
        assert!(!w.last_comment("1").contains("status:"), "no front matter");
        assert_eq!(w.issue_now("2").status, Status::Blocked);
        assert!(w.last_comment("2").contains("failed:\n\nTests fail."));
        assert!(w.inbox.pass().unwrap().is_empty(), "posted once");
    }

    #[test]
    fn a_closed_issue_is_forgotten_and_one_inbox_runs_at_a_time() {
        let w = World::new();
        w.issue("1", "ensemble", "shop");
        w.inbox.pass().unwrap();
        w.inbox.tracker().set_status("1", Status::Done).unwrap();
        w.inbox.pass().unwrap();
        assert!(w.inbox.seen().is_empty());

        let held = w.inbox.lock().unwrap();
        assert!(
            w.inbox
                .lock()
                .unwrap_err()
                .contains("runs this hub's inbox already")
        );
        let stop = AtomicBool::new(true);
        w.inbox
            .work(&stop, &|_| panic!("a second inbox does nothing"));
        drop(held);
    }
}
