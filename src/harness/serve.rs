//! `ensemble serve`: run a working folder's tasks with no agent session around
//! them, so a queue goes on after the manager has closed.

use super::lock;
use super::queue::{Queue, Task};
use crate::router::Ensemble;
use crate::tasks::Store;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Work through the tasks of `ensemble`'s working folder until `stop` is set.
/// Running tasks are then stopped and queued again for the next worker; tasks
/// that had not started stay queued. Fails when another worker has the folder.
///
/// When nothing is left to do it says so, once, and goes on waiting for the
/// next task; with `exit_when_idle` it stops there instead.
pub fn run(
    ensemble: &Ensemble,
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
    let queue = Queue::new(ensemble, Some(store));
    say(&format!(
        "serving the tasks in {}",
        state.join("tasks").display()
    ));
    std::thread::scope(|scope| {
        let queue = &queue;
        scope.spawn(move || {
            queue.work(&|task: Task<'_>| {
                scope.spawn(task);
            })
        });
        let mut idle = false;
        let mut checked = std::time::Instant::now() - Duration::from_secs(60);
        while !stop.load(Ordering::Relaxed) {
            if checked.elapsed() >= Duration::from_secs(1) {
                checked = std::time::Instant::now();
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
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        say("stopping: running tasks are stopped and queued again");
        queue.close();
    });
    Ok(())
}
