//! Path confinement for the agent's file tools.
//!
//! The project directory is fixed when the server starts. A model only ever
//! names paths *inside* it; this module is what makes that true. Every path is
//! resolved through the filesystem (symlinks followed) and must land under the
//! canonical root, so neither `..` nor a symlink planted in the tree can lead
//! out of it.

use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    pub fn new(workdir: &Path) -> Result<Self, String> {
        let root = workdir
            .canonicalize()
            .map_err(|e| format!("working directory {}: {e}", workdir.display()))?;
        if !root.is_dir() {
            return Err(format!("{} is not a directory", root.display()));
        }
        Ok(Sandbox { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A path to something that must already exist.
    pub fn existing(&self, path: &str) -> Result<PathBuf, String> {
        let joined = self.join(path)?;
        let real = joined
            .canonicalize()
            .map_err(|_| format!("no such file or directory: {path}"))?;
        self.inside(real, path)
    }

    /// A path that may not exist yet (a file about to be written). Its nearest
    /// existing ancestor must be inside the root, and it may not be a dangling
    /// symlink, which a write would follow out of the tree.
    pub fn new_or_existing(&self, path: &str) -> Result<PathBuf, String> {
        let joined = self.join(path)?;
        if joined.exists() {
            return self.existing(path);
        }
        if joined.symlink_metadata().is_ok() {
            return Err(format!("{path} is a broken symlink"));
        }
        let mut ancestor = joined.as_path();
        let mut tail = Vec::new();
        while !ancestor.exists() {
            let name = ancestor
                .file_name()
                .ok_or_else(|| format!("invalid path: {path}"))?;
            tail.push(name.to_owned());
            ancestor = ancestor
                .parent()
                .ok_or_else(|| format!("invalid path: {path}"))?;
        }
        let mut real = self.inside(
            ancestor
                .canonicalize()
                .map_err(|e| format!("{path}: {e}"))?,
            path,
        )?;
        for name in tail.into_iter().rev() {
            real.push(name);
        }
        Ok(real)
    }

    /// `path` as the model should see it: relative to the root.
    pub fn show(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    fn join(&self, path: &str) -> Result<PathBuf, String> {
        if path.trim().is_empty() {
            return Err("path is empty".into());
        }
        let p = Path::new(path);
        if p.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(format!(
                "{path}: '..' is not allowed; paths stay inside the project"
            ));
        }
        Ok(if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        })
    }

    fn inside(&self, real: PathBuf, asked: &str) -> Result<PathBuf, String> {
        if real.starts_with(&self.root) {
            Ok(real)
        } else {
            Err(format!("{asked}: outside the project directory"))
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
}

#[cfg(test)]
mod tests {
    use super::testing::TempDir;
    use super::*;

    fn sandbox() -> (TempDir, Sandbox) {
        let tmp = TempDir::new();
        std::fs::create_dir(tmp.0.join("project")).unwrap();
        let sb = Sandbox::new(&tmp.0.join("project")).unwrap();
        (tmp, sb)
    }

    #[test]
    fn a_relative_path_inside_resolves() {
        let (tmp, sb) = sandbox();
        std::fs::write(tmp.0.join("project/a.txt"), "x").unwrap();
        let p = sb.existing("a.txt").unwrap();
        assert_eq!(sb.show(&p), "a.txt");
    }

    #[test]
    fn dot_dot_is_refused_even_when_it_would_stay_inside() {
        let (_tmp, sb) = sandbox();
        assert!(sb.existing("../project").unwrap_err().contains("'..'"));
        assert!(sb.new_or_existing("a/../b").unwrap_err().contains("'..'"));
    }

    #[test]
    fn an_absolute_path_outside_is_refused() {
        let (tmp, sb) = sandbox();
        std::fs::write(tmp.0.join("secret"), "x").unwrap();
        let abs = tmp.0.join("secret");
        let err = sb.existing(abs.to_str().unwrap()).unwrap_err();
        assert!(err.contains("outside"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_tree_is_refused() {
        let (tmp, sb) = sandbox();
        std::fs::write(tmp.0.join("secret"), "x").unwrap();
        std::os::unix::fs::symlink(tmp.0.join("secret"), tmp.0.join("project/link")).unwrap();
        std::os::unix::fs::symlink(tmp.0.join("nowhere"), tmp.0.join("project/dangling")).unwrap();
        assert!(sb.existing("link").unwrap_err().contains("outside"));
        assert!(sb.new_or_existing("link").unwrap_err().contains("outside"));
        assert!(
            sb.new_or_existing("dangling")
                .unwrap_err()
                .contains("broken symlink")
        );
    }

    #[test]
    fn a_new_file_in_a_new_subdirectory_resolves_under_the_root() {
        let (_tmp, sb) = sandbox();
        let p = sb.new_or_existing("src/new/mod.rs").unwrap();
        assert_eq!(sb.show(&p), "src/new/mod.rs");
    }
}
