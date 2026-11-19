//! The runners connected to a hub server.
//!
//! A runner is a connection that said `runner`: it has an owner (the account
//! it signed in as), a name, whom it works for (`owner`: its owner; `hub`:
//! also every account whose role has `compute`), every agent of its teams,
//! and the agents it lends other accounts (those on LLM backends, which the
//! runner works out). The hub keeps no runner when its connection is gone;
//! runners are presence, not state. Who is connected says which runners may
//! take an account's tasks, and whether a task they gave back is blocked
//! (`board`).
//!
//! Every connection has an outbox here: events for it are put there by
//! whichever connection has them, and the connection's own task sends them.

use crate::board::{Lender, Present};
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
    /// It works for the hub: also for every account whose role has
    /// `compute`.
    for_hub: bool,
    /// Every agent of its teams, with the first line of each one's prompt.
    agents: Vec<(String, String)>,
    /// The agents it lends other accounts, when it works for the hub.
    lends: Vec<String>,
}

impl Runner {
    fn id(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// Does it work for `user`? For its owner; for others when it works for
    /// the hub and their role has `compute`.
    fn serves(&self, user: &str, compute: bool) -> bool {
        self.owner == user || (self.for_hub && compute)
    }

    /// Its agents that work for `user`: all of them for its owner, those it
    /// lends for anyone else.
    fn agents_for(&self, user: &str) -> Vec<&(String, String)> {
        self.agents
            .iter()
            .filter(|(name, _)| self.owner == user || self.lends.contains(name))
            .collect()
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

    /// Make `conn` a runner of `owner`, or say again what it has. Its name
    /// must not be taken by another connection of the same owner.
    pub(crate) fn register(
        &self,
        conn: u64,
        owner: &str,
        params: &Value,
        may_host: bool,
    ) -> Result<Value, String> {
        let name = params["name"].as_str().unwrap_or("").trim().to_string();
        if !ensemble_config::runner::valid_name(&name) {
            return Err(format!(
                "runner: '{name}' is not a runner name: letters, digits, '-', '_' or '.'"
            ));
        }
        let for_hub = match params["works_for"].as_str() {
            Some("hub") => true,
            Some("owner") => false,
            other => {
                return Err(format!(
                    "runner: `works_for` must be owner or hub, not {other:?}"
                ));
            }
        };
        if for_hub && !may_host {
            return Err(
                "runner: a runner that works for the hub needs the `host` permission in your role; say `works_for: owner`, or ask an admin"
                    .into(),
            );
        }
        let texts = |key: &str| -> Vec<String> {
            params[key]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default()
        };
        let agents: Vec<(String, String)> = params["agents"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| {
                        Some((
                            v["name"].as_str()?.to_string(),
                            v["about"].as_str().unwrap_or("").to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let lends = if for_hub { texts("lends") } else { Vec::new() };
        if let Some(stray) = lends
            .iter()
            .find(|l| !agents.iter().any(|(name, _)| name == *l))
        {
            return Err(format!(
                "runner: it lends '{stray}', which is not one of its agents"
            ));
        }
        let runner = Runner {
            owner: owner.to_string(),
            name,
            for_hub,
            agents,
            lends,
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

    /// The runners that work for `user`, each with its agents that do.
    pub(crate) fn listed(&self, user: &str, compute: bool) -> Value {
        let inner = self.lock();
        let mut runners: Vec<Value> = inner
            .runners
            .values()
            .filter(|r| r.serves(user, compute))
            .map(|r| {
                json!({
                    "runner": r.id(),
                    "owner": r.owner,
                    "works_for": if r.for_hub { "hub" } else { "owner" },
                    "agents": r.agents_for(user).iter().map(|(name, about)| json!({"name": name, "about": about})).collect::<Vec<_>>(),
                })
            })
            .collect();
        runners.sort_by(|a, b| a["runner"].as_str().cmp(&b["runner"].as_str()));
        json!({ "runners": runners })
    }

    /// The runners connected now that may take `account`'s tasks: its own,
    /// by their names, with every agent; and, when its role has `compute`,
    /// others' that work for the hub, as `<owner>/<name>`, with the agents
    /// they lend. `compute` is asked only when such a runner is connected.
    pub(crate) fn present(&self, account: &str, compute: impl FnOnce() -> bool) -> Vec<Present> {
        let lending = self
            .lock()
            .runners
            .values()
            .any(|r| r.owner != account && r.for_hub && !r.lends.is_empty());
        // Asked with the lock let go: it reads the accounts.
        let compute = lending && compute();
        let mut present: Vec<Present> = self
            .lock()
            .runners
            .values()
            .filter_map(|r| {
                if r.owner == account {
                    Some(Present {
                        name: r.name.clone(),
                        agents: r.agents.iter().map(|(name, _)| name.clone()).collect(),
                        own: true,
                    })
                } else {
                    (r.for_hub && compute && !r.lends.is_empty()).then(|| Present {
                        name: r.id(),
                        agents: r.lends.clone(),
                        own: false,
                    })
                }
            })
            .collect();
        present.sort_by(|a, b| (!a.own, &a.name).cmp(&(!b.own, &b.name)));
        present
    }

    /// `owner`'s runner `name`, connected now, as a lender to other accounts:
    /// when it works for the hub.
    pub(crate) fn lender(&self, owner: &str, name: &str) -> Option<Lender> {
        self.lock()
            .runners
            .values()
            .find(|r| r.owner == owner && r.name == name && r.for_hub)
            .map(|r| Lender {
                name: r.id(),
                lends: r.lends.clone(),
            })
    }
}

#[cfg(test)]
mod tests {
    use crate::board::{Lender, Present};
    use crate::server::testing::start;
    use ensemble_link::client::Connection;
    use ensemble_link::protocol::*;
    use serde_json::{Value, json};
    use std::time::Duration;

    /// A runner that works for `works_for`, with two agents, lending the
    /// reviewer when it works for the hub.
    async fn runner(url: &str, token: &str, name: &str, works_for: &str) -> Connection {
        let mut link = Connection::open(url, token).await.unwrap();
        link.call(
            RUNNER,
            json!({"name": name, "works_for": works_for,
                   "agents": [{"name": "web/coder", "about": "Writes code."},
                              {"name": "web/reviewer", "about": "Reviews it."}],
                   "lends": ["web/reviewer"]}),
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

    /// The names of the agents of the runner listed `at`.
    fn agents(listed: &Value, at: usize) -> Vec<&str> {
        listed["runners"][at]["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_runner_is_listed_to_its_owner_with_all_its_agents_and_to_others_with_what_it_lends()
     {
        let hub = start();
        // root may connect a runner that works for the hub; ana, a member,
        // may not.
        let _gpu = runner(&hub.url, &hub.root, "gpu-box", "hub").await;
        let mut ana = open(&hub.url, &hub.ana).await;
        let said = refused(
            &mut ana,
            RUNNER,
            json!({"name": "laptop", "works_for": "hub"}),
        )
        .await;
        assert!(said.contains("`host`"), "{said}");
        let said = refused(
            &mut ana,
            RUNNER,
            json!({"name": "laptop", "works_for": "shared"}),
        )
        .await;
        assert!(said.contains("owner or hub"), "{said}");
        // What it lends must be its own.
        let mut root = open(&hub.url, &hub.root).await;
        let said = refused(
            &mut root,
            RUNNER,
            json!({"name": "lab", "works_for": "hub",
                   "agents": [{"name": "web/coder", "about": ""}], "lends": ["pcb/router"]}),
        )
        .await;
        assert!(said.contains("not one of its agents"), "{said}");
        // A runner that works for its owner is listed to its owner all the
        // same, and lends nothing, whatever it says.
        ana.call(
            RUNNER,
            json!({"name": "laptop", "works_for": "owner",
                   "agents": [{"name": "web/coder", "about": ""}], "lends": ["web/coder"]}),
        )
        .await
        .unwrap();
        let listed = ana.call(RUNNERS, json!({})).await.unwrap();
        assert_eq!(listed["runners"][0]["runner"], "ana/laptop");
        assert_eq!(listed["runners"][0]["owner"], "ana");
        assert_eq!(listed["runners"][0]["works_for"], "owner");
        assert_eq!(agents(&listed, 0), ["web/coder"]);
        assert_eq!(listed["runners"][1]["runner"], "root/gpu-box");
        assert_eq!(listed["runners"][1]["works_for"], "hub");
        assert_eq!(agents(&listed, 1), ["web/reviewer"], "what it lends ana");
        assert_eq!(listed["runners"][1]["agents"][0]["about"], "Reviews it.");
        let listed = root.call(RUNNERS, json!({})).await.unwrap();
        assert_eq!(
            listed["runners"].as_array().unwrap().len(),
            1,
            "ana's works for ana alone"
        );
        assert_eq!(agents(&listed, 0), ["web/coder", "web/reviewer"]);
        // Said again, as when its teams change: what it has now.
        ana.call(
            RUNNER,
            json!({"name": "laptop", "works_for": "owner", "agents": []}),
        )
        .await
        .unwrap();
        let listed = ana.call(RUNNERS, json!({})).await.unwrap();
        assert!(agents(&listed, 0).is_empty());

        // A reader has no `compute`: root's runner does not work for them.
        let mut rex = open(&hub.url, &hub.rex).await;
        assert_eq!(
            rex.call(RUNNERS, json!({})).await.unwrap()["runners"],
            json!([])
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_runners_name_is_taken_once_and_it_is_present_while_connected() {
        let hub = start();
        let laptop = runner(&hub.url, &hub.ana, "laptop", "owner").await;
        let mut again = open(&hub.url, &hub.ana).await;
        let said = refused(
            &mut again,
            RUNNER,
            json!({"name": "laptop", "works_for": "owner"}),
        )
        .await;
        assert!(said.contains("connected already"), "{said}");
        let present = hub.server.routes.present("ana", || false);
        assert_eq!(present.len(), 1);
        assert_eq!(
            (present[0].name.as_str(), present[0].agents.clone()),
            (
                "laptop",
                vec!["web/coder".to_string(), "web/reviewer".into()]
            )
        );
        assert!(present[0].own);
        assert!(hub.server.routes.present("root", || false).is_empty());
        assert_eq!(hub.server.routes.lender("ana", "laptop"), None);

        // root's runner works for the hub: it may take ana's tasks, when
        // ana's role has `compute`, with what it lends, after ana's own.
        let _gpu = runner(&hub.url, &hub.root, "gpu-box", "hub").await;
        let present = hub.server.routes.present("ana", || true);
        assert_eq!(
            present[1],
            Present {
                name: "root/gpu-box".into(),
                agents: vec!["web/reviewer".into()],
                own: false,
            }
        );
        assert_eq!(hub.server.routes.present("ana", || false).len(), 1);
        assert_eq!(
            hub.server.routes.lender("root", "gpu-box"),
            Some(Lender {
                name: "root/gpu-box".into(),
                lends: vec!["web/reviewer".into()],
            })
        );
        assert_eq!(hub.server.routes.lender("ana", "gpu-box"), None);
        let mut laptop = laptop;
        laptop.close().await;
        drop(laptop);
        let gone = async {
            while !hub.server.routes.present("ana", || false).is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), gone)
            .await
            .expect("a runner that went away is not present");
    }
}
