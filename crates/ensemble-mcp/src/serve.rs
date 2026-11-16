//! `ensemble serve`: run a working folder's tasks with no agent session around
//! them, so a queue goes on after the manager has closed.

use crate::Hub;
use ensemble_agents::router::Ensemble;
use ensemble_agents::runtime;
use ensemble_tasks::lock;
use ensemble_tasks::tasks::Store;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Work through the tasks of `ensemble`'s working folder until `stop` is set.
/// Running tasks are then stopped and queued again for the next worker; tasks
/// that had not started stay queued. Fails when another worker has the folder.
///
/// When nothing is left to do it says so, once, and goes on waiting for the
/// next task; with `exit_when_idle` it stops there instead. With a hub, shares
/// become notes and issues become tasks alongside.
pub fn run(
    ensemble: &Ensemble,
    hub: Hub<'_>,
    stop: &AtomicBool,
    say: &(dyn Fn(&str) + Sync),
    exit_when_idle: bool,
) -> Result<(), String> {
    let state = ensemble.state_dir().map_err(|e| e.to_string())?;
    let _lock = lock::acquire(&state).map_err(|pid| {
        format!(
            "another worker (process {pid}) is already running the tasks of this folder. Stop it first, or just use `ensemble mcp`, which leaves the tasks to it."
        )
    })?;
    let mut store = Store::new(&state);
    if let Some(usage) = ensemble.usage_dir() {
        store = store.with_usage_dir(usage);
    }
    crate::note_recall_without_knowledge(ensemble, hub.knowledge);
    let queue = crate::queue_for(ensemble, Some(store), hub.knowledge);
    say(&format!(
        "serving the tasks in {}",
        state.join("tasks").display()
    ));
    let hub_stop = &AtomicBool::new(false);
    std::thread::scope(|scope| {
        let queue = &queue;
        if let Some(memory) = hub.distill {
            scope.spawn(move || memory.work(hub_stop, say));
        }
        if let Some(inbox) = hub.inbox {
            scope.spawn(move || inbox.work(hub_stop, say));
        }
        // Says when the worker runs out of work and when it has some again,
        // and closes the queue when it is time to stop.
        let watch = async {
            let mut idle = false;
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            while !stop.load(Ordering::Relaxed) {
                match queue.idle() {
                    Some(line) if !idle => {
                        idle = true;
                        say(&line);
                        if exit_when_idle {
                            break;
                        }
                    }
                    None if idle => {
                        idle = false;
                        say("working again");
                    }
                    _ => {}
                }
                tokio::select! {
                    _ = tick.tick() => {}
                    () = runtime::until_set(stop) => {}
                }
            }
            say("stopping: running tasks are stopped and queued again");
            queue.close();
            hub_stop.store(true, Ordering::Relaxed);
        };
        let node = async {
            if let Some(node) = hub.node {
                node.work(hub_stop, say).await;
            }
        };
        runtime::block_on(async { tokio::join!(queue.work(true), watch, node) });
        hub_stop.store(true, Ordering::Relaxed);
    });
    Ok(())
}
