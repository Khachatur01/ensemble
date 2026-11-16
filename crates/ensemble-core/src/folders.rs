//! The folders a node has worked in, by project: a project is a folder's
//! name, and the folder kept for it is the one a session last ran in. A task
//! from a hub names its project; this is where it goes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const FILE: &str = "folders.json";

/// The folders a node has worked in, kept in its state directory.
#[derive(Debug, Clone)]
pub struct Folders {
    dir: PathBuf,
}

impl Folders {
    /// The folders kept in `dir`.
    pub fn new(dir: &Path) -> Self {
        Folders {
            dir: dir.to_path_buf(),
        }
    }

    /// The project a folder is: its name.
    pub fn project_of(folder: &Path) -> Option<String> {
        folder
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
    }

    /// Every project and its folder.
    pub fn all(&self) -> BTreeMap<String, PathBuf> {
        std::fs::read_to_string(self.dir.join(FILE))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// The folder of `project`, when a session ran in one.
    pub fn of(&self, project: &str) -> Option<PathBuf> {
        self.all().remove(project)
    }

    /// Keep `folder` as its project's folder, in place of any other.
    pub fn remember(&self, folder: &Path) -> Result<(), String> {
        let Some(project) = Self::project_of(folder) else {
            return Ok(());
        };
        let mut all = self.all();
        if all.get(&project).is_some_and(|f| f == folder) {
            return Ok(());
        }
        all.insert(project, folder.to_path_buf());
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| format!("cannot create {}: {e}", self.dir.display()))?;
        let path = self.dir.join(FILE);
        let tmp = path.with_extension("tmp");
        let json = serde_json::to_string_pretty(&all).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, json)
            .and_then(|()| std::fs::rename(&tmp, &path))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::testing::TempDir;

    #[test]
    fn a_project_is_the_folder_of_its_name_where_a_session_last_ran() {
        let tmp = TempDir::new();
        let folders = Folders::new(&tmp.0);
        assert_eq!(folders.of("shop"), None);
        folders.remember(Path::new("/code/shop")).unwrap();
        folders.remember(Path::new("/code/garden")).unwrap();
        assert_eq!(folders.of("shop"), Some(PathBuf::from("/code/shop")));
        folders.remember(Path::new("/work/shop")).unwrap();
        assert_eq!(folders.of("shop"), Some(PathBuf::from("/work/shop")));
        assert_eq!(folders.all().len(), 2);
    }
}
