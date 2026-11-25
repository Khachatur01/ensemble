//! The runners connected to a hub server.
//!
//! A runner is a connection that said `runner`: it has an owner (the account
//! it signed in as), a name, and the harnesses it runs tasks on, each with the
//! models it can drive; a runner with none runs no task. The hub keeps no
//! runner when its connection is gone; runners are presence, not state. Who is
//! connected, with its workers and how busy each harness and provider is, is
//! what the hub's task picker chooses the worker of a task from
//! (`dispatch`), and says whether a task they gave back is blocked
//! (`board`).
//!
//! Every connection has an outbox here: events for it are put there by
//! whichever connection has them, and the connection's own task sends them.

use crate::board::Present;
use crate::dispatch::{RunnerNow, WorkerNow};
use ensemble_link::protocol::Event;
use ensemble_tasks::tasks::{ALONE, Worker};
use serde_json::{Value, json};
use std::collections::BTreeMap;
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
    /// Its harnesses that may run, each `{harness, max_concurrent, models:
    /// [{model, provider, tier, context_window, tools, abilities}]}`: what the
    /// hub may choose a worker of for a task.
    harnesses: Vec<Value>,
    /// The LLMs of its pool that may answer a task alone, each `{model,
    /// provider, tier, context_window, tools, abilities}`.
    alone: Vec<Value>,
    /// The most calls at once to each provider of its pool.
    providers: Value,
    /// How busy it is, as it last said: `{harnesses, providers}`, the runs on
    /// each.
    busy: Value,
}

impl Runner {
    fn id(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// A worker of it: `harness`, or [`ALONE`], and the model `m`, as it
    /// said it.
    fn worker(&self, harness: &str, m: &Value) -> Option<WorkerNow> {
        let (Some(model), Some(tier)) = (
            m["model"].as_str(),
            m["tier"].as_str().and_then(ensemble_config::Tier::parse),
        ) else {
            return None;
        };
        Some(WorkerNow {
            worker: Worker {
                runner: self.name.clone(),
                harness: harness.to_string(),
                model: model.to_string(),
            },
            provider: m["provider"].as_str().map(String::from),
            tier,
            context_window: m["context_window"].as_u64().map(|n| n as u32),
            tools: m["tools"].as_bool(),
            abilities: m["abilities"].as_str().map(String::from),
        })
    }

    /// It as the task picker sees it.
    fn now(&self) -> RunnerNow {
        let mut workers = Vec::new();
        let mut harness_limits = BTreeMap::new();
        for h in &self.harnesses {
            let Some(harness) = h["harness"].as_str() else {
                continue;
            };
            if let Some(n) = h["max_concurrent"].as_u64() {
                harness_limits.insert(harness.to_string(), n as u32);
            }
            for m in h["models"].as_array().into_iter().flatten() {
                workers.extend(self.worker(harness, m));
            }
        }
        for m in &self.alone {
            workers.extend(self.worker(ALONE, m));
        }
        RunnerNow {
            owner: self.owner.clone(),
            name: self.name.clone(),
            workers,
            harness_limits,
            provider_limits: numbers(&self.providers),
            busy_harnesses: numbers(&self.busy["harnesses"]),
            busy_providers: numbers(&self.busy["providers"]),
        }
    }
}

/// A map of names to counts, as kept.
fn numbers(kept: &Value) -> BTreeMap<String, u32> {
    kept.as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| Some((k.clone(), v.as_u64()? as u32)))
        .collect()
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
            .map(|a| a.iter().filter_map(harness).collect())
            .unwrap_or_default();
        let alone: Vec<Value> = params["alone"]
            .as_array()
            .map(|a| a.iter().filter_map(model).collect())
            .unwrap_or_default();
        let providers = counts(&params["providers"]);
        let runner = Runner {
            owner: owner.to_string(),
            name,
            harnesses,
            alone,
            providers,
            busy: json!({ "harnesses": {}, "providers": {} }),
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

    /// How busy the runner on `conn` is now, as it says.
    pub(crate) fn busy(&self, conn: u64, params: &Value) -> Result<Value, String> {
        let mut inner = self.lock();
        let runner = inner
            .runners
            .get_mut(&conn)
            .ok_or("runner_busy: this connection has not said it is a runner")?;
        runner.busy = json!({
            "harnesses": counts(&params["harnesses"]),
            "providers": counts(&params["providers"]),
        });
        Ok(json!({}))
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
                    "alone": r.alone,
                    "providers": r.providers,
                    "busy": r.busy,
                })
            })
            .collect();
        runners.sort_by(|a, b| a["runner"].as_str().cmp(&b["runner"].as_str()));
        json!({ "runners": runners })
    }

    /// The runners connected now that may take `account`'s tasks: its own
    /// that run tasks, by their names, each with its workers.
    pub(crate) fn present(&self, account: &str) -> Vec<Present> {
        self.now()
            .into_iter()
            .filter(|r| r.owner == account && !r.workers.is_empty())
            .map(|r| Present {
                workers: r.workers.into_iter().map(|w| w.worker).collect(),
                name: r.name,
            })
            .collect()
    }

    /// Every runner connected now, with its workers and how busy it is, by
    /// its owner and name.
    pub(crate) fn now(&self) -> Vec<RunnerNow> {
        let mut all: Vec<RunnerNow> = self.lock().runners.values().map(Runner::now).collect();
        all.sort_by(|a, b| (&a.owner, &a.name).cmp(&(&b.owner, &b.name)));
        all
    }
}

/// A harness as a runner said it, with only what the hub reads of it; none
/// when it is not one.
fn harness(h: &Value) -> Option<Value> {
    let models: Vec<Value> = h["models"].as_array()?.iter().filter_map(model).collect();
    Some(json!({
        "harness": h["harness"].as_str()?,
        "max_concurrent": h["max_concurrent"].as_u64(),
        "models": models,
    }))
}

/// A model as a runner said it, with only what the hub reads of it; none when
/// it is not one.
fn model(m: &Value) -> Option<Value> {
    Some(json!({
        "model": m["model"].as_str()?,
        "provider": m["provider"].as_str(),
        "tier": m["tier"].as_str()?,
        "context_window": m["context_window"].as_u64(),
        "tools": m["tools"].as_bool(),
        "abilities": m["abilities"].as_str(),
    }))
}

/// A map of names to counts as a runner said it, without what is not one.
fn counts(said: &Value) -> Value {
    let counted: serde_json::Map<String, Value> = said
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| Some((k.clone(), json!(v.as_u64()?))))
        .collect();
    Value::Object(counted)
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
            {"harness": "claude-code", "max_concurrent": 1, "models": [
                {"model": "sonnet", "provider": null, "tier": "strong", "context_window": null,
                 "tools": true, "abilities": "strong at Rust"},
            ]},
            {"harness": "opencode", "max_concurrent": 2, "models": [
                {"model": "local:qwen3.5:9b", "provider": "local", "tier": "medium",
                 "context_window": 32768, "tools": true, "abilities": null},
            ]},
        ])
    }

    /// What a runner whose pool has one LLM says may answer alone.
    fn alone() -> Value {
        json!([
            {"model": "local:qwen3.5:9b", "provider": "local", "tier": "medium",
             "context_window": 32768, "tools": true, "abilities": null},
        ])
    }

    /// A runner on Claude Code and OpenCode.
    async fn runner(url: &str, token: &str, name: &str) -> Connection {
        let mut link = Connection::open(url, token).await.unwrap();
        link.call(
            RUNNER,
            json!({"name": name, "harnesses": harnesses(), "alone": alone(), "providers": {"local": 1}}),
        )
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
        assert_eq!(listed["runners"][0]["alone"], alone());
        let now = hub.server.routes.now();
        let gpu = now.iter().find(|r| r.name == "gpu-box").unwrap();
        let names: Vec<String> = gpu.workers.iter().map(|w| w.worker.name()).collect();
        assert_eq!(
            names,
            [
                "claude-code on sonnet on gpu-box",
                "opencode on local:qwen3.5:9b on gpu-box",
                "local:qwen3.5:9b alone on gpu-box",
            ]
        );
        assert_eq!(listed["runners"][0]["providers"], json!({"local": 1}));
        assert_eq!(
            listed["runners"][0]["busy"],
            json!({"harnesses": {}, "providers": {}})
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_runner_says_how_busy_it_is_and_its_owner_sees_it() {
        let hub = start();
        let mut laptop = runner(&hub.url, &hub.ana, "laptop").await;
        let busy = json!({"harnesses": {"opencode": 1}, "providers": {"local": 1}});
        laptop.call(RUNNER_BUSY, busy.clone()).await.unwrap();
        let mut ana = open(&hub.url, &hub.ana).await;
        let listed = ana.call(RUNNERS, json!({})).await.unwrap();
        assert_eq!(listed["runners"][0]["busy"], busy);
        let said = refused(&mut ana, RUNNER_BUSY, busy).await;
        assert!(said.contains("not said it is a runner"), "{said}");
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
