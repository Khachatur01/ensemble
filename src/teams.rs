//! The helper teams of this computer's node, kept in one directory
//! (`~/.local/share/ensemble/teams/`), as the node and the commands use them:
//! every team that is ready, combined into one helper team whose agents are
//! named `<team>/<agent>`; and putting teams there, as a person makes, links
//! or pulls them, or as an agent writes one for the person to approve.
//!
//! A pulled team is a git repository pinned to a commit, like an image pinned
//! to a digest: the clone lives in the cache (`remote`), is checked every time
//! it is used, and runs only once the person has trusted that commit.

use crate::config::teams::{self, Origin, TeamEntry};
use crate::config::{self, Config, Env};
use crate::remote;
use crate::state;
use std::path::{Path, PathBuf};

/// The directory of the node's teams.
pub fn dir() -> Result<PathBuf, String> {
    state::teams_dir()
}

/// Every team in the directory, by name.
pub fn list() -> Result<Vec<TeamEntry>, String> {
    Ok(teams::list(&dir()?))
}

/// The folder with the `config.yaml` of `team`: its own, the folder it links
/// to, or the checked and trusted clone of a pulled one.
pub fn config_dir(team: &TeamEntry) -> Result<PathBuf, String> {
    match &team.origin {
        Origin::Pulled { url, commit } => Ok(remote::use_source(&remote::Source::Remote {
            url: url.clone(),
            sha: Some(commit.clone()),
            git_ref: None,
        })?
        .dir),
        _ => Ok(team.config_dir().expect("a team made or linked here")),
    }
}

/// Every team that is ready, as one helper team, and a line for each that is
/// not: waiting for approval. A team that cannot be loaded is an error that
/// names it, so that a broken team is seen, not skipped.
pub fn load_all(env_file: Option<&Path>, env: &dyn Env) -> Result<(Config, Vec<String>), String> {
    let dir = dir()?;
    let mut ready = Vec::new();
    let mut waiting = Vec::new();
    for team in teams::list(&dir) {
        if team.pending {
            waiting.push(format!(
                "team '{}' waits for your approval: ensemble team approve {}",
                team.name, team.name
            ));
            continue;
        }
        let config = config_dir(&team)
            .and_then(|d| {
                config::load(&config::config_path(&d), env_file, env).map_err(|e| e.to_string())
            })
            .map_err(|e| format!("team '{}': {e}", team.name))?;
        ready.push((team.name, config));
    }
    Ok((teams::combine(ready, dir), waiting))
}

/// The folder of `name` whose `config.yaml` a command may change: a team made
/// here or linked. A pulled one is read-only.
pub fn editable(name: &str) -> Result<PathBuf, String> {
    let team = teams::entry(&dir()?, name).ok_or_else(|| missing(name))?;
    match &team.origin {
        Origin::Pulled { url, .. } => Err(format!(
            "team '{name}' is pulled from {url}, so it is read-only: clone it, change your copy, and `ensemble team add` that folder"
        )),
        _ => Ok(team.config_dir().expect("a team made or linked here")),
    }
}

/// What to say when there is no team `name`.
pub fn missing(name: &str) -> String {
    format!("no team '{name}'. `ensemble teams` lists them; `ensemble team new {name}` makes one.")
}

/// Make the team `name` here with `text` as its `config.yaml`.
pub fn make(name: &str, text: &str) -> Result<PathBuf, String> {
    let dir = dir()?;
    teams::check_new(&dir, name)?;
    let path = dir.join(name);
    std::fs::create_dir_all(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    let file = config::config_path(&path);
    std::fs::write(&file, text).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    Ok(path)
}

/// Link the folder `target`, which holds a `config.yaml`, as the team `name`.
pub fn link(name: &str, target: &Path) -> Result<PathBuf, String> {
    let dir = dir()?;
    teams::check_new(&dir, name)?;
    if !config::config_path(target).is_file() {
        return Err(format!(
            "{} has no {}: a team's folder holds its config.yaml",
            target.display(),
            config::CONFIG_FILENAME
        ));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(name);
    std::os::unix::fs::symlink(target, &path)
        .map_err(|e| format!("cannot link {}: {e}", path.display()))?;
    Ok(path)
}

/// Record that the team `name` is the repository `url` at `commit`, which
/// the caller has fetched and trusted.
pub fn pin(name: &str, url: &str, commit: &str, new: bool) -> Result<PathBuf, String> {
    let dir = dir()?;
    if new {
        teams::check_new(&dir, name)?;
    }
    let path = dir.join(name);
    std::fs::create_dir_all(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    let pulled = serde_json::json!({ "url": url, "commit": commit });
    let file = path.join(teams::PULLED_FILE);
    std::fs::write(&file, format!("{pulled:#}\n"))
        .map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    Ok(path)
}

/// Let the team `name`, which an agent wrote, run.
pub fn approve(name: &str) -> Result<(), String> {
    let team = teams::entry(&dir()?, name).ok_or_else(|| missing(name))?;
    let marker = team.path.join(teams::PENDING_FILE);
    std::fs::remove_file(&marker).map_err(|e| format!("cannot remove {}: {e}", marker.display()))
}

/// Take the team `name` out of the directory. A linked team's folder, and a
/// pulled team's clone in the cache, are left where they are.
pub fn remove(name: &str) -> Result<(), String> {
    let team = teams::entry(&dir()?, name).ok_or_else(|| missing(name))?;
    match team.origin {
        Origin::Linked(_) => std::fs::remove_file(&team.path),
        _ => std::fs::remove_dir_all(&team.path),
    }
    .map_err(|e| format!("cannot remove {}: {e}", team.path.display()))
}

/// The name a pulled repository gets when none is given: its last part, as
/// `git clone` names its folder.
pub fn name_of(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(url)
        .trim_end_matches(".git")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pulled_team_is_named_for_its_repository() {
        assert_eq!(name_of("github.com/acme/web-team"), "web-team");
        assert_eq!(name_of("git@github.com:acme/pcb.git"), "pcb");
        assert_eq!(name_of("https://example.com/teams/docs/"), "docs");
    }
}
