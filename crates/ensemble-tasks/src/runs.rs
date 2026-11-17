//! What a node keeps for the runs it does in one working folder, under the
//! folder's state directory: each run's activity log in `runs/<id>/log.txt`,
//! the worktrees of tasks that run in one, and how much each agent with a
//! budget ran lately. The tasks themselves are kept on the hub
//! ([`crate::board`]); this is only the working side of the ones this node
//! runs, which `runs/` keeps out of git should the state directory be put in
//! a repository.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const RUNS_DIR: &str = "runs";
const LOG_FILE: &str = "log.txt";
/// When each agent with a budget started its recent runs, across sessions.
const USAGE_FILE: &str = "usage.json";

/// The runs of the work done in one directory: `runs/` under its state
/// directory.
#[derive(Debug, Clone)]
pub struct Runs {
    root: PathBuf,
    runs: PathBuf,
    /// Where usage counters are kept when not in `runs/`: the directory of
    /// the helper teams, shared by every project that uses them.
    usage: Option<PathBuf>,
}

impl Runs {
    /// The runs in `dir`, a state directory. Nothing is created until
    /// something is written.
    pub fn new(dir: &Path) -> Self {
        Runs {
            root: dir.to_path_buf(),
            runs: dir.join(RUNS_DIR),
            usage: None,
        }
    }

    /// Keep usage counters in `dir` instead of beside the runs.
    pub fn with_usage_dir(mut self, dir: PathBuf) -> Self {
        self.usage = Some(dir);
        self
    }

    fn usage_path(&self) -> PathBuf {
        self.usage.as_ref().unwrap_or(&self.runs).join(USAGE_FILE)
    }

    /// The state directory, where worktrees are made.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `runs/`, made on first use, with the `.gitignore` that keeps every
    /// run out of git.
    fn runs_dir(&self) -> io::Result<&Path> {
        fs::create_dir_all(&self.runs)?;
        let ignore = self.runs.join(".gitignore");
        if !ignore.exists() {
            fs::write(ignore, "*\n")?;
        }
        Ok(&self.runs)
    }

    /// Add a line to a run's activity log.
    pub fn log(&self, id: &str, line: &str) {
        let append = || -> io::Result<()> {
            let dir = self.runs_dir()?.join(id);
            fs::create_dir_all(&dir)?;
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(LOG_FILE))?;
            writeln!(f, "{line}")
        };
        if let Err(e) = append() {
            eprintln!("ensemble: cannot write the log of {id}: {e}");
        }
    }

    /// A run's activity log, when this node ran it.
    pub fn log_of(&self, id: &str) -> Option<String> {
        if !crate::tasks::valid_id(id) {
            return None;
        }
        fs::read_to_string(self.runs.join(id).join(LOG_FILE)).ok()
    }

    /// When each agent started its recent runs, in seconds since the epoch.
    pub fn usage(&self) -> BTreeMap<String, Vec<u64>> {
        fs::read_to_string(self.usage_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save_usage(&self, usage: &BTreeMap<String, Vec<u64>>) {
        let write = || -> io::Result<()> {
            let text = serde_json::to_string(usage).expect("usage serializes");
            match &self.usage {
                Some(dir) => fs::create_dir_all(dir)?,
                None => {
                    self.runs_dir()?;
                }
            }
            let path = self.usage_path();
            let tmp = path.with_extension("tmp");
            fs::write(&tmp, text)?;
            fs::rename(&tmp, &path)
        };
        if let Err(e) = write() {
            eprintln!("ensemble: cannot record usage: {e}");
        }
    }

    /// The highest `adhoc-N` a run directory has, so a new session does not
    /// reuse one.
    pub fn last_adhoc(&self) -> u64 {
        let Ok(entries) = fs::read_dir(&self.runs) else {
            return 0;
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|n| n.strip_prefix("adhoc-")?.parse().ok())
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_core::workspace::testing::TempDir;

    #[test]
    fn runs_keep_a_log_and_ignore_themselves() {
        let tmp = TempDir::new();
        let runs = Runs::new(&tmp.0);
        runs.log("adhoc-3", "scout · step 1");
        runs.log("adhoc-3", "scout · step 2");
        assert_eq!(
            runs.log_of("adhoc-3").unwrap(),
            "scout · step 1\nscout · step 2\n"
        );
        assert_eq!(
            fs::read_to_string(tmp.0.join("runs/.gitignore")).unwrap(),
            "*\n"
        );
        assert_eq!(runs.last_adhoc(), 3);
        assert_eq!(runs.log_of("../x"), None);
    }

    #[test]
    fn usage_is_kept_where_it_is_shared() {
        let tmp = TempDir::new();
        let runs = Runs::new(&tmp.0.join("a")).with_usage_dir(tmp.0.join("team"));
        let usage = BTreeMap::from([("coder".to_string(), vec![1, 2])]);
        runs.save_usage(&usage);
        assert_eq!(
            Runs::new(&tmp.0.join("b"))
                .with_usage_dir(tmp.0.join("team"))
                .usage(),
            usage
        );
    }
}
