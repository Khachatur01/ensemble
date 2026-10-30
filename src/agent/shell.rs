//! The `bash` tool: run a command in the project, inside a Landlock sandbox.
//!
//! What the sandbox does, and what it does not:
//!
//! * **Writes are confined** to the project directory, `/tmp` and `/dev`. A
//!   command cannot create, change or delete anything elsewhere.
//! * **Outbound TCP is denied** unless the agent sets `network: true` (needs a
//!   kernel with Landlock ABI 4, Linux 6.7). A command cannot upload what it
//!   reads. DNS over UDP and unix sockets are not covered.
//! * **Reads are confined** to the project, the system directories toolchains
//!   live in ([`SYSTEM_READABLE`]), a few toolchain directories in the home
//!   ([`HOME_READABLE`]), and whatever the agent lists in `readable`. The rest
//!   of the home — `~/.ssh`, `~/.aws`, browser profiles — cannot be read.
//!   Landlock also stops a command reading another process's environment
//!   through `/proc`.
//! * The environment is rebuilt from an allowlist, so API keys and tokens in
//!   Ensemble's own environment never reach a command.
//!
//! If the kernel has no Landlock the tool refuses to run rather than running
//! unconfined. There is no flag to override that.

use std::time::Duration;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);
/// Per stream. The tool-level cap in `tools::cap` applies on top.
const MAX_STREAM_BYTES: usize = 24_000;
/// What a command inherits from Ensemble's environment. Everything else,
/// secrets above all, is dropped.
const ENV_ALLOWLIST: &[&str] = &["PATH", "HOME", "LANG", "LC_ALL", "TERM", "USER", "SHELL"];

/// Readable by every command: where programs, libraries and system
/// configuration live. Missing ones are skipped.
pub const SYSTEM_READABLE: &[&str] = &[
    "/nix",
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/opt",
    "/etc",
    "/proc",
    "/sys",
    "/run/current-system",
    "/run/booted-system",
    "/run/wrappers",
    "/run/opengl-driver",
    "/run/systemd/resolve",
];

/// Readable by every command, relative to `$HOME`: toolchains and the
/// configuration git needs. Nothing that usually holds a secret.
pub const HOME_READABLE: &[&str] = &[
    ".cargo",
    ".rustup",
    ".npm",
    ".nix-profile",
    ".local/state/nix",
    ".gitconfig",
    ".config/git",
];

#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub allow_network: bool,
    /// Readable on top of the defaults. Absolute, or `~/` for the home.
    pub readable: Vec<String>,
}

impl Policy {
    /// Every path a command may read, with `~/` expanded. Without a `$HOME`
    /// the home entries are dropped rather than guessed.
    pub fn readable_paths(&self) -> Vec<std::path::PathBuf> {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let mut out: Vec<std::path::PathBuf> = SYSTEM_READABLE
            .iter()
            .map(std::path::PathBuf::from)
            .collect();
        if let Some(home) = &home {
            out.extend(HOME_READABLE.iter().map(|rel| home.join(rel)));
        }
        for path in &self.readable {
            match path.strip_prefix("~/") {
                Some(rel) => out.extend(home.as_ref().map(|h| h.join(rel))),
                None => out.push(path.into()),
            }
        }
        out
    }
}

#[cfg(target_os = "linux")]
pub use linux::run;

#[cfg(not(target_os = "linux"))]
pub fn run(
    _project: &std::path::Path,
    _command: &str,
    _timeout: Duration,
    _policy: &Policy,
    _stop: &dyn Fn() -> bool,
) -> Result<String, String> {
    Err("bash needs Linux (Landlock); this platform has no sandbox, so it will not run".into())
}

fn clip(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX_STREAM_BYTES {
        return text.into_owned();
    }
    let mut end = MAX_STREAM_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[output truncated]", &text[..end])
}

/// `killed` says why the command was killed, when it was.
fn render(code: Option<i32>, stdout: &[u8], stderr: &[u8], killed: Option<String>) -> String {
    let mut out = String::new();
    if !stdout.is_empty() {
        out.push_str(&clip(stdout));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    if !stderr.is_empty() {
        out.push_str("[stderr]\n");
        out.push_str(&clip(stderr));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    match (killed, code) {
        (Some(why), _) => out.push_str(&format!("[killed: {why}]")),
        (None, Some(c)) => out.push_str(&format!("[exit {c}]")),
        (None, None) => out.push_str("[terminated by a signal]"),
    }
    out
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Command, Stdio};

    // Landlock ABI, from <linux/landlock.h>.
    const SYS_CREATE_RULESET: libc::c_long = 444;
    const SYS_ADD_RULE: libc::c_long = 445;
    const SYS_RESTRICT_SELF: libc::c_long = 446;
    const CREATE_RULESET_VERSION: libc::c_ulong = 1;
    const RULE_PATH_BENEATH: libc::c_int = 1;

    const FS_EXECUTE: u64 = 1 << 0;
    const FS_WRITE_FILE: u64 = 1 << 1;
    const FS_READ_FILE: u64 = 1 << 2;
    const FS_READ_DIR: u64 = 1 << 3;
    const FS_REMOVE_DIR: u64 = 1 << 4;
    const FS_REMOVE_FILE: u64 = 1 << 5;
    const FS_MAKE_CHAR: u64 = 1 << 6;
    const FS_MAKE_DIR: u64 = 1 << 7;
    const FS_MAKE_REG: u64 = 1 << 8;
    const FS_MAKE_SOCK: u64 = 1 << 9;
    const FS_MAKE_FIFO: u64 = 1 << 10;
    const FS_MAKE_BLOCK: u64 = 1 << 11;
    const FS_MAKE_SYM: u64 = 1 << 12;
    const FS_REFER: u64 = 1 << 13; // ABI 2
    const FS_TRUNCATE: u64 = 1 << 14; // ABI 3
    const NET_CONNECT_TCP: u64 = 1 << 1; // ABI 4

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
    }

    #[repr(C, packed)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    /// Where a command may write besides the project.
    const WRITABLE_EXTRA: &[&str] = &["/tmp", "/dev"];

    fn abi_version() -> Result<i64, String> {
        // SAFETY: a version query takes no pointer arguments.
        let v = unsafe {
            libc::syscall(
                SYS_CREATE_RULESET,
                std::ptr::null::<RulesetAttr>(),
                0usize,
                CREATE_RULESET_VERSION,
            )
        };
        if v < 0 {
            Err(format!(
                "Landlock is not available on this kernel ({}), so bash will not run unconfined",
                std::io::Error::last_os_error()
            ))
        } else {
            Ok(v)
        }
    }

    /// Build the ruleset in the parent, so the forked child only has to call
    /// two async-signal-safe syscalls. Returns the ruleset fd.
    fn build_ruleset(
        project: &Path,
        extra_writable: &[&Path],
        policy: &Policy,
    ) -> Result<i32, String> {
        let abi = abi_version()?;
        let read = FS_EXECUTE | FS_READ_FILE | FS_READ_DIR;
        let mut fs = read
            | FS_WRITE_FILE
            | FS_REMOVE_DIR
            | FS_REMOVE_FILE
            | FS_MAKE_CHAR
            | FS_MAKE_DIR
            | FS_MAKE_REG
            | FS_MAKE_SOCK
            | FS_MAKE_FIFO
            | FS_MAKE_BLOCK
            | FS_MAKE_SYM;
        if abi >= 2 {
            fs |= FS_REFER;
        }
        if abi >= 3 {
            fs |= FS_TRUNCATE;
        }
        let net = if !policy.allow_network && abi >= 4 {
            NET_CONNECT_TCP
        } else {
            0
        };
        let attr = RulesetAttr {
            handled_access_fs: fs,
            handled_access_net: net,
        };
        // ABI 1-3 know only the fs field; passing the longer struct is an
        // error there, so pass the size the kernel will accept.
        let size = if abi >= 4 { 16usize } else { 8usize };
        // SAFETY: `attr` is a valid repr(C) struct of at least `size` bytes.
        let fd =
            unsafe { libc::syscall(SYS_CREATE_RULESET, &attr as *const RulesetAttr, size, 0u64) };
        if fd < 0 {
            return Err(format!(
                "could not create the sandbox: {}",
                std::io::Error::last_os_error()
            ));
        }
        let fd = fd as i32;

        let mut rules: Vec<(std::path::PathBuf, u64)> = vec![(project.to_path_buf(), fs)];
        rules.extend(extra_writable.iter().map(|p| (p.to_path_buf(), fs)));
        rules.extend(policy.readable_paths().into_iter().map(|p| (p, read)));
        for (path, access) in rules {
            if let Err(e) = allow_path(fd, &path, access) {
                // SAFETY: closing the fd we just opened.
                unsafe { libc::close(fd) };
                return Err(e);
            }
        }
        Ok(fd)
    }

    /// Rights that only make sense on a directory. Landlock refuses a rule
    /// that grants them on a file.
    const DIR_ONLY: u64 = FS_READ_DIR
        | FS_REMOVE_DIR
        | FS_REMOVE_FILE
        | FS_MAKE_CHAR
        | FS_MAKE_DIR
        | FS_MAKE_REG
        | FS_MAKE_SOCK
        | FS_MAKE_FIFO
        | FS_MAKE_BLOCK
        | FS_MAKE_SYM
        | FS_REFER;

    fn allow_path(ruleset: i32, path: &Path, access: u64) -> Result<(), String> {
        let access = if std::fs::metadata(path).is_ok_and(|m| !m.is_dir()) {
            access & !DIR_ONLY
        } else {
            access
        };
        use std::os::unix::ffi::OsStrExt;
        let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return Err(format!("bad path {}", path.display()));
        };
        // SAFETY: `c` is a valid NUL-terminated string.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            // A missing optional directory (no /dev in a chroot) grants nothing.
            return Ok(());
        }
        let rule = PathBeneathAttr {
            allowed_access: access,
            parent_fd: fd,
        };
        // SAFETY: `rule` is a valid packed struct; `fd` is open.
        let r = unsafe {
            libc::syscall(
                SYS_ADD_RULE,
                ruleset,
                RULE_PATH_BENEATH,
                &rule as *const PathBeneathAttr,
                0u32,
            )
        };
        let err = std::io::Error::last_os_error();
        // SAFETY: closing the fd we opened above.
        unsafe { libc::close(fd) };
        if r < 0 {
            return Err(format!("could not allow {}: {err}", path.display()));
        }
        Ok(())
    }

    pub fn run(
        project: &Path,
        command: &str,
        timeout: Duration,
        policy: &Policy,
        stop: &dyn Fn() -> bool,
    ) -> Result<String, String> {
        run_with(
            project,
            command,
            timeout,
            policy,
            stop,
            &WRITABLE_EXTRA.iter().map(Path::new).collect::<Vec<_>>(),
        )
    }

    pub(super) fn run_with(
        project: &Path,
        command: &str,
        timeout: Duration,
        policy: &Policy,
        stop: &dyn Fn() -> bool,
        extra_writable: &[&Path],
    ) -> Result<String, String> {
        if command.trim().is_empty() {
            return Err("command is empty".into());
        }
        let ruleset = build_ruleset(project, extra_writable, policy)?;

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(project)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        for key in ENV_ALLOWLIST {
            if let Ok(v) = std::env::var(key) {
                cmd.env(key, v);
            }
        }
        // SAFETY: only async-signal-safe syscalls run between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::syscall(SYS_RESTRICT_SELF, ruleset, 0u32) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let spawned = cmd.spawn();
        // SAFETY: the parent no longer needs the ruleset fd.
        unsafe { libc::close(ruleset) };
        let mut child = spawned.map_err(|e| format!("could not start the command: {e}"))?;
        let pid = child.id() as i32;

        let mut out_pipe = child.stdout.take().ok_or("no stdout pipe")?;
        let mut err_pipe = child.stderr.take().ok_or("no stderr pipe")?;
        let out_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out_pipe.read_to_end(&mut buf);
            buf
        });
        let err_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = err_pipe.read_to_end(&mut buf);
            buf
        });

        let deadline = std::time::Instant::now() + timeout;
        let mut killed = None;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(e) => return Err(format!("could not wait for the command: {e}")),
            }
            let why = if std::time::Instant::now() >= deadline {
                Some(format!("still running after {}s", timeout.as_secs()))
            } else if stop() {
                Some("the delegation was stopped".to_string())
            } else {
                None
            };
            if let Some(why) = why {
                // The whole group: a shell command is rarely one process.
                // SAFETY: signalling the process group we created.
                unsafe { libc::killpg(pid, libc::SIGKILL) };
                let _ = child.wait();
                killed = Some(why);
                break None;
            }
            std::thread::sleep(Duration::from_millis(15));
        };
        // A killed group closes its pipes; a daemonised grandchild that kept
        // them would block these joins, so the group kill above matters.
        let stdout = out_thread.join().unwrap_or_default();
        let stderr = err_thread.join().unwrap_or_default();
        Ok(render(
            status.and_then(|s| s.code()),
            &stdout,
            &stderr,
            killed,
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::path::PathBuf;

        /// A project and a sibling "elsewhere", both under `target/` — which is
        /// outside the `/tmp` and `/dev` the sandbox allows writing to.
        struct Dirs {
            base: PathBuf,
            project: PathBuf,
            elsewhere: PathBuf,
        }

        impl Dirs {
            fn new(name: &str) -> Self {
                let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("target")
                    .join(format!("shell-test-{name}-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&base);
                let project = base.join("project");
                let elsewhere = base.join("elsewhere");
                std::fs::create_dir_all(&project).unwrap();
                std::fs::create_dir_all(&elsewhere).unwrap();
                Dirs {
                    base,
                    project: project.canonicalize().unwrap(),
                    elsewhere: elsewhere.canonicalize().unwrap(),
                }
            }
        }

        impl Drop for Dirs {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.base);
            }
        }

        const OFF: &Policy = &Policy {
            allow_network: false,
            readable: Vec::new(),
        };

        fn go(d: &Dirs, cmd: &str) -> String {
            run(&d.project, cmd, Duration::from_secs(20), OFF, &|| false).unwrap()
        }

        #[test]
        fn reads_outside_the_project_and_the_system_are_denied() {
            let d = Dirs::new("read");
            std::fs::write(d.elsewhere.join("secret.txt"), "hunter2").unwrap();
            std::fs::write(d.project.join("mine.txt"), "visible").unwrap();
            let out = go(
                &d,
                &format!(
                    "cat mine.txt; cat '{}'; ls '{}'",
                    d.elsewhere.join("secret.txt").display(),
                    d.elsewhere.display()
                ),
            );
            assert!(out.starts_with("visible"), "{out}");
            assert!(!out.contains("hunter2"), "{out}");
            assert!(
                !out.contains("secret.txt\n"),
                "the listing is denied too: {out}"
            );
            assert!(out.contains("Permission denied"), "{out}");
        }

        #[test]
        fn a_path_the_role_lists_as_readable_can_be_read_but_not_written() {
            let d = Dirs::new("readable");
            let file = d.elsewhere.join("notes.txt");
            std::fs::write(&file, "shared notes").unwrap();
            let policy = Policy {
                allow_network: false,
                readable: vec![d.elsewhere.display().to_string()],
            };
            let out = run(
                &d.project,
                &format!("cat '{0}'; echo x >> '{0}'", file.display()),
                Duration::from_secs(20),
                &policy,
                &|| false,
            )
            .unwrap();
            assert!(out.starts_with("shared notes"), "{out}");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "shared notes");
        }

        #[test]
        fn home_entries_expand_and_unknown_homes_are_dropped() {
            let policy = Policy {
                allow_network: false,
                readable: vec!["~/.m2".into(), "/srv/data".into()],
            };
            let paths = policy.readable_paths();
            assert!(paths.contains(&PathBuf::from("/srv/data")));
            assert!(paths.contains(&PathBuf::from("/nix")));
            if let Some(home) = std::env::var_os("HOME") {
                assert!(paths.contains(&PathBuf::from(home).join(".m2")));
            }
        }

        #[test]
        fn a_command_runs_in_the_project_and_reports_its_output_and_exit_code() {
            let d = Dirs::new("basic");
            std::fs::write(d.project.join("a.txt"), "hi").unwrap();
            let out = go(&d, "pwd; cat a.txt; echo oops >&2; exit 3");
            assert!(
                out.starts_with(&format!("{}\nhi", d.project.display())),
                "{out}"
            );
            assert!(out.contains("[stderr]\noops"), "{out}");
            assert!(out.ends_with("[exit 3]"), "{out}");
        }

        #[test]
        fn writes_inside_the_project_work_and_writes_elsewhere_are_denied() {
            let d = Dirs::new("write");
            let out = go(&d, "echo ok > inside.txt && echo done");
            assert!(out.contains("done"), "{out}");
            assert_eq!(
                std::fs::read_to_string(d.project.join("inside.txt")).unwrap(),
                "ok\n"
            );

            let target = d.elsewhere.join("outside.txt");
            let out = go(&d, &format!("echo no > '{}'", target.display()));
            assert!(!target.exists(), "must not have been written: {out}");
            assert!(
                out.contains("[stderr]") && !out.ends_with("[exit 0]"),
                "{out}"
            );

            // Nor can it delete or rename a file out there.
            std::fs::write(&target, "keep").unwrap();
            go(
                &d,
                &format!("rm -f '{0}'; mv '{0}' '{0}.moved'", target.display()),
            );
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        }

        #[test]
        fn the_environment_is_rebuilt_from_an_allowlist() {
            let d = Dirs::new("env");
            // SAFETY: tests in this module that read the environment are
            // tolerant of an extra variable; none depends on it being absent.
            unsafe { std::env::set_var("ENSEMBLE_TEST_SECRET", "hunter2") };
            let out = go(&d, "echo \"[$ENSEMBLE_TEST_SECRET]\"; echo $PATH");
            assert!(out.starts_with("[]\n"), "{out}");
            assert!(
                out.lines().nth(1).is_some_and(|l| !l.is_empty()),
                "PATH survives: {out}"
            );
        }

        #[test]
        fn a_command_that_outlives_the_timeout_is_killed_with_its_children() {
            let d = Dirs::new("timeout");
            let started = std::time::Instant::now();
            let out = run(
                &d.project,
                "sleep 30 & sleep 30",
                Duration::from_millis(300),
                OFF,
                &|| false,
            )
            .unwrap();
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "returned promptly"
            );
            assert!(out.contains("[killed: still running after 0s]"), "{out}");
        }

        #[test]
        fn outbound_tcp_is_denied_unless_the_role_allows_it() {
            if abi_version().unwrap_or(0) < 4 {
                return; // this kernel cannot restrict the network
            }
            let d = Dirs::new("net");
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let probe = format!("bash -c 'echo > /dev/tcp/127.0.0.1/{port}' && echo connected");

            let denied = go(&d, &probe);
            assert!(!denied.contains("connected"), "{denied}");

            let allowed = run(
                &d.project,
                &probe,
                Duration::from_secs(20),
                &Policy {
                    allow_network: true,
                    readable: Vec::new(),
                },
                &|| false,
            )
            .unwrap();
            assert!(allowed.contains("connected"), "{allowed}");
        }

        #[test]
        fn a_stopped_delegation_kills_its_running_command() {
            let d = Dirs::new("stop");
            let started = std::time::Instant::now();
            let out = run(
                &d.project,
                "sleep 30",
                Duration::from_secs(60),
                OFF,
                &|| started.elapsed() > Duration::from_millis(200),
            )
            .unwrap();
            assert!(started.elapsed() < Duration::from_secs(10));
            assert!(
                out.contains("[killed: the delegation was stopped]"),
                "{out}"
            );
        }

        #[test]
        fn an_empty_command_is_refused() {
            let d = Dirs::new("empty");
            assert!(run(&d.project, "  ", Duration::from_secs(1), OFF, &|| false).is_err());
        }
    }
}
