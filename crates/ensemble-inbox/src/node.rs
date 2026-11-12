//! A member's side of a hub server's task inbox. While `ensemble mcp` or
//! `ensemble serve` runs, it keeps its own connection to the hub, subscribed to
//! the account's tasks:
//!
//! - a `task` event becomes a task in the folder `node.yaml` maps its project
//!   to, for the agent its `<label>:<agent>` label names or in the backlog,
//!   and the hub is told; with no folder for the project, the hub is told
//!   that, and another of the person's computers may take it;
//! - every few seconds, each task's new question or result goes back;
//! - an `answer` event is passed to its task.
//!
//! What it did is kept in `node.json` in its state directory, with the lock
//! that keeps one per hub on this computer.

use crate::tracker::{Issue, Status};
use crate::work::{self, Update};
use ensemble_config::Node;
use ensemble_link::client::{Connection, Failure};
use ensemble_link::protocol::{
    ANSWER_EVENT, ANSWER_TAKEN, Event, SUBSCRIBE, TASK_ACCEPT, TASK_EVENT, TASK_REFUSE, TASK_UPDATE,
};
use ensemble_tasks::tasks::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const STATE_FILE: &str = "node.json";
/// How often the tasks are looked at for something to say.
const FOLLOW: Duration = Duration::from_secs(2);

type StateFor = dyn Fn(&Path) -> Result<PathBuf, String> + Send + Sync;

/// One delivery made into a task on this computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Local {
    pub folder: PathBuf,
    pub task: String,
    /// The question last sent, while one waits.
    pub asked: Option<String>,
    /// Whether the result was sent.
    pub reported: bool,
}

/// This computer's part in a hub server's task inbox.
pub struct NodeInbox {
    url: String,
    token: String,
    node: Node,
    /// The helper team's agents, which `<label>:<agent>` may name.
    agents: Vec<String>,
    /// Where `node.json` and the lock live.
    dir: PathBuf,
    state_for: Box<StateFor>,
}

impl NodeInbox {
    pub fn new(url: &str, token: &str, node: Node, agents: Vec<String>, dir: PathBuf) -> Self {
        NodeInbox {
            url: url.to_string(),
            token: token.to_string(),
            node,
            agents,
            dir,
            state_for: Box::new(ensemble_core::state::dir_for),
        }
    }

    /// Keep each folder's tasks where `state_for` says.
    pub fn with_state_for(
        mut self,
        state_for: impl Fn(&Path) -> Result<PathBuf, String> + Send + Sync + 'static,
    ) -> Self {
        self.state_for = Box::new(state_for);
        self
    }

    /// What this computer has taken, by delivery.
    pub fn taken(&self) -> BTreeMap<String, Local> {
        std::fs::read_to_string(self.dir.join(STATE_FILE))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self, taken: &BTreeMap<String, Local>) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| format!("cannot create {}: {e}", self.dir.display()))?;
        let path = self.dir.join(STATE_FILE);
        let tmp = path.with_extension("tmp");
        let json = serde_json::to_string_pretty(taken).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, json)
            .and_then(|()| std::fs::rename(&tmp, &path))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    fn store(&self, folder: &Path) -> Result<Store, String> {
        Ok(Store::new(&(self.state_for)(folder)?))
    }

    /// Take the hub's tasks until `stop` is set, connecting again when the
    /// link breaks. Another process that does this for the hub already, or a
    /// token that may not receive tasks, makes it return.
    pub fn work(&self, stop: &AtomicBool, say: &(dyn Fn(&str) + Sync)) {
        let Ok(_lock) = ensemble_core::lock::acquire(&self.dir) else {
            return;
        };
        let mut backoff = Duration::from_secs(1);
        let mut said_down = false;
        while !stop.load(Ordering::Relaxed) {
            match self.session(stop, say) {
                Ok(()) => return,
                Err(Failure::Hub(e)) => {
                    say(&format!(
                        "tasks: the hub {} sends no tasks here: {e}",
                        self.url
                    ));
                    return;
                }
                Err(Failure::Broken(e)) => {
                    if !said_down {
                        say(&format!(
                            "tasks: the link to {} is down ({e}); trying again",
                            self.url
                        ));
                        said_down = true;
                    }
                }
            }
            let until = Instant::now() + backoff;
            while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }

    /// One connection's life: subscribe, take events, report, until `stop`.
    fn session(&self, stop: &AtomicBool, say: &(dyn Fn(&str) + Sync)) -> Result<(), Failure> {
        let mut link = Connection::open(&self.url, &self.token).map_err(Failure::Broken)?;
        link.call(SUBSCRIBE, json!({}))?;
        let mut followed = Instant::now() - FOLLOW;
        while !stop.load(Ordering::Relaxed) {
            if let Some(event) = link
                .next_event(Duration::from_millis(200))
                .map_err(Failure::Broken)?
            {
                for line in self.on_event(&mut link, &event)? {
                    say(&format!("tasks: {line}"));
                }
            }
            if followed.elapsed() >= FOLLOW {
                followed = Instant::now();
                for line in self.report(&mut link)? {
                    say(&format!("tasks: {line}"));
                }
            }
        }
        link.close();
        Ok(())
    }

    /// Act on one event from the hub.
    fn on_event(&self, link: &mut Connection, event: &Event) -> Result<Vec<String>, Failure> {
        let data = &event.data;
        let text = |k: &str| data[k].as_str().unwrap_or("").to_string();
        let delivery = text("delivery");
        let mut taken = self.taken();
        match event.event.as_str() {
            TASK_EVENT => {
                if let Some(local) = taken.get(&delivery) {
                    // Pushed again after a reconnect: say again what it became.
                    link.call(
                        TASK_ACCEPT,
                        json!({ "delivery": delivery, "task": local.task }),
                    )?;
                    return Ok(Vec::new());
                }
                let project = text("project");
                let folder = match self.node.projects.get(&project) {
                    Some(f) if f.is_dir() => f.clone(),
                    Some(f) => {
                        let why = format!(
                            "its folder for project {project}, {}, is not a folder",
                            f.display()
                        );
                        link.call(TASK_REFUSE, json!({ "delivery": delivery, "why": why }))?;
                        return Ok(vec![format!("refused {delivery}: {why}")]);
                    }
                    None => {
                        let why =
                            format!("it has no folder for project '{project}' in its node.yaml");
                        link.call(TASK_REFUSE, json!({ "delivery": delivery, "why": why }))?;
                        return Ok(vec![format!("refused {delivery}: {why}")]);
                    }
                };
                let issue = Issue {
                    id: text("issue"),
                    title: text("title"),
                    assignee: text("assignee"),
                    labels: data["labels"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(String::from)
                                .collect()
                        })
                        .unwrap_or_default(),
                    project: Some(project),
                    status: Status::Open,
                    body: text("body"),
                };
                let store = self.store(&folder).map_err(Failure::Hub)?;
                let (task, _) = work::make_task(
                    &store,
                    &issue,
                    &text("label"),
                    &self.agents,
                    &issue.assignee,
                    "on this computer",
                )
                .map_err(Failure::Hub)?;
                match link.call(TASK_ACCEPT, json!({ "delivery": delivery, "task": task })) {
                    Ok(_) => {
                        taken.insert(
                            delivery.clone(),
                            Local {
                                folder,
                                task: task.clone(),
                                asked: None,
                                reported: false,
                            },
                        );
                        self.save(&taken).map_err(Failure::Hub)?;
                        Ok(vec![format!("issue {} -> task {task}", issue.id)])
                    }
                    Err(Failure::Hub(e)) => {
                        // Another computer was first: this task is not needed.
                        let _ = store.write_result(
                            &task,
                            "",
                            ensemble_tasks::tasks::Status::Cancelled,
                            std::time::SystemTime::now(),
                            &[],
                            &format!("Not needed here: {e}."),
                        );
                        Ok(vec![format!("issue {}: {e}", issue.id)])
                    }
                    Err(broken) => Err(broken),
                }
            }
            ANSWER_EVENT => {
                let Some(local) = taken.get(&delivery) else {
                    return Ok(Vec::new());
                };
                let store = self.store(&local.folder).map_err(Failure::Hub)?;
                // Passed on already when there is no question any more.
                if store.open_question(&local.task).is_some() {
                    work::answer(&store, &local.task, &text("text")).map_err(Failure::Hub)?;
                }
                link.call(ANSWER_TAKEN, json!({ "delivery": delivery }))?;
                Ok(vec![format!("task {}: answered", local.task)])
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Send each task's new question or result.
    fn report(&self, link: &mut Connection) -> Result<Vec<String>, Failure> {
        let mut taken = self.taken();
        let mut said = Vec::new();
        for (delivery, local) in taken.iter_mut().filter(|(_, l)| !l.reported) {
            let store = self.store(&local.folder).map_err(Failure::Hub)?;
            match work::follow(&store, &local.task, local.asked.as_deref()) {
                Some(Update::Question(q)) => {
                    link.call(TASK_UPDATE, json!({ "delivery": delivery, "question": q }))?;
                    said.push(format!("task {} asked a question", local.task));
                    local.asked = Some(q);
                }
                Some(Update::Result { status, reply }) => {
                    link.call(
                        TASK_UPDATE,
                        json!({
                            "delivery": delivery,
                            "result": { "status": status.as_str(), "text": reply },
                        }),
                    )?;
                    said.push(format!("task {} {}", local.task, status.as_str()));
                    local.reported = true;
                }
                None => {}
            }
        }
        if !said.is_empty() {
            taken.retain(|_, l| !l.reported);
            self.save(&taken).map_err(Failure::Hub)?;
        }
        Ok(said)
    }
}
