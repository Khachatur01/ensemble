//! Running an external agent's CLI: start it in the project, hand its output
//! to the adapter line by line as it arrives, and stop it — the whole process
//! tree — at the deadline or on a cancel.

use crate::port::Error;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long to wait for stderr after the child has gone. Something it left
/// running may still hold the pipe; the answer does not depend on stderr.
const READER_GRACE: Duration = Duration::from_secs(2);

/// How a run of the CLI ended.
pub struct Ended {
    /// The exit code; `None` when it was killed or died on a signal.
    pub status: Option<i32>,
    pub stderr: String,
    /// Why it was stopped, when it was: "cancelled" or the time limit.
    pub stopped: Option<String>,
}

/// One run of an agent CLI.
pub struct Run<'a> {
    /// The adapter type, for errors.
    pub backend: &'a str,
    /// The program, then arguments that come before `args`. A list so tests
    /// can stand in `sh script` for the real binary.
    pub program: &'a [String],
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub workdir: &'a Path,
    pub max_seconds: u32,
    pub cancel: &'a AtomicBool,
}

impl Run<'_> {
    /// Start the CLI and call `line` for each line of its stdout until it
    /// ends or is stopped.
    pub fn stream(self, line: &mut dyn FnMut(&str)) -> Result<Ended, Error> {
        let (exe, prefix) = self
            .program
            .split_first()
            .ok_or_else(|| Error::Unsupported {
                backend: self.backend.into(),
                detail: "no command to run".into(),
            })?;
        let mut cmd = Command::new(exe);
        cmd.args(prefix)
            .args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .current_dir(self.workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Its own process group, so a stop can take the whole tree down:
        // agent CLIs are node or bun programs that start children of their own.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| Error::Spawn {
            backend: self.backend.into(),
            program: exe.clone(),
            detail: e.to_string(),
        })?;
        let pid = child.id();

        // Read both pipes on their own threads: a child that fills a pipe
        // nobody reads blocks forever.
        let stdout = child.stdout.take().expect("stdout was piped");
        let (lines_tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for l in BufReader::new(stdout).lines() {
                let Ok(l) = l else { break };
                if lines_tx.send(l).is_err() {
                    break;
                }
            }
        });
        let mut stderr = child.stderr.take().expect("stderr was piped");
        let (err_tx, err_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            let _ = err_tx.send(s);
        });

        let deadline = Instant::now() + Duration::from_secs(u64::from(self.max_seconds));
        let mut stopped = None;
        loop {
            match lines.recv_timeout(Duration::from_millis(50)) {
                Ok(l) => line(&l),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            let why = if self.cancel.load(Ordering::Relaxed) {
                Some("cancelled".to_string())
            } else if Instant::now() >= deadline {
                Some(format!("stopped at the {}s time limit", self.max_seconds))
            } else {
                None
            };
            if why.is_some() {
                kill_group(pid);
                let _ = child.kill();
                stopped = why;
                break;
            }
        }
        let status = child.wait().ok().and_then(|s| s.code());
        let stderr = err_rx.recv_timeout(READER_GRACE).unwrap_or_default();
        Ok(Ended {
            status,
            stderr,
            stopped,
        })
    }
}

/// Check that a CLI is there and starts. `--version` spends nothing.
pub fn health(backend: &str, command: &str) -> Result<(), Error> {
    let mut child = Command::new(command)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Spawn {
            backend: backend.into(),
            program: command.into(),
            detail: e.to_string(),
        })?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().ok().flatten() {
            return if status.success() {
                Ok(())
            } else {
                let mut err = String::new();
                if let Some(mut s) = child.stderr.take() {
                    let _ = s.read_to_string(&mut err);
                }
                Err(Error::ProcessFailed {
                    backend: backend.into(),
                    code: status.code(),
                    stderr: err,
                })
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err(Error::Timeout {
                backend: backend.into(),
                seconds: 20,
            });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Kill the child's whole process group. The child was put in its own group
/// at spawn, so the group id is its pid and this reaches nothing else.
#[cfg(unix)]
fn kill_group(pid: u32) {
    // SAFETY: a plain libc call with no memory arguments, on our own group.
    unsafe {
        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: u32) {}

#[cfg(test)]
pub mod testing {
    use std::path::Path;

    /// A stand-in for an agent CLI: a shell script, run through `sh` so
    /// nothing executes a file still open for writing.
    pub fn fake(dir: &Path, body: &str) -> Vec<String> {
        let script = dir.join("fake-agent.sh");
        std::fs::write(&script, body).unwrap();
        vec!["sh".into(), script.display().to_string()]
    }

    /// Print `events` one per line, as a CLI's JSON output does.
    pub fn emitting(events: &[serde_json::Value]) -> String {
        let mut body = String::new();
        for e in events {
            body.push_str(&format!(
                "printf '%s\\n' '{}'\n",
                e.to_string().replace('\'', "'\\''")
            ));
        }
        body
    }
}
