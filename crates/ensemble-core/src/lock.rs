//! One worker per directory. The worker that runs a folder's tasks, in
//! `ensemble mcp` or in `ensemble serve`, holds a lock file in the folder's
//! state directory, and the one that distills a knowledge vault holds one in
//! the vault's `.git`; another that finds it held leaves that work to it.

use std::path::{Path, PathBuf};

const LOCK_FILE: &str = "worker.lock";

/// The lock, held until it is dropped.
#[derive(Debug)]
pub struct WorkerLock {
    path: PathBuf,
}

impl Drop for WorkerLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Is a process with this id running?
#[cfg(unix)]
fn alive(pid: u32) -> bool {
    // Signal 0 checks that the process exists without sending anything. A
    // process of another user answers EPERM, and is alive.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn alive(_: u32) -> bool {
    true
}

/// The process that holds the lock in `state`, if there is one running. A lock
/// left behind by a process that died is not a holder.
pub fn holder(state: &Path) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(state.join(LOCK_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    alive(pid).then_some(pid)
}

/// Take the lock in `state`, or say which process has it.
pub fn acquire(state: &Path) -> Result<WorkerLock, u32> {
    let path = state.join(LOCK_FILE);
    let _ = std::fs::create_dir_all(state);
    for _ in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                let _ = write!(file, "{}", std::process::id());
                return Ok(WorkerLock { path });
            }
            Err(_) => match holder(state) {
                Some(pid) => return Err(pid),
                // Stale: its process is gone. Clear it and try once more.
                None => {
                    let _ = std::fs::remove_file(&path);
                }
            },
        }
    }
    // Lost a race for it twice: someone else has it now.
    Err(holder(state).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::testing::TempDir;

    #[test]
    fn a_held_lock_names_its_holder_and_is_released_on_drop() {
        let tmp = TempDir::new();
        let lock = acquire(&tmp.0).unwrap();
        assert_eq!(holder(&tmp.0), Some(std::process::id()));
        assert_eq!(acquire(&tmp.0).unwrap_err(), std::process::id());
        drop(lock);
        assert_eq!(holder(&tmp.0), None);
        assert!(acquire(&tmp.0).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_lock_left_by_a_dead_process_is_taken_over() {
        let tmp = TempDir::new();
        // A process that has certainly ended: wait for a child to exit.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        std::fs::write(tmp.0.join(LOCK_FILE), dead.to_string()).unwrap();
        assert_eq!(holder(&tmp.0), None);
        let lock = acquire(&tmp.0).unwrap();
        assert_eq!(holder(&tmp.0), Some(std::process::id()));
        drop(lock);
    }

    #[test]
    fn nonsense_in_the_lock_file_is_no_holder() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join(LOCK_FILE), "not a pid").unwrap();
        assert_eq!(holder(&tmp.0), None);
        assert!(acquire(&tmp.0).is_ok());
    }
}
