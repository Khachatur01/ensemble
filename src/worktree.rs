//! A git worktree of its own for a task, so agents working at the same time
//! do not change each other's files. The task's work is committed on a branch
//! named after it; Ensemble never merges it.

use std::path::{Path, PathBuf};
use std::process::Command;

pub const WORKTREES_DIR: &str = "worktrees";
const BRANCH_PREFIX: &str = "ensemble/";

/// Where a task works, and the branch its work goes on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub dir: PathBuf,
    pub branch: String,
}

/// Run git in `dir` and return what it printed, or why it failed.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The worktree of task `id`, under `ensemble_dir` (the project's
/// `.ensemble/`). Made from the project's `HEAD` on the branch
/// `ensemble/<id>` the first time; found again after that, so a task that is
/// run again, or was interrupted, carries on where it was.
pub fn prepare(project: &Path, ensemble_dir: &Path, id: &str) -> Result<Worktree, String> {
    let root = ensemble_dir.join(WORKTREES_DIR);
    let dir = root.join(id);
    let branch = format!("{BRANCH_PREFIX}{id}");
    if dir.join(".git").exists() {
        return Ok(Worktree { dir, branch });
    }
    git(project, &["rev-parse", "--verify", "--quiet", "HEAD"]).map_err(|_| {
        "`isolation: worktree` needs the project to be a git repository with at least one commit"
            .to_string()
    })?;
    std::fs::create_dir_all(&root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    // Worktrees are checkouts, not project files: keep them out of git.
    let ignore = root.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, "*\n")
            .map_err(|e| format!("cannot write {}: {e}", ignore.display()))?;
    }
    let path = dir.to_string_lossy().to_string();
    let exists = git(
        project,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok();
    if exists {
        git(project, &["worktree", "add", &path, &branch])?;
    } else {
        git(project, &["worktree", "add", "-b", &branch, &path, "HEAD"])?;
    }
    Ok(Worktree { dir, branch })
}

/// Commit everything changed in the worktree on its branch, and return the
/// commit, or `None` when nothing changed.
pub fn commit(wt: &Worktree, message: &str) -> Result<Option<String>, String> {
    git(&wt.dir, &["add", "--all"])?;
    if git(&wt.dir, &["diff", "--cached", "--quiet"]).is_ok() {
        return Ok(None);
    }
    git(
        &wt.dir,
        &["commit", "--quiet", "--no-verify", "-m", message],
    )?;
    git(&wt.dir, &["rev-parse", "--short", "HEAD"]).map(Some)
}

#[cfg(test)]
pub(crate) mod testing {
    use std::path::Path;
    use std::process::Command;

    /// Make `dir` a git repository with one commit.
    pub fn repo(dir: &Path) {
        for args in [
            vec!["init", "--quiet", "--initial-branch=main"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            assert!(
                Command::new("git")
                    .args(&args)
                    .current_dir(dir)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(dir.join("README"), "hello\n").unwrap();
        for args in [
            vec!["add", "README"],
            vec!["commit", "--quiet", "-m", "start"],
        ] {
            assert!(
                Command::new("git")
                    .args(&args)
                    .current_dir(dir)
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::testing::TempDir;

    #[test]
    fn a_task_gets_a_worktree_on_its_own_branch_and_its_work_is_committed_there() {
        let tmp = TempDir::new();
        testing::repo(&tmp.0);
        let ens = tmp.0.join(".ensemble");
        let wt = prepare(&tmp.0, &ens, "0001-a").unwrap();
        assert_eq!(wt.branch, "ensemble/0001-a");
        assert!(wt.dir.join("README").is_file());

        assert_eq!(commit(&wt, "0001-a: nothing").unwrap(), None);
        std::fs::write(wt.dir.join("new.txt"), "x").unwrap();
        let sha = commit(&wt, "0001-a: add new.txt")
            .unwrap()
            .expect("a commit");
        let log = git(&tmp.0, &["log", "--format=%s", "ensemble/0001-a"]).unwrap();
        assert_eq!(log, "0001-a: add new.txt\nstart");
        assert!(git(&tmp.0, &["rev-parse", "--short", "ensemble/0001-a"]).unwrap() == sha);
        // The project is untouched, and its git sees no worktree files.
        assert!(!tmp.0.join("new.txt").exists());
        assert_eq!(git(&tmp.0, &["status", "--porcelain"]).unwrap(), "");

        // Found again as it was.
        assert_eq!(prepare(&tmp.0, &ens, "0001-a").unwrap(), wt);
        // And made again from its branch if the directory went away.
        git(
            &tmp.0,
            &["worktree", "remove", "--force", &wt.dir.to_string_lossy()],
        )
        .unwrap();
        let again = prepare(&tmp.0, &ens, "0001-a").unwrap();
        assert!(again.dir.join("new.txt").is_file());
    }

    #[test]
    fn a_project_without_a_commit_says_what_it_needs() {
        let tmp = TempDir::new();
        let err = prepare(&tmp.0, &tmp.0.join(".ensemble"), "0001-a").unwrap_err();
        assert!(
            err.contains("git repository with at least one commit"),
            "{err}"
        );
    }
}
