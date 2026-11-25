//! The runners connected to a hub server.
//!
//! A runner is a connection that said `runner`: it has an owner (the account
//! it signed in as), a name, and the harnesses it runs tasks on, each with the
//! models it can drive; a runner with none runs no task. The hub keeps no
//! runner when its connection is gone; runners are presence, not state. Who is
//! connected says which runners may take an account's tasks, and whether a
//! task they gave back is blocked (`board`).
//!
//! Every connection has an outbox here: events for it are put there by
//! whichever connection has them, and the connection's own task sends them.

use crate::board::Present;
use ensemble_link::protocol::Event;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc::{
    UnboundedReceiver as Receiver, UnboundedSender as Sender, unbounded_channel as channel,
};

/// One connected runner.
#[derive(Debug, Clone)]
struct Runner {
    owner: String,
    name: String,
    /// Its harnesses that may run, each `{harness, models: [{model, tier}]}`.
    harnesses: Vec<Value>,
}

impl Runner {
    fn id(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

#[derive(Default)]
struct Inner {
    outboxes: HashMap<u64, Sender<Event>>,
    runners: HashMap<u64, Runner>,
}

/// The connections of a hub server, and the runners among them.
#[derive(Default)]
pub(crate) struct Routes {
    inner: Mutex<Inner>,
    next: AtomicU64,
}

impl Routes {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A new connection: its number, and the events for it.
    pub(crate) fn connect(&self) -> (u64, Receiver<Event>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = channel();
        self.lock().outboxes.insert(id, tx);
        (id, rx)
    }

    /// A connection ended: its runner is gone.
    pub(crate) fn disconnect(&self, conn: u64) {
        let mut inner = self.lock();
        inner.outboxes.remove(&conn);
        inner.runners.remove(&conn);
    }

    /// Make `conn` a runner of `owner`, with what it says it runs on. Its name
    /// must not be taken by another connection of the same owner.
    pub(crate) fn register(&self, conn: u64, owner: &str, params: &Value) -> Result<Value, String> {
        let name = params["name"].as_str().unwrap_or("").trim().to_string();
        if !ensemble_config::runner::valid_name(&name) {
            return Err(format!(
                "runner: '{name}' is not a runner name: letters, digits, '-', '_' or '.'"
            ));
        }
        let harnesses: Vec<Value> = params["harnesses"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|h| {
                        let models: Vec<Value> = h["models"]
                            .as_array()?
                            .iter()
                            .filter_map(|m| {
                                Some(json!({
                                    "model": m["model"].as_str()?,
                                    "tier": m["tier"].as_str()?,
                                }))
                            })
                            .collect();
                        Some(json!({ "harness": h["harness"].as_str()?, "models": models }))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let runner = Runner {
            owner: owner.to_string(),
            name,
            harnesses,
        };
        let mut inner = self.lock();
        let id = runner.id();
        if inner
            .runners
            .iter()
            .any(|(c, r)| *c != conn && r.id() == id)
        {
            return Err(format!(
                "runner: {id} is connected already, from another process; give this one another name"
            ));
        }
        inner.runners.insert(conn, runner);
        Ok(json!({ "runner": id }))
    }

    /// `user`'s runners, each with what it runs tasks on.
    pub(crate) fn listed(&self, user: &str) -> Value {
        let inner = self.lock();
        let mut runners: Vec<Value> = inner
            .runners
            .values()
            .filter(|r| r.owner == user)
            .map(|r| {
                json!({
                    "runner": r.id(),
                    "harnesses": r.harnesses,
                })
            })
            .collect();
        runners.sort_by(|a, b| a["runner"].as_str().cmp(&b["runner"].as_str()));
        json!({ "runners": runners })
    }

    /// The runners connected now that may take `account`'s tasks: its own
    /// that run tasks, by their names.
    pub(crate) fn present(&self, account: &str) -> Vec<Present> {
        let mut present: Vec<Present> = self
            .lock()
            .runners
            .values()
            .filter(|r| r.owner == account && !r.harnesses.is_empty())
            .map(|r| Present {
                name: r.name.clone(),
            })
            .collect();
        present.sort_by(|a, b| a.name.cmp(&b.name));
        present
    }
}

#[cfg(test)]
mod tests {
    use crate::server::testing::start;
    use ensemble_link::client::Connection;
    use ensemble_link::protocol::*;
    use serde_json::{Value, json};
    use std::time::Duration;

    /// What a runner on Claude Code and OpenCode says it runs on.
    fn harnesses() -> Value {
        json!([
            {"harness": "claude-code", "models": [{"model": "sonnet", "tier": "strong"}]},
            {"harness": "opencode", "models": [{"model": "local:qwen3.5:9b", "tier": "medium"}]},
        ])
    }

    /// A runner on Claude Code and OpenCode.
    async fn runner(url: &str, token: &str, name: &str) -> Connection {
        let mut link = Connection::open(url, token).await.unwrap();
        link.call(RUNNER, json!({"name": name, "harnesses": harnesses()}))
            .await
            .unwrap();
        link
    }

    async fn open(url: &str, token: &str) -> Connection {
        Connection::open(url, token).await.unwrap()
    }

    /// What a call is refused with.
    async fn refused(link: &mut Connection, method: &str, params: Value) -> String {
        link.call(method, params).await.unwrap_err().message()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_runner_is_listed_to_its_owner_alone_with_what_it_runs_on() {
        let hub = start();
        let _gpu = runner(&hub.url, &hub.root, "gpu-box").await;
        let mut ana = open(&hub.url, &hub.ana).await;
        let said = refused(&mut ana, RUNNER, json!({"name": "a b"})).await;
        assert!(said.contains("not a runner name"), "{said}");
        ana.call(RUNNER, json!({"name": "laptop", "harnesses": []}))
            .await
            .unwrap();
        let listed = ana.call(RUNNERS, json!({})).await.unwrap();
        assert_eq!(
            listed["runners"].as_array().unwrap().len(),
            1,
            "root's works for root alone"
        );
        assert_eq!(listed["runners"][0]["runner"], "ana/laptop");
        assert_eq!(listed["runners"][0]["harnesses"], json!([]));
        assert!(
            hub.server.routes.present("ana").is_empty(),
            "a runner with no harness runs no task"
        );
        let mut root = open(&hub.url, &hub.root).await;
        let listed = root.call(RUNNERS, json!({})).await.unwrap();
        assert_eq!(listed["runners"][0]["runner"], "root/gpu-box");
        assert_eq!(listed["runners"][0]["harnesses"], harnesses());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_runners_name_is_taken_once_and_it_is_present_while_connected() {
        let hub = start();
        let laptop = runner(&hub.url, &hub.ana, "laptop").await;
        let mut again = open(&hub.url, &hub.ana).await;
        let said = refused(&mut again, RUNNER, json!({"name": "laptop"})).await;
        assert!(said.contains("connected already"), "{said}");
        let present = hub.server.routes.present("ana");
        assert_eq!(present.len(), 1);
        assert_eq!(present[0].name, "laptop");
        assert!(hub.server.routes.present("root").is_empty());
        // Another account's runner never takes ana's tasks.
        let _gpu = runner(&hub.url, &hub.root, "gpu-box").await;
        assert_eq!(hub.server.routes.present("ana").len(), 1);
        let mut laptop = laptop;
        laptop.close().await;
        drop(laptop);
        let gone = async {
            while !hub.server.routes.present("ana").is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), gone)
            .await
            .expect("a runner that went away is not present");
    }
}
