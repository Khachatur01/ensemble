//! Running an external agent's CLI: start it in the project, hand its output
//! to the adapter line by line as it arrives, and stop it — the whole process
//! tree — at the deadline or on a cancel.

use crate::runtime::until_set;
use ensemble_core::port::Error;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

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

/// Kills the child's process group when dropped, so a run whose future is
/// dropped half way, such as by a cancelled caller, leaves nothing running.
struct Group(Option<u32>);

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            kill_group(pid);
        }
    }
}

impl Run<'_> {
    /// Start the CLI and call `line` for each line of its stdout until it
    /// ends or is stopped.
    pub async fn stream(self, line: &mut (dyn FnMut(&str) + Send)) -> Result<Ended, Error> {
        let (exe, prefix) = self
            .program
            .split_first()
            .ok_or_else(|| Error::Unsupported {
                backend: self.backend.into(),
                detail: "no command to run".into(),
            })?;
        let mut cmd = tokio::process::Command::new(exe);
        cmd.args(prefix)
            .args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .current_dir(self.workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Its own process group, so a stop can take the whole tree down:
        // agent CLIs are node or bun programs that start children of their own.
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = spawn(&mut cmd).await.map_err(|e| Error::Spawn {
            backend: self.backend.into(),
            program: exe.clone(),
            detail: e.to_string(),
        })?;
        let mut group = Group(child.id());

        // Both pipes are read at once: a child that fills a pipe nobody reads
        // blocks forever.
        let mut lines = BufReader::new(child.stdout.take().expect("stdout was piped")).lines();
        let mut stderr = child.stderr.take().expect("stderr was piped");
        let errors = tokio::spawn(async move {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s).await;
            s
        });

        let limit = tokio::time::sleep(Duration::from_secs(u64::from(self.max_seconds)));
        let cancelled = until_set(self.cancel);
        tokio::pin!(limit, cancelled);
        let stopped = loop {
            tokio::select! {
                l = lines.next_line() => match l {
                    Ok(Some(l)) => line(&l),
                    _ => break None,
                },
                () = &mut limit => break Some(format!("stopped at the {}s time limit", self.max_seconds)),
                () = &mut cancelled => break Some("cancelled".to_string()),
            }
        };
        if stopped.is_some() {
            if let Some(pid) = group.0.take() {
                kill_group(pid);
            }
            let _ = child.start_kill();
        }
        let status = child.wait().await.ok().and_then(|s| s.code());
        // It has ended; nothing is left of it to kill.
        group.0 = None;
        let stderr = tokio::time::timeout(READER_GRACE, errors)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        Ok(Ended {
            status,
            stderr,
            stopped,
        })
    }
}

/// Start `cmd`, trying again for a moment if its program is busy: a binary
/// being replaced, or a script another thread still had open for writing when
/// this process forked, is briefly `ETXTBSY`.
async fn spawn(cmd: &mut tokio::process::Command) -> std::io::Result<tokio::process::Child> {
    let mut tries = 0;
    loop {
        match cmd.spawn() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 20 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            other => return other,
        }
    }
}

/// Check that a CLI is there and starts. `--version` spends nothing.
pub fn health(backend: &str, command: &str) -> Result<(), Error> {
    let mut cmd = Command::new(command);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut tries = 0;
    let mut child = loop {
        match cmd.spawn() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 20 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(25));
            }
            other => break other,
        }
    }
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
