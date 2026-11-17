//! The task inbox on a hub server. It watches the tracker as the embedded inbox
//! does, but the work happens on members' computers:
//!
//! 1. An open issue with the label, assigned to someone who is an account's
//!    tracker name and whose role may receive `tasks`, becomes a **delivery**
//!    for that account, and the issue says it waits for their computer. With
//!    no such account the issue is blocked, saying why, once.
//! 2. The delivery is pushed over the link to every computer of that account
//!    that subscribed. The first to make it a task accepts it; the issue goes
//!    to `in-progress`. A computer with no folder for the project refuses, and
//!    the issue says so once; the delivery waits for another.
//! 3. What the task says comes back: a question (the issue goes to `blocked`,
//!    and the assignee's next comment goes back as an answer), then its result
//!    (`review` or `blocked`). Ensemble never closes an issue.
//! 4. An issue closed, or without the label, drops its delivery.
//!
//! The deliveries are kept in the hub's database (`ensemble_link::store`), in
//! its state directory.

use crate::tracker::{Issue, IssueTracker, Status};
use crate::work::{self, AUTHOR, Update, reply_after};
use ensemble_config::{Permission, Tasks};
use ensemble_link::Users;
use ensemble_link::protocol::{
    ANSWER_EVENT, ANSWER_TAKEN, Event, TASK_ACCEPT, TASK_EVENT, TASK_REFUSE, TASK_UPDATE, TaskRelay,
};
use ensemble_link::store;
use ensemble_tasks::tasks::Status as TaskStatus;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Where deliveries were kept before the database; taken into it once.
const DELIVERIES_FILE: &str = "deliveries.json";

/// Where a delivery stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    /// No account takes it; the issue says why.
    Unassigned,
    /// Waiting for one of the account's computers.
    Queued,
    /// A task on one of them.
    Accepted,
    /// The task asked a question, posted on the issue.
    Asking,
    /// The task ended, and its result is on the issue.
    Done,
}

/// One issue's way to a member's computer and back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    pub issue: String,
    /// The account it is for; empty while unassigned.
    pub account: String,
    pub stage: Stage,
    /// The task it became, once accepted.
    pub task: Option<String>,
    /// How many comments the issue had after the hub's last one.
    pub comments: usize,
    /// The assignee's answer, until the computer says it passed it on.
    pub answer: Option<String>,
    /// Whether a refusal was already said on the issue.
    pub refused: bool,
}

/// The deliveries kept in `dir`, by issue. Those from before the database,
/// in `deliveries.json`, are taken into it once.
fn load(dir: &std::path::Path) -> Result<BTreeMap<String, Delivery>, String> {
    store::with(dir, async |c| {
        let mut out = BTreeMap::new();
        for row in store::rows(c, "SELECT issue, data FROM deliveries", vec![]).await? {
            let (Some(issue), Some(data)) = (store::text(&row[0]), store::text(&row[1])) else {
                continue;
            };
            let delivery = serde_json::from_str(&data)
                .map_err(|e| format!("the delivery of issue {issue} is unreadable: {e}"))?;
            out.insert(issue, delivery);
        }
        let old = dir.join(DELIVERIES_FILE);
        if out.is_empty()
            && let Ok(text) = std::fs::read_to_string(&old)
        {
            let before: BTreeMap<String, Delivery> = serde_json::from_str(&text)
                .map_err(|e| format!("{} is not readable: {e}", old.display()))?;
            let mut statements = Vec::new();
            for (issue, delivery) in &before {
                let data = serde_json::to_string(delivery).map_err(|e| e.to_string())?;
                statements.push((
                    "INSERT INTO deliveries (issue, data) VALUES (?1, ?2)",
                    vec![store::Value::Text(issue.clone()), store::Value::Text(data)],
                ));
            }
            store::transaction(c, statements).await?;
            std::fs::rename(&old, old.with_extension("json.imported"))
                .map_err(|e| format!("cannot move {} aside: {e}", old.display()))?;
            out = before;
        }
        Ok(out)
    })
}

/// A hub server's task inbox.
pub struct ServerInbox {
    tasks: Tasks,
    tracker: Box<dyn IssueTracker + Send>,
    roles: BTreeMap<String, Vec<Permission>>,
    /// The hub's state directory, with its database: its users, and the
    /// deliveries.
    dir: PathBuf,
    /// Held for every pass and every request, so the tracker and the
    /// deliveries change one step at a time.
    deliveries: Mutex<BTreeMap<String, Delivery>>,
}

impl ServerInbox {
    pub fn new(
        tasks: &Tasks,
        roles: &BTreeMap<String, Vec<Permission>>,
        dir: PathBuf,
    ) -> Result<Self, String> {
        let deliveries = load(&dir)?;
        Ok(ServerInbox {
            tracker: crate::tracker_for(tasks),
            tasks: tasks.clone(),
            roles: roles.clone(),
            dir,
            deliveries: Mutex::new(deliveries),
        })
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
        let mut statements = vec![("DELETE FROM deliveries", Vec::new())];
        for (issue, delivery) in deliveries {
            let data = serde_json::to_string(delivery).map_err(|e| e.to_string())?;
            statements.push((
                "INSERT INTO deliveries (issue, data) VALUES (?1, ?2)",
                vec![store::Value::Text(issue.clone()), store::Value::Text(data)],
            ));
        }
        store::with(&self.dir, async |c| store::transaction(c, statements).await)
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

    /// One look at the tracker. Returns a line for each thing it did.
    pub fn pass(&self) -> Result<Vec<String>, String> {
        let users = ensemble_link::Users::load(&self.dir).map_err(String::from)?;
        let mut deliveries = self.deliveries.lock().unwrap_or_else(|e| e.into_inner());
        let mut said = Vec::new();
        let issues = self.tracker.labelled(&self.tasks.label);
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
                let delivery = match self.account_for(&users, issue) {
                    Some(account) => {
                        let mut d = new_delivery(&issue.id, &account, Stage::Queued);
                        self.say(
                            issue,
                            &format!("Waiting for {account}'s computer: the task starts when it is connected."),
                            Status::Open,
                            &mut d,
                        )?;
                        said.push(format!("issue {} -> {account}", issue.id));
                        d
                    }
                    None => {
                        let mut d = new_delivery(&issue.id, "", Stage::Unassigned);
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
                        d
                    }
                };
                deliveries.insert(issue.id.clone(), delivery);
                continue;
            }
            if let Some(d) = deliveries.get_mut(&issue.id)
                && d.stage == Stage::Asking
                && d.answer.is_none()
            {
                let comments = self.tracker.comments(&issue.id)?;
                if let Some(answer) = reply_after(&comments, d.comments, &issue.assignee) {
                    d.answer = Some(answer);
                    d.comments = comments.len();
                    said.push(format!("issue {}: answered", issue.id));
                }
            }
        }
        deliveries.retain(|id, _| {
            issues
                .iter()
                .any(|i| &i.id == id && i.status != Status::Done)
        });
        self.save(&deliveries)?;
        Ok(said)
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

    fn request(&self, user: &str, method: &str, params: &Value) -> Result<Value, String> {
        let id = params
            .get("delivery")
            .and_then(Value::as_str)
            .ok_or("`delivery` is required")?;
        let mut deliveries = self.deliveries.lock().unwrap_or_else(|e| e.into_inner());
        let d = deliveries
            .get_mut(id)
            .filter(|d| d.account == user)
            .ok_or_else(|| format!("no delivery {id} for you"))?;
        let issue = self.tracker.issue(&d.issue)?;
        let out = match method {
            TASK_ACCEPT => {
                let task = params
                    .get("task")
                    .and_then(Value::as_str)
                    .ok_or("`task` is required")?;
                match (d.stage, d.task.as_deref()) {
                    (Stage::Queued, _) => {
                        d.stage = Stage::Accepted;
                        d.task = Some(task.to_string());
                        self.say(
                            &issue,
                            &format!("Picked up as task {task} on {user}'s computer."),
                            Status::InProgress,
                            d,
                        )?;
                        json!({ "accepted": true })
                    }
                    (_, Some(t)) if t == task => json!({ "accepted": true }),
                    _ => return Err("taken already by another computer".into()),
                }
            }
            TASK_REFUSE => {
                let why = params.get("why").and_then(Value::as_str).unwrap_or("");
                if !d.refused && d.stage == Stage::Queued {
                    d.refused = true;
                    self.say(
                        &issue,
                        &format!("One of {user}'s computers cannot take this: {why}"),
                        issue.status,
                        d,
                    )?;
                }
                json!({ "refused": true })
            }
            TASK_UPDATE => {
                let task = d.task.clone().ok_or("this delivery was not accepted")?;
                let update = if let Some(q) = params.get("question").and_then(Value::as_str) {
                    Update::Question(q.to_string())
                } else {
                    let result = params
                        .get("result")
                        .ok_or("`question` or `result` is required")?;
                    let status = result
                        .get("status")
                        .and_then(Value::as_str)
                        .and_then(TaskStatus::parse)
                        .ok_or("`result.status` must be done, failed or cancelled")?;
                    let reply = result.get("text").and_then(Value::as_str).unwrap_or("");
                    Update::Result {
                        status,
                        reply: reply.trim().to_string(),
                    }
                };
                if d.stage == Stage::Done {
                    return Ok(json!({ "noted": true }));
                }
                let (comment, status) = work::on_issue(&task, &update);
                self.say(&issue, &comment, status, d)?;
                d.stage = match update {
                    Update::Question(_) => Stage::Asking,
                    Update::Result { .. } => Stage::Done,
                };
                d.answer = None;
                json!({ "noted": true })
            }
            ANSWER_TAKEN => {
                if d.stage == Stage::Asking && d.answer.is_some() {
                    d.answer = None;
                    d.stage = Stage::Accepted;
                    if issue.status != Status::InProgress {
                        self.tracker.set_status(&issue.id, Status::InProgress)?;
                    }
                }
                json!({ "taken": true })
            }
            other => return Err(format!("no method `{other}`")),
        };
        self.save(&deliveries)?;
        Ok(out)
    }

    /// Look at the tracker every `poll_seconds` until `stop` is set.
    pub fn work(&self, stop: &AtomicBool, say: &(dyn Fn(&str) + Sync)) {
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

fn new_delivery(issue: &str, account: &str, stage: Stage) -> Delivery {
    Delivery {
        issue: issue.to_string(),
        account: account.to_string(),
        stage,
        task: None,
        comments: 0,
        answer: None,
        refused: false,
    }
}

impl TaskRelay for ServerInbox {
    fn pending(&self, user: &str) -> Vec<(String, Event)> {
        let deliveries = self.deliveries.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        for d in deliveries.values().filter(|d| d.account == user) {
            match (d.stage, &d.answer) {
                (Stage::Queued, _) => {
                    let Ok(issue) = self.tracker.issue(&d.issue) else {
                        continue;
                    };
                    out.push((
                        format!("task:{}", d.issue),
                        Event {
                            event: TASK_EVENT.into(),
                            data: json!({
                                "delivery": d.issue,
                                "issue": issue.id,
                                "title": issue.title,
                                "body": issue.body,
                                "project": issue.project,
                                "labels": issue.labels,
                                "label": self.tasks.label,
                                "assignee": issue.assignee,
                            }),
                        },
                    ));
                }
                (Stage::Asking, Some(answer)) => out.push((
                    format!("answer:{}:{}", d.issue, d.comments),
                    Event {
                        event: ANSWER_EVENT.into(),
                        data: json!({ "delivery": d.issue, "text": answer }),
                    },
                )),
                _ => {}
            }
        }
        out
    }

    fn handle(&self, user: &str, method: &str, params: &Value) -> Result<Value, String> {
        self.request(user, method, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_config::IssueTracker as Kind;
    use ensemble_core::workspace::testing::TempDir;

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
        let mut users = Users::load(&tmp.0.join("state")).unwrap();
        users.add("ana", "member", &roles).unwrap();
        users.set_tracker_user("ana", Some("ana.p")).unwrap();
        users.add("rex", "reader", &roles).unwrap();
        users.save().unwrap();
        let tasks = Tasks {
            tracker: Kind::LocalMarkdown {
                dir: tmp.0.join("issues"),
            },
            label: "ensemble".into(),
            poll_seconds: 1,
        };
        let inbox = ServerInbox::new(&tasks, &roles, tmp.0.join("state")).unwrap();
        World { tmp, inbox }
    }

    #[test]
    fn deliveries_are_kept_in_the_database_and_those_from_before_are_taken_in_once() {
        let w = world();
        let state = w.tmp.0.join("state");
        let mut kept = BTreeMap::new();
        kept.insert(
            "7".to_string(),
            Delivery {
                issue: "7".into(),
                account: "ana".into(),
                stage: Stage::Queued,
                task: None,
                comments: 0,
                answer: None,
                refused: false,
            },
        );
        w.inbox.save(&kept).unwrap();
        let roles = w.inbox.roles.clone();
        let again = ServerInbox::new(&w.inbox.tasks, &roles, state.clone()).unwrap();
        assert_eq!(again.deliveries(), kept);

        // A hub from before the database: its file is read once, then set aside.
        let old = w.tmp.0.join("old");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(
            old.join(DELIVERIES_FILE),
            serde_json::to_string(&kept).unwrap(),
        )
        .unwrap();
        let taken = ServerInbox::new(&w.inbox.tasks, &roles, old.clone()).unwrap();
        assert_eq!(taken.deliveries(), kept);
        assert!(!old.join(DELIVERIES_FILE).exists());
        assert!(old.join("deliveries.json.imported").exists());
    }

    impl World {
        fn issue(&self, id: &str, assignee: &str) {
            std::fs::write(
                self.tmp.0.join("issues").join(format!("{id}.md")),
                format!("---\ntitle: Fix the login redirect\nassignee: {assignee}\nlabels: [ensemble, \"ensemble:coder\"]\nproject: shop\nstatus: open\n---\nIt redirects to /home.\n"),
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

        fn ask(&self, user: &str, method: &str, params: Value) -> Result<Value, String> {
            self.inbox.handle(user, method, &params)
        }
    }

    #[test]
    fn an_issue_waits_for_its_assignees_computer_and_goes_there() {
        let w = world();
        w.issue("1", "ana.p");
        w.inbox.pass().unwrap();
        assert!(w.last_comment("1").contains("Waiting for ana's computer"));
        assert_eq!(w.status("1"), Status::Open);
        let pending = w.inbox.pending("ana");
        assert_eq!(pending.len(), 1);
        let (_, event) = &pending[0];
        assert_eq!(event.event, TASK_EVENT);
        assert_eq!(event.data["project"], "shop");
        assert_eq!(event.data["label"], "ensemble");
        assert!(w.inbox.pending("rex").is_empty());
        assert!(w.inbox.pass().unwrap().is_empty(), "said once");

        w.ask("ana", TASK_ACCEPT, json!({"delivery": "1", "task": "3"}))
            .unwrap();
        assert_eq!(w.status("1"), Status::InProgress);
        assert!(w.last_comment("1").contains("task 3 on ana's computer"));
        assert!(w.inbox.pending("ana").is_empty(), "taken");
        assert!(
            w.ask("ana", TASK_ACCEPT, json!({"delivery": "1", "task": "3"}))
                .is_ok(),
            "the same computer again"
        );
        let other = w
            .ask("ana", TASK_ACCEPT, json!({"delivery": "1", "task": "9"}))
            .unwrap_err();
        assert!(other.contains("taken already"), "{other}");
        assert!(
            w.ask("rex", TASK_ACCEPT, json!({"delivery": "1", "task": "3"}))
                .is_err(),
            "not rex's"
        );
    }

    #[test]
    fn a_question_goes_on_the_issue_and_the_assignees_comment_comes_back() {
        let w = world();
        w.issue("1", "ana.p");
        w.inbox.pass().unwrap();
        w.ask("ana", TASK_ACCEPT, json!({"delivery": "1", "task": "3"}))
            .unwrap();
        w.ask(
            "ana",
            TASK_UPDATE,
            json!({"delivery": "1", "question": "Which page?"}),
        )
        .unwrap();
        assert_eq!(w.status("1"), Status::Blocked);
        assert!(w.last_comment("1").contains("Which page?"));
        w.inbox
            .tracker()
            .comment("1", "bob", "not mine to say")
            .unwrap();
        w.inbox.pass().unwrap();
        assert!(
            w.inbox.pending("ana").is_empty(),
            "only the assignee answers"
        );
        w.inbox
            .tracker()
            .comment("1", "ana.p", "The one they came from.")
            .unwrap();
        w.inbox.pass().unwrap();
        let pending = w.inbox.pending("ana");
        assert_eq!(pending[0].1.event, ANSWER_EVENT);
        assert_eq!(pending[0].1.data["text"], "The one they came from.");
        w.ask("ana", ANSWER_TAKEN, json!({"delivery": "1"}))
            .unwrap();
        assert_eq!(w.status("1"), Status::InProgress);
        assert!(w.inbox.pending("ana").is_empty());

        w.ask(
            "ana",
            TASK_UPDATE,
            json!({"delivery": "1", "result": {"status": "done", "text": "Fixed in auth.rs."}}),
        )
        .unwrap();
        assert_eq!(w.status("1"), Status::Review);
        assert!(
            w.last_comment("1")
                .contains("Task 3 is done:\n\nFixed in auth.rs.")
        );
    }

    #[test]
    fn nobody_or_a_computer_without_the_folder_is_said_once() {
        let w = world();
        w.issue("1", "someone.else");
        w.issue("2", "rex");
        w.inbox.pass().unwrap();
        assert_eq!(w.status("1"), Status::Blocked);
        assert!(
            w.last_comment("1")
                .contains("no account on the hub is someone.else")
        );
        assert_eq!(w.status("2"), Status::Blocked, "rex may not receive tasks");
        assert!(w.inbox.pass().unwrap().is_empty(), "said once");

        w.issue("3", "ana.p");
        w.inbox.pass().unwrap();
        for _ in 0..2 {
            w.ask(
                "ana",
                TASK_REFUSE,
                json!({"delivery": "3", "why": "no folder for project shop"}),
            )
            .unwrap();
        }
        let refusals = w
            .inbox
            .tracker()
            .comments("3")
            .unwrap()
            .iter()
            .filter(|c| c.text.contains("cannot take this"))
            .count();
        assert_eq!(refusals, 1);
        assert_eq!(w.inbox.pending("ana").len(), 1, "it still waits");

        w.inbox.tracker().set_status("3", Status::Done).unwrap();
        w.inbox.pass().unwrap();
        assert!(!w.inbox.deliveries().contains_key("3"), "closed, forgotten");
    }
}
