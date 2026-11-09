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
/// such as remote organizations.
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

/// The state of the work done in `workdir`: its tasks, runs and worktrees.
/// Named for the folder, with a hash of its whole path so two folders of the
/// same name do not share one.
pub fn dir_for(workdir: &Path) -> Result<PathBuf, String> {
    let canonical = workdir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", workdir.display()))?;
    Ok(root()?.join("workdirs").join(labelled(&canonical)))
}

/// What is shared by every project that uses the organization at `config`: how
/// many runs each agent has started, because a subscription or a model server
/// is shared too.
pub fn org_dir(config: &Path) -> Result<PathBuf, String> {
    let canonical = config
        .canonicalize()
        .map_err(|e| format!("{}: {e}", config.display()))?;
    Ok(root()?.join("orgs").join(labelled(&canonical)))
}

pub(crate) fn labelled_text(text: &str) -> String {
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
