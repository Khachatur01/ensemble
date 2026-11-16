//! A folder's files, carried to a node of a hub server and back: what a run
//! there starts from, and what it changed.
//!
//! The files sent are those git tracks or would track (`git ls-files
//! --cached --others --exclude-standard`), so what `.gitignore` leaves out
//! (builds, secrets in `.env`) stays here. Symbolic links stay too. On the
//! node they are written into a folder of the run's own, and what differs
//! there afterwards comes back as [`Change`]s, which are written over the
//! folder here. Paths that leave the folder are refused on both sides.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// The most bytes of files one run may carry, either way.
pub const MAX_BYTES: usize = 24 * 1024 * 1024;

/// One file, its path relative to the folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct File {
    pub path: String,
    /// Whether it may be run.
    #[serde(default)]
    pub exec: bool,
    #[serde(with = "base64")]
    pub data: Vec<u8>,
}

/// What a run did to one file: its new content, or `None` when it deleted it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    #[serde(default)]
    pub exec: bool,
    #[serde(default, with = "base64_option")]
    pub data: Option<Vec<u8>>,
}

/// The files of the git repository at `dir` (or of the folder of it that
/// `dir` is) that git tracks or would track.
pub fn pack(dir: &Path) -> Result<Vec<File>, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{} is not in a git repository, and a run on a node takes the files git tracks: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut files = Vec::new();
    let mut total = 0;
    let mut seen = std::collections::BTreeSet::new();
    for path in out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = String::from_utf8(path.to_vec())
            .map_err(|_| "a file name is not UTF-8, which a run on a node cannot carry")?;
        // Listed twice when both in the index and changed.
        if !seen.insert(path.clone()) {
            continue;
        }
        let full = dir.join(&path);
        let Ok(meta) = std::fs::symlink_metadata(&full) else {
            continue; // tracked, but deleted
        };
        if !meta.is_file() {
            continue;
        }
        let data = std::fs::read(&full).map_err(|e| format!("{}: {e}", full.display()))?;
        total += data.len();
        if total > MAX_BYTES {
            return Err(format!(
                "the files git tracks in {} are more than {} MiB, the most a run on a node carries",
                dir.display(),
                MAX_BYTES / (1024 * 1024)
            ));
        }
        files.push(File {
            path,
            exec: executable(&meta),
            data,
        });
    }
    Ok(files)
}

/// Write `files` into `dir`.
pub fn unpack(files: &[File], dir: &Path) -> Result<(), String> {
    for f in files {
        write(dir, &f.path, &f.data, f.exec)?;
    }
    Ok(())
}

/// What differs in `dir` from `before`, the files it was given: files
/// changed or added, and files deleted. `.git` is not looked at.
pub fn changes(before: &[File], dir: &Path) -> Result<Vec<Change>, String> {
    let mut now = BTreeMap::new();
    walk(dir, dir, &mut now)?;
    let was: BTreeMap<&str, &File> = before.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut out = Vec::new();
    let mut total = 0;
    for (path, (data, exec)) in &now {
        let same = was
            .get(path.as_str())
            .is_some_and(|f| &f.data == data && f.exec == *exec);
        if !same {
            total += data.len();
            out.push(Change {
                path: path.clone(),
                exec: *exec,
                data: Some(data.clone()),
            });
        }
    }
    if total > MAX_BYTES {
        return Err(format!(
            "the run changed more than {} MiB of files, the most that is sent back",
            MAX_BYTES / (1024 * 1024)
        ));
    }
    for path in was.keys() {
        if !now.contains_key(*path) {
            out.push(Change {
                path: path.to_string(),
                exec: false,
                data: None,
            });
        }
    }
    Ok(out)
}

/// Write `changes` over `dir`, and return the paths they touched.
pub fn apply(changes: &[Change], dir: &Path) -> Result<Vec<String>, String> {
    // Every path is checked before anything is written.
    for c in changes {
        inside(dir, &c.path)?;
    }
    for c in changes {
        match &c.data {
            Some(data) => write(dir, &c.path, data, c.exec)?,
            None => match std::fs::remove_file(inside(dir, &c.path)?) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("cannot delete {}: {e}", c.path)),
            },
        }
    }
    Ok(changes.iter().map(|c| c.path.clone()).collect())
}

/// `path` under `dir`, or why it may not be: absolute, or leaving `dir`.
fn inside(dir: &Path, path: &str) -> Result<PathBuf, String> {
    let p = Path::new(path);
    let fine = !path.is_empty()
        && p.components().all(|c| matches!(c, Component::Normal(_)))
        && !p.components().any(|c| c.as_os_str() == ".git");
    if fine {
        Ok(dir.join(p))
    } else {
        Err(format!("refused the path '{path}': it leaves the folder"))
    }
}

fn write(dir: &Path, path: &str, data: &[u8], exec: bool) -> Result<(), String> {
    let full = inside(dir, path)?;
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    // A link in its place is replaced, never written through.
    if std::fs::symlink_metadata(&full).is_ok_and(|m| m.file_type().is_symlink()) {
        std::fs::remove_file(&full).map_err(|e| format!("{}: {e}", full.display()))?;
    }
    std::fs::write(&full, data).map_err(|e| format!("{}: {e}", full.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if exec { 0o755 } else { 0o644 };
        std::fs::set_permissions(&full, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("{}: {e}", full.display()))?;
    }
    Ok(())
}

fn walk(
    root: &Path,
    dir: &Path,
    out: &mut BTreeMap<String, (Vec<u8>, bool)>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name() == ".git" {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            walk(root, &path, out)?;
        } else if meta.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_str()
                .ok_or_else(|| format!("{}: the name is not UTF-8", path.display()))?
                .to_string();
            let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            out.insert(rel, (data, executable(&meta)));
        }
    }
    Ok(())
}

fn executable(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Bytes as standard base64, padded.
pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Standard base64 back to bytes.
pub fn decode(text: &str) -> Result<Vec<u8>, String> {
    let text = text.trim_end_matches('=');
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in text.bytes() {
        let v = ALPHABET.iter().position(|a| *a == c).ok_or("not base64")? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

mod base64 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(data: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::encode(data))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        super::decode(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

mod base64_option {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(data: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match data {
            Some(d) => s.serialize_some(&super::encode(d)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|t| super::decode(&t).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::testing::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn base64_goes_there_and_back() {
        for data in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"\x00\xff\x10 binary",
        ] {
            assert_eq!(decode(&encode(data)).unwrap(), data);
        }
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert!(decode("no!").is_err());
    }

    #[test]
    fn a_run_gets_what_git_tracks_and_its_changes_come_back() {
        let tmp = TempDir::new();
        let (here, there) = (tmp.0.join("here"), tmp.0.join("there"));
        std::fs::create_dir_all(here.join("src")).unwrap();
        git(&here, &["init", "-q"]);
        std::fs::write(here.join(".gitignore"), ".env\n").unwrap();
        std::fs::write(here.join(".env"), "KEY=secret").unwrap();
        std::fs::write(here.join("src/a.rs"), "a").unwrap();
        std::fs::write(here.join("old.txt"), "old").unwrap();
        std::fs::write(here.join("untracked.txt"), "u").unwrap();
        git(&here, &["add", "src/a.rs", "old.txt", ".gitignore"]);

        let files = pack(&here).unwrap();
        let mut names: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            [".gitignore", "old.txt", "src/a.rs", "untracked.txt"]
        );

        // On the node: the files, some work, and what differs.
        std::fs::create_dir_all(&there).unwrap();
        let carried: Vec<File> =
            serde_json::from_str(&serde_json::to_string(&files).unwrap()).unwrap();
        unpack(&carried, &there).unwrap();
        std::fs::write(there.join("src/a.rs"), "a, changed").unwrap();
        std::fs::write(there.join("src/b.rs"), "new").unwrap();
        std::fs::remove_file(there.join("old.txt")).unwrap();
        let changes = changes(&carried, &there).unwrap();
        let carried: Vec<Change> =
            serde_json::from_str(&serde_json::to_string(&changes).unwrap()).unwrap();

        let touched = apply(&carried, &here).unwrap();
        assert_eq!(touched, ["src/a.rs", "src/b.rs", "old.txt"]);
        assert_eq!(
            std::fs::read_to_string(here.join("src/a.rs")).unwrap(),
            "a, changed"
        );
        assert_eq!(
            std::fs::read_to_string(here.join("src/b.rs")).unwrap(),
            "new"
        );
        assert!(!here.join("old.txt").exists());
        assert_eq!(
            std::fs::read_to_string(here.join(".env")).unwrap(),
            "KEY=secret"
        );
    }

    #[test]
    fn paths_that_leave_the_folder_are_refused_and_nothing_is_written() {
        let tmp = TempDir::new();
        for bad in ["../x", "/etc/passwd", "a/../../x", ".git/config", ""] {
            let changes = [
                Change {
                    path: "fine.txt".into(),
                    exec: false,
                    data: Some(b"x".to_vec()),
                },
                Change {
                    path: bad.into(),
                    exec: false,
                    data: Some(b"x".to_vec()),
                },
            ];
            assert!(apply(&changes, &tmp.0).is_err(), "{bad}");
            assert!(!tmp.0.join("fine.txt").exists(), "{bad}");
        }
        assert!(pack(&tmp.0).is_err(), "not a git repository");
    }
}
