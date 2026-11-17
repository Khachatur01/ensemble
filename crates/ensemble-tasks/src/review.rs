//! Looking at, and taking in, the work of a task that ran in a git worktree.
//!
//! Ensemble never merges on its own: a task's work stays on its branch,
//! `ensemble/<id>`, until a person decides. `review` and `diff` only look.
//! `merge` is for a person to type; the calling agent gets no tool for it.

use crate::tasks::{Status, Store};
use ensemble_core::workspace;
use std::path::{Path, PathBuf};
use std::process::Command;

const BRANCH_PREFIX: &str = "ensemble/";

/// Run git in `dir`: what it printed, or why it failed.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        Err(format!(
            "git {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim(),
            String::from_utf8_lossy(&out.stdout).trim()
        ))
    }
}

/// A task whose branch can be looked at.
#[derive(Debug)]
pub struct Task {
    pub id: String,
    pub agent: String,
    /// The repository the task worked in.
    pub repo: PathBuf,
    pub branch: String,
    /// The task's own `result.md`, once it has ended.
    pub result: Option<String>,
    pub status: Option<Status>,
}

/// Find task `id` and its branch in the repository it worked in under
/// `workdir`.
pub fn find(store: &Store, workdir: &Path, id: &str) -> Result<Task, String> {
    if !store.exists(id) {
        return Err(format!("no task '{id}'. `ensemble tasks` lists them."));
    }
    let ticket = store.read(id)?;
    let repo = match ticket.repo.as_deref() {
        Some(repo) => workspace::resolve_repo(workdir, repo)?,
        None => workdir.to_path_buf(),
    };
    let branch = format!("{BRANCH_PREFIX}{id}");
    git(
        &repo,
        &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")],
    )
    .map_err(|_| {
        format!(
            "task {id} has no branch '{branch}' in {}: only tasks of an agent with `isolation: worktree` keep their work on one",
            repo.display()
        )
    })?;
    let outcome = store.outcome(id);
    Ok(Task {
        id: id.to_string(),
        agent: ticket.agent,
        repo,
        branch,
        status: outcome.as_ref().map(|o| o.status),
        result: outcome.map(|o| o.text),
    })
}

/// What the branch holds that the repository's `HEAD` does not, as a summary.
pub fn stat(task: &Task) -> Result<String, String> {
    git(
        &task.repo,
        &[
            "diff",
            "--stat",
            "--no-color",
            &format!("HEAD...{}", task.branch),
        ],
    )
}

/// The same, whole.
pub fn diff(task: &Task) -> Result<String, String> {
    git(
        &task.repo,
        &["diff", "--no-color", &format!("HEAD...{}", task.branch)],
    )
}

/// A task's branch and result, for a person or the calling agent to read. It changes
/// nothing.
pub fn review(task: &Task) -> Result<String, String> {
    let changes = stat(task)?;
    let mut out = format!(
        "task {} · {} · {}\nbranch {} in {}\n\n{}\n",
        task.id,
        task.agent,
        task.status.map_or("not finished", |s| s.as_str()),
        task.branch,
        task.repo.display(),
        if changes.is_empty() {
            "(no changes beyond the repository's HEAD)".to_string()
        } else {
            changes
        }
    );
    if let Some(result) = &task.result {
        out.push_str(&format!("\nResult:\n{}\n", result.trim()));
    }
    Ok(out)
}

/// How to take the work in.
#[derive(Debug, Clone, Copy, Default)]
pub struct MergeOptions {
    /// One commit instead of the branch's own.
    pub squash: bool,
    /// Afterwards remove the task's worktree and branch.
    pub cleanup: bool,
}

/// Merge the task's branch into what the repository has checked out. Refuses
/// a task that is not done, a repository with uncommitted changes, and a
/// merge that conflicts, which it leaves undone, naming the files.
pub fn merge(task: &Task, worktree_dir: &Path, options: MergeOptions) -> Result<String, String> {
    if task.status != Some(Status::Done) {
        return Err(format!(
            "task {} is {}, not done; only finished work is merged",
            task.id,
            task.status.map_or("not finished", |s| s.as_str())
        ));
    }
    let dirty = git(
        &task.repo,
        &["status", "--porcelain", "--untracked-files=no"],
    )?;
    if !dirty.is_empty() {
        return Err(format!(
            "{} has uncommitted changes; commit or stash them first:\n{dirty}",
            task.repo.display()
        ));
    }
    let message = format!("Merge {} ({})", task.branch, task.agent);
    let attempt = if options.squash {
        git(&task.repo, &["merge", "--squash", &task.branch])
    } else {
        git(
            &task.repo,
            &["merge", "--no-ff", "-m", &message, &task.branch],
        )
    };
    if let Err(e) = attempt {
        let conflicts =
            git(&task.repo, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
        // Leave the repository as it was.
        if options.squash {
            let _ = git(&task.repo, &["reset", "--hard", "--quiet", "HEAD"]);
        } else {
            let _ = git(&task.repo, &["merge", "--abort"]);
        }
        return Err(if conflicts.is_empty() {
            format!("the merge failed and was undone: {e}")
        } else {
            format!(
                "the merge conflicts in these files, and was undone:\n{conflicts}\nMerge the branch {} by hand, or have the agent rebase it.",
                task.branch
            )
        });
    }
    let mut said = if options.squash {
        git(&task.repo, &["commit", "-m", &message])
            .map(|_| format!("squashed {} into one commit", task.branch))?
    } else {
        format!("merged {}", task.branch)
    };
    if options.cleanup {
        let dir = worktree_dir.to_string_lossy().to_string();
        // The worktree holds the branch, so it goes first.
        let _ = git(&task.repo, &["worktree", "remove", "--force", &dir]);
        // A squash leaves the branch looking unmerged to git, so it is forced.
        let flag = if options.squash { "-D" } else { "-d" };
        match git(&task.repo, &["branch", flag, &task.branch]) {
            Ok(_) => said.push_str(&format!(
                "; removed its worktree and branch {}",
                task.branch
            )),
            Err(e) => said.push_str(&format!("; could not remove the branch: {e}")),
        }
    }
    Ok(said)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::Ticket;
    use ensemble_core::workspace::testing::TempDir;

    fn run(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "user.name=T", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repository with a finished task whose branch changed `a.txt`.
    struct Fixture {
        _tmp: TempDir,
        repo: PathBuf,
        store: Store,
        wt: PathBuf,
    }

    fn fixture(status: Status, change: &str) -> Fixture {
        let tmp = TempDir::new();
        let repo = tmp.0.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "--quiet", "-b", "main"]);
        // `merge` commits as whoever the repository says; a sandbox has no one.
        run(&repo, &["config", "user.name", "T"]);
        run(&repo, &["config", "user.email", "t@t"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "--quiet", "-m", "start"]);

        let state = tmp.0.join("state");
        let store = Store::new(&state);
        let id = store
            .create(&Ticket {
                title: "Edit".into(),
                agent: "coder".into(),
                text: "edit".into(),
                ..Ticket::default()
            })
            .unwrap();
        assert_eq!(id, "0001-edit");
        let wt = state.join("worktrees").join(&id);
        run(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                &format!("ensemble/{id}"),
                wt.to_str().unwrap(),
                "HEAD",
            ],
        );
        std::fs::write(wt.join("a.txt"), change).unwrap();
        run(&wt, &["add", "--all"]);
        run(&wt, &["commit", "--quiet", "-m", "work"]);
        store
            .write_result(
                &id,
                "coder",
                status,
                std::time::SystemTime::now(),
                &[],
                "did it",
            )
            .unwrap();
        Fixture {
            _tmp: tmp,
            repo,
            store,
            wt,
        }
    }

    #[test]
    fn review_shows_the_branch_the_change_and_the_result_and_changes_nothing() {
        let f = fixture(Status::Done, "two\n");
        let task = find(&f.store, &f.repo, "0001-edit").unwrap();
        let text = review(&task).unwrap();
        assert!(text.contains("task 0001-edit · coder · done"), "{text}");
        assert!(text.contains("branch ensemble/0001-edit"), "{text}");
        assert!(text.contains("a.txt | 2"), "{text}");
        assert!(
            text.contains("Result:") && text.contains("did it"),
            "{text}"
        );
        assert!(diff(&task).unwrap().contains("-one\n+two"));
        assert_eq!(
            std::fs::read_to_string(f.repo.join("a.txt")).unwrap(),
            "one\n"
        );
        assert!(
            find(&f.store, &f.repo, "0009")
                .unwrap_err()
                .contains("no task")
        );
    }

    #[test]
    fn a_task_without_a_branch_says_why() {
        let f = fixture(Status::Done, "two\n");
        let id = f
            .store
            .create(&Ticket {
                title: "Plain".into(),
                agent: "coder".into(),
                text: "t".into(),
                ..Ticket::default()
            })
            .unwrap();
        let err = find(&f.store, &f.repo, &id).err().unwrap();
        assert!(err.contains("isolation: worktree"), "{err}");
    }

    #[test]
    fn merging_takes_the_work_in_and_keeps_the_branch_unless_asked() {
        let f = fixture(Status::Done, "two\n");
        let task = find(&f.store, &f.repo, "0001-edit").unwrap();
        let said = merge(&task, &f.wt, MergeOptions::default()).unwrap();
        assert!(said.contains("merged ensemble/0001-edit"), "{said}");
        assert_eq!(
            std::fs::read_to_string(f.repo.join("a.txt")).unwrap(),
            "two\n"
        );
        assert!(f.wt.exists(), "nothing is deleted without --cleanup");
        assert!(
            run(&f.repo, &["branch", "--list", "ensemble/0001-edit"])
                .contains("ensemble/0001-edit")
        );
    }

    #[test]
    fn a_squash_with_cleanup_leaves_one_commit_and_no_worktree_or_branch() {
        let f = fixture(Status::Done, "two\n");
        let task = find(&f.store, &f.repo, "0001-edit").unwrap();
        let before = run(&f.repo, &["rev-list", "--count", "HEAD"]);
        let said = merge(
            &task,
            &f.wt,
            MergeOptions {
                squash: true,
                cleanup: true,
            },
        )
        .unwrap();
        assert!(
            said.contains("squashed") && said.contains("removed its worktree and branch"),
            "{said}"
        );
        assert_eq!(
            run(&f.repo, &["rev-list", "--count", "HEAD"]),
            (before.parse::<u32>().unwrap() + 1).to_string()
        );
        assert_eq!(
            std::fs::read_to_string(f.repo.join("a.txt")).unwrap(),
            "two\n"
        );
        assert!(!f.wt.exists());
        assert!(run(&f.repo, &["branch", "--list", "ensemble/0001-edit"]).is_empty());
    }

    #[test]
    fn a_task_that_is_not_done_a_dirty_tree_and_a_conflict_are_refused_and_nothing_changes() {
        let f = fixture(Status::Failed, "two\n");
        let task = find(&f.store, &f.repo, "0001-edit").unwrap();
        assert!(
            merge(&task, &f.wt, MergeOptions::default())
                .unwrap_err()
                .contains("not done")
        );

        let f = fixture(Status::Done, "two\n");
        let task = find(&f.store, &f.repo, "0001-edit").unwrap();
        std::fs::write(f.repo.join("a.txt"), "dirty\n").unwrap();
        let err = merge(&task, &f.wt, MergeOptions::default()).unwrap_err();
        assert!(err.contains("uncommitted changes"), "{err}");
        assert_eq!(
            std::fs::read_to_string(f.repo.join("a.txt")).unwrap(),
            "dirty\n"
        );

        // The repository moves on the same line the branch changed.
        let f = fixture(Status::Done, "two\n");
        let task = find(&f.store, &f.repo, "0001-edit").unwrap();
        std::fs::write(f.repo.join("a.txt"), "other\n").unwrap();
        run(&f.repo, &["commit", "--quiet", "-am", "other"]);
        for squash in [false, true] {
            let err = merge(
                &task,
                &f.wt,
                MergeOptions {
                    squash,
                    cleanup: true,
                },
            )
            .unwrap_err();
            assert!(
                err.contains("conflicts") && err.contains("a.txt") && err.contains("undone"),
                "{err}"
            );
            assert_eq!(
                std::fs::read_to_string(f.repo.join("a.txt")).unwrap(),
                "other\n"
            );
            assert!(run(&f.repo, &["status", "--porcelain", "--untracked-files=no"]).is_empty());
            assert!(f.wt.exists(), "a refused merge removes nothing");
        }
    }
}
