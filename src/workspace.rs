//! The project directory an agent works in, and what a run changed in it.
//!
//! The directory is fixed when the server starts; no request can name another.
//! An external agent edits files itself, so Ensemble finds out what changed by
//! comparing the project before and after the run.

use std::fs;
use std::path::{Path, PathBuf};

/// Directories a snapshot does not look into: they are large and the agent's
/// changes there are not what a caller wants listed.
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules"];

/// Above this many files a snapshot is not taken; changes then go unreported
/// rather than every run paying for a huge walk.
const MAX_SNAPSHOT_FILES: usize = 50_000;

#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn new(workdir: &Path) -> Result<Self, String> {
        let root = workdir
            .canonicalize()
            .map_err(|e| format!("working directory {}: {e}", workdir.display()))?;
        if !root.is_dir() {
            return Err(format!("{} is not a directory", root.display()));
        }
        Ok(Workspace { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `path` relative to the root, as a caller should see it.
    pub fn show(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .display()
            .to_string()
    }
}

/// What happened to one file during a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Added,
    Modified,
    Deleted,
}

impl Change {
    pub fn as_str(self) -> &'static str {
        match self {
            Change::Added => "added",
            Change::Modified => "modified",
            Change::Deleted => "deleted",
        }
    }
}

/// Every file a run changed, by project-relative path.
pub type Changes = std::collections::BTreeMap<String, Change>;

/// Fold one change into what is already known about that file: a file added
/// and then edited is still added, and one added then deleted never existed.
pub fn record(changes: &mut Changes, path: String, change: Change) {
    match (changes.get(&path), change) {
        (Some(Change::Added), Change::Modified) => {}
        (Some(Change::Added), Change::Deleted) => {
            changes.remove(&path);
        }
        (Some(Change::Deleted), Change::Added) => {
            changes.insert(path, Change::Modified);
        }
        _ => {
            changes.insert(path, change);
        }
    }
}

/// Size and modification time of every project file, skipping [`SKIP_DIRS`].
/// Comparing two of them shows what a run changed.
pub type Snapshot = std::collections::BTreeMap<String, (Option<std::time::SystemTime>, u64)>;

pub fn snapshot(ws: &Workspace) -> Option<Snapshot> {
    let mut out = Snapshot::new();
    let mut too_many = false;
    walk(ws.root(), &mut |p| {
        if out.len() >= MAX_SNAPSHOT_FILES {
            too_many = true;
            return;
        }
        if let Ok(meta) = fs::metadata(p) {
            out.insert(ws.show(p), (meta.modified().ok(), meta.len()));
        }
    });
    (!too_many).then_some(out)
}

pub fn diff(before: &Snapshot, after: &Snapshot, changes: &mut Changes) {
    for (path, stamp) in after {
        match before.get(path) {
            None => record(changes, path.clone(), Change::Added),
            Some(old) if old != stamp => record(changes, path.clone(), Change::Modified),
            Some(_) => {}
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            record(changes, path.clone(), Change::Deleted);
        }
    }
}

/// Call `visit` for every file under `dir`, skipping [`SKIP_DIRS`]. Symlinks
/// are not followed: a link is neither visited nor descended into.
fn walk(dir: &Path, visit: &mut dyn FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            let name = entry.file_name();
            if !SKIP_DIRS.iter().any(|s| name == *s) {
                walk(&path, visit);
            }
        } else if kind.is_file() {
            visit(&path);
        }
    }
}

#[cfg(test)]
pub mod testing {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    /// A fresh empty directory, removed on drop.
    pub struct TempDir(pub PathBuf);

    impl Default for TempDir {
        fn default() -> Self {
            Self::new()
        }
    }

    impl TempDir {
        pub fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "ensemble-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Write `body` as an executable `#!/bin/sh` script in `dir`: a stand-in
    /// for an agent CLI, named in a config as its `command`.
    pub fn executable(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
}

#[cfg(test)]
mod tests {
    use super::testing::TempDir;
    use super::*;

    fn project() -> (TempDir, Workspace) {
        let tmp = TempDir::new();
        let root = tmp.0.join("p");
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("src/deep/lib.rs"), "let x = 2;\n").unwrap();
        fs::write(root.join("target/junk.rs"), "let x = 3;\n").unwrap();
        fs::write(root.join("README.md"), "hello\n").unwrap();
        let ws = Workspace::new(&root).unwrap();
        (tmp, ws)
    }

    #[test]
    fn a_missing_directory_is_refused() {
        let tmp = TempDir::new();
        let err = Workspace::new(&tmp.0.join("nope")).unwrap_err();
        assert!(err.contains("working directory"), "{err}");
    }

    #[test]
    fn paths_are_shown_relative_to_the_root() {
        let (_t, ws) = project();
        assert_eq!(ws.show(&ws.root().join("src/main.rs")), "src/main.rs");
    }

    #[test]
    fn a_snapshot_diff_finds_added_modified_and_deleted_files() {
        let (tmp, ws) = project();
        let before = snapshot(&ws).unwrap();
        fs::write(tmp.0.join("p/new.txt"), "n").unwrap();
        fs::write(tmp.0.join("p/README.md"), "changed and longer\n").unwrap();
        fs::remove_file(tmp.0.join("p/src/deep/lib.rs")).unwrap();
        fs::write(tmp.0.join("p/target/junk.rs"), "ignored").unwrap();
        let mut changes = Changes::new();
        diff(&before, &snapshot(&ws).unwrap(), &mut changes);
        let got: Vec<(&str, Change)> = changes.iter().map(|(p, c)| (p.as_str(), *c)).collect();
        assert_eq!(
            got,
            vec![
                ("README.md", Change::Modified),
                ("new.txt", Change::Added),
                ("src/deep/lib.rs", Change::Deleted),
            ]
        );
    }

    #[test]
    fn changes_to_one_file_fold_into_one_entry() {
        let mut c = Changes::new();
        record(&mut c, "a".into(), Change::Added);
        record(&mut c, "a".into(), Change::Modified);
        assert_eq!(c["a"], Change::Added);
        record(&mut c, "a".into(), Change::Deleted);
        assert!(c.is_empty(), "added then deleted never existed");
        record(&mut c, "b".into(), Change::Deleted);
        record(&mut c, "b".into(), Change::Added);
        assert_eq!(c["b"], Change::Modified);
    }
}
