//! Where Ensemble keeps everything on a computer: one folder, `--config-dir`
//! (by default `$XDG_STATE_HOME/ensemble`, that is
//! `~/.local/state/ensemble/`), with a folder for a hub, `hub/`, and one for
//! a runner, `runner/`, each of its own: neither reads the other's.
//!
//! ```text
//! <config-dir>/
//!   hub/                 a hub this computer runs (HubDir)
//!     core/              database.db (settings, accounts, sealed keys), master.key
//!     knowledge/         vault/ (the notes, a git repository), embeddings.db
//!     tasks/             database.db (the tasks it keeps)
//!     trackers/          database.db (which issue became which task)
//!   runner/              this computer's runner (RunnerDir)
//!     core/              database.db (settings, sealed keys, hub tokens), master.key
//!     teams/             its helper teams, a folder each
//!     workdirs/          the runs and worktrees of each repository it works in
//!     orgs/              what agents of each team used
//! ```

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The database file in a folder of a hub's or a runner's, such as its core.
pub const DB_FILE: &str = "database.db";

/// The folder `--config-dir` names, for the rest of this process.
static ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Keep everything in `dir` for the rest of this process, as `--config-dir`
/// says, in place of `$XDG_STATE_HOME/ensemble`. The first call wins.
pub fn use_root(dir: &Path) -> Result<(), String> {
    let dir = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("cannot read the current directory: {e}"))?
            .join(dir)
    };
    let _ = ROOT.set(dir);
    Ok(())
}

/// The folder everything is kept in: `--config-dir`, or else
/// `$XDG_STATE_HOME/ensemble`, or `~/.local/state/ensemble`.
pub fn root() -> Result<PathBuf, String> {
    if let Some(dir) = ROOT.get() {
        return Ok(dir.clone());
    }
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .ok_or("cannot find a state directory: neither XDG_STATE_HOME nor HOME is set")?;
            PathBuf::from(home).join(".local/state")
        }
    };
    Ok(base.join("ensemble"))
}

/// `$XDG_CACHE_HOME/ensemble`, or `~/.cache/ensemble`: what can be fetched again,
/// such as remote helper teams.
pub fn cache_root() -> Result<PathBuf, String> {
    let base = match std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .ok_or("cannot find a cache directory: neither XDG_CACHE_HOME nor HOME is set")?;
            PathBuf::from(home).join(".cache")
        }
    };
    Ok(base.join("ensemble"))
}

/// The folder of a hub, `<config-dir>/hub/`: a folder for its core and one
/// for each of its connectors that keeps anything. A computer runs one hub;
/// its lock and its socket are named for this folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubDir(PathBuf);

impl HubDir {
    /// The hub whose folder is `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        HubDir(dir.into())
    }

    /// The hub this computer runs: `<config-dir>/hub/`.
    pub fn this() -> Result<Self, String> {
        Ok(HubDir(root()?.join("hub")))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Its core: `database.db`, with its settings, its accounts and their
    /// tokens, the keys of its backends, sealed, and the audit log of its
    /// settings; and `master.key`, which seals the keys.
    pub fn core(&self) -> PathBuf {
        self.0.join("core")
    }

    /// Its core's database, where its settings are kept.
    pub fn database(&self) -> PathBuf {
        self.core().join(DB_FILE)
    }

    /// The knowledge connector's: the vault, and the vectors of its notes.
    pub fn knowledge(&self) -> PathBuf {
        self.0.join("knowledge")
    }

    /// The knowledge vault: shares and notes, Markdown files in a git
    /// repository.
    pub fn vault(&self) -> PathBuf {
        self.knowledge().join("vault")
    }

    /// The tasks connector's: the tasks the hub keeps for its accounts.
    pub fn tasks(&self) -> PathBuf {
        self.0.join("tasks")
    }

    /// The trackers': which issue of which tracker became which task; and a
    /// local tracker's folder, when it names a relative one.
    pub fn trackers(&self) -> PathBuf {
        self.0.join("trackers")
    }
}

/// The folder of this computer's runner, `<config-dir>/runner/`. A computer
/// runs one runner; its lock and its socket are named for this folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerDir(PathBuf);

impl RunnerDir {
    /// The runner whose folder is `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        RunnerDir(dir.into())
    }

    /// This computer's runner: `<config-dir>/runner/`.
    pub fn this() -> Result<Self, String> {
        Ok(RunnerDir(root()?.join("runner")))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Its core: `database.db`, with its settings, the keys of its backends,
    /// sealed, its hub tokens, sealed, and the audit log of its settings; and
    /// `master.key`, which seals them.
    pub fn core(&self) -> PathBuf {
        self.0.join("core")
    }

    /// Its core's database, where its settings are kept.
    pub fn database(&self) -> PathBuf {
        self.core().join(DB_FILE)
    }

    /// Its helper teams: a folder per team, named for it.
    pub fn teams(&self) -> PathBuf {
        self.0.join("teams")
    }
}

/// The runner's helper teams: a folder per team, named for it.
pub fn teams_dir() -> Result<PathBuf, String> {
    Ok(RunnerDir::this()?.teams())
}

/// The state of the work the runner does in `workdir`: its tasks, runs and
/// worktrees. Named for the folder, with a hash of its whole path so two
/// folders of the same name do not share one.
pub fn dir_for(workdir: &Path) -> Result<PathBuf, String> {
    dir_in(RunnerDir::this()?.path(), workdir)
}

/// [`dir_for`], for the runner whose folder is `runner` instead of this
/// computer's.
pub fn dir_in(runner: &Path, workdir: &Path) -> Result<PathBuf, String> {
    let canonical = workdir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", workdir.display()))?;
    Ok(runner.join("workdirs").join(labelled(&canonical)))
}

/// What is shared by every project that uses the helper team at `config`: how
/// many runs each agent has started, because a subscription or a model server
/// is shared too.
pub fn team_dir(config: &Path) -> Result<PathBuf, String> {
    let canonical = config
        .canonicalize()
        .map_err(|e| format!("{}: {e}", config.display()))?;
    Ok(RunnerDir::this()?
        .path()
        .join("orgs")
        .join(labelled(&canonical)))
}

/// What this computer's runner keeps while it runs: its lock.
pub fn runner_dir() -> Result<PathBuf, String> {
    Ok(RunnerDir::this()?.path().to_path_buf())
}

/// What a hub this computer runs keeps while it runs: its lock.
pub fn hub_dir() -> Result<PathBuf, String> {
    Ok(HubDir::this()?.path().to_path_buf())
}

/// Where the hub whose state is in `hub_dir` takes the operations of
/// `ensemble-hub …`, as [`runner_socket`] says.
pub fn hub_socket(hub_dir: &Path) -> PathBuf {
    runner_socket(hub_dir)
}

/// Where the runner whose state is in `runner_dir` takes the operations of
/// `ensemble-runner …`. A socket's path must be short, so it is not in the
/// state directory but in the runtime one (`$XDG_RUNTIME_DIR`, else `/tmp`),
/// in a folder only its user may enter, named by a hash of `runner_dir`.
pub fn runner_socket(runner_dir: &Path) -> PathBuf {
    // What the kernel takes, less room to spare.
    const MAX_SOCKET_PATH: usize = 100;
    let file = format!(
        "{:016x}.sock",
        fnv1a(runner_dir.to_string_lossy().as_bytes())
    );
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(|d| PathBuf::from(d).join("ensemble").join(&file))
        .filter(|p| p.as_os_str().len() <= MAX_SOCKET_PATH);
    runtime.unwrap_or_else(|| {
        let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
        PathBuf::from("/tmp")
            .join(format!("ensemble-{}", labelled_text(&user)))
            .join(file)
    })
}
pub fn labelled_text(text: &str) -> String {
    labelled(Path::new(text))
}

fn labelled(path: &Path) -> String {
    let name: String = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{name}-{:016x}", fnv1a(path.to_string_lossy().as_bytes()))
}

/// FNV-1a, 64 bits. Not `DefaultHasher`, whose output may change between Rust
/// versions and would orphan every directory.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_is_stable_and_tells_folders_of_one_name_apart() {
        assert_eq!(fnv1a(b"/a/proj"), fnv1a(b"/a/proj"));
        assert_ne!(fnv1a(b"/a/proj"), fnv1a(b"/b/proj"));
        assert_eq!(fnv1a(b""), 0xcbf29ce484222325);
        let a = labelled(Path::new("/a/proj"));
        let b = labelled(Path::new("/b/proj"));
        assert!(a.starts_with("proj-") && b.starts_with("proj-") && a != b);
    }

    #[test]
    fn a_folder_name_is_made_safe_for_a_path() {
        assert!(labelled(Path::new("/x/my project!")).starts_with("my-project--"));
    }
}
