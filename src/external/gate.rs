//! A task's gate: a command from `config.yaml` that its work must pass,
//! run in the task's working directory after the agent is done.

use super::process;
use crate::config::Gate;
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// Lines of output kept for the agent's next attempt and the result.
const KEPT_LINES: usize = 60;

/// What running the gate found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub passed: bool,
    /// How it ended: "exit 0", "exit 1", "stopped at the 60s time limit", ...
    pub ended: String,
    /// The last lines of what it printed, stdout and stderr together.
    pub output: String,
}

/// Run `gate` in `dir` until it exits, reaches its time limit, or `cancel` is
/// set.
pub fn run(gate: &Gate, dir: &Path, cancel: &AtomicBool) -> Verdict {
    let program = [
        "sh".to_string(),
        "-c".to_string(),
        // One stream, in the order it was written.
        format!("exec 2>&1\n{}", gate.command),
    ];
    let mut lines: Vec<String> = Vec::new();
    let run = process::Run {
        backend: "gate",
        program: &program,
        args: Vec::new(),
        env: Vec::new(),
        workdir: dir,
        max_seconds: gate.timeout_seconds,
        cancel,
    }
    .stream(&mut |line| lines.push(line.to_string()));
    let skip = lines.len().saturating_sub(KEPT_LINES);
    let mut output = lines[skip..].join("\n");
    if skip > 0 {
        output = format!("({skip} earlier line(s) not shown)\n{output}");
    }
    match run {
        Err(e) => Verdict {
            passed: false,
            ended: format!("did not start: {e}"),
            output,
        },
        Ok(ended) => Verdict {
            passed: ended.stopped.is_none() && ended.status == Some(0),
            ended: match (ended.stopped, ended.status) {
                (Some(why), _) => why,
                (None, Some(code)) => format!("exit {code}"),
                (None, None) => "killed by a signal".into(),
            },
            output,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::testing::TempDir;

    fn gate(command: &str, timeout_seconds: u32) -> Gate {
        Gate {
            command: command.into(),
            timeout_seconds,
        }
    }

    #[test]
    fn exit_status_zero_passes_and_anything_else_fails_with_its_output() {
        let tmp = TempDir::new();
        let no = AtomicBool::new(false);
        let ok = run(&gate("echo fine", 10), &tmp.0, &no);
        assert_eq!(
            (ok.passed, ok.ended.as_str(), ok.output.as_str()),
            (true, "exit 0", "fine")
        );
        let bad = run(&gate("echo out; echo err >&2; exit 3", 10), &tmp.0, &no);
        assert!(!bad.passed);
        assert_eq!(bad.ended, "exit 3");
        assert_eq!(bad.output, "out\nerr");
    }

    #[test]
    fn it_runs_in_the_directory_and_stops_at_its_time_limit() {
        let tmp = TempDir::new();
        std::fs::write(tmp.0.join("here"), "").unwrap();
        let no = AtomicBool::new(false);
        assert!(run(&gate("test -e here", 10), &tmp.0, &no).passed);
        let slow = run(&gate("sleep 30", 1), &tmp.0, &no);
        assert!(
            !slow.passed && slow.ended.contains("time limit"),
            "{slow:?}"
        );
    }

    #[test]
    fn only_the_last_lines_are_kept() {
        let tmp = TempDir::new();
        let v = run(
            &gate("seq 100; exit 1", 10),
            &tmp.0,
            &AtomicBool::new(false),
        );
        assert!(
            v.output.starts_with("(40 earlier line(s) not shown)\n41\n"),
            "{}",
            v.output
        );
        assert!(v.output.ends_with("\n100"));
    }
}
