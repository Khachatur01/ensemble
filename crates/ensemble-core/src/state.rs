//! Where Ensemble keeps what it learns while it runs: per-user state, never
//! configuration and never in the project. Tasks, run logs, worktrees and usage
//! counters live under `$XDG_STATE_HOME/ensemble/` (by default
//! `~/.local/state/ensemble/`), one directory per working directory.

use std::path::{Path, PathBuf};

/// `$XDG_STATE_HOME/ensemble`, or `~/.local/state/ensemble`.
pub fn root() -> Result<PathBuf, String> {
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

/// `$XDG_CONFIG_HOME/ensemble`, or `~/.config/ensemble`: everything a person
/// sets up on this computer, in two fixed folders: `node/` for its node and
/// `hub/` for a hub it runs.
pub fn config_root() -> Result<PathBuf, String> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .ok_or("cannot find a config directory: neither XDG_CONFIG_HOME nor HOME is set")?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("ensemble"))
}

/// This computer's node: `node.yaml`, its helper teams in `teams/`, and its
/// hub tokens in `hubs.json`.
pub fn node_config_dir() -> Result<PathBuf, String> {
    Ok(config_root()?.join("node"))
}

/// The node's helper teams: a folder per team, named for it.
pub fn teams_dir() -> Result<PathBuf, String> {
    Ok(node_config_dir()?.join("teams"))
}

/// A hub this computer runs (`ensemble hub serve`): `hub.yaml`, its
/// `database.db` of accounts and deliveries, and its knowledge vault.
pub fn hub_config_dir() -> Result<PathBuf, String> {
    Ok(config_root()?.join("hub"))
}

/// The state of the work done in `workdir`: its tasks, runs and worktrees.
/// Named for the folder, with a hash of its whole path so two folders of the
/// same name do not share one.
pub fn dir_for(workdir: &Path) -> Result<PathBuf, String> {
    dir_in(&root()?, workdir)
}

/// [`dir_for`], under the state directory `root` instead of this user's.
pub fn dir_in(root: &Path, workdir: &Path) -> Result<PathBuf, String> {
    let canonical = workdir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", workdir.display()))?;
    Ok(root.join("workdirs").join(labelled(&canonical)))
}

/// What is shared by every project that uses the helper team at `config`: how
/// many runs each agent has started, because a subscription or a model server
/// is shared too.
pub fn team_dir(config: &Path) -> Result<PathBuf, String> {
    let canonical = config
        .canonicalize()
        .map_err(|e| format!("{}: {e}", config.display()))?;
    Ok(root()?.join("orgs").join(labelled(&canonical)))
}

/// What this computer's node keeps: its lock, the folders it worked in, its
/// runs for others. A computer runs one node.
pub fn node_dir() -> Result<PathBuf, String> {
    Ok(root()?.join("node"))
}

/// Where the node whose state is in `node_dir` listens for `ensemble mcp`. A
/// socket's path must be short, so it is not in the state directory but in
/// the runtime one (`$XDG_RUNTIME_DIR`, else `/tmp`), in a folder only its
/// user may enter, named by a hash of `node_dir`.
pub fn node_socket(node_dir: &Path) -> PathBuf {
    // What the kernel takes, less room to spare.
    const MAX_SOCKET_PATH: usize = 100;
    let file = format!("{:016x}.sock", fnv1a(node_dir.to_string_lossy().as_bytes()));
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
