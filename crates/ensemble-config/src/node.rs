//! `node.yaml`: what one person's computer says about itself to a hub server,
//! in `~/.config/ensemble/node.yaml`. For now, which folder each project's
//! work happens in, so that the tasks the hub sends land in the right place.
//! Without the file this computer takes no tasks from a hub.

use super::validate::explain;
use super::*;
use serde::Deserialize;

pub const NODE_FILENAME: &str = "node.yaml";
pub const NODE_SUPPORTED_VERSION: u32 = 1;

/// The JSON Schema for `node.yaml`.
pub const NODE_SCHEMA: &str = include_str!("../../../node.schema.json");

/// The file `ensemble node init` writes.
pub const NODE_SAMPLE: &str = include_str!("node_sample.yaml");

/// This computer, as a hub's tasks need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub version: u32,
    /// An issue's `project`, and the folder on this computer its work happens
    /// in.
    pub projects: BTreeMap<String, PathBuf>,
    pub source: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawNode {
    pub(crate) version: u32,
    pub(crate) projects: BTreeMap<String, String>,
}

/// The node file in `dir`, usually `~/.config/ensemble`.
pub fn node_path(dir: &Path) -> PathBuf {
    dir.join(NODE_FILENAME)
}

/// The node file at `path`, or none when there is no such file.
pub fn load_node(path: &Path) -> Result<Option<Node>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(ConfigError::Unreadable {
                path: path.to_path_buf(),
                detail: e.to_string(),
            });
        }
    };
    parse_node(&text, path).map(Some)
}

pub fn parse_node(text: &str, path: &Path) -> Result<Node, ConfigError> {
    let raw: RawNode = serde_norway::from_str(text).map_err(|e| ConfigError::Syntax {
        path: path.to_path_buf(),
        detail: explain(&e),
    })?;
    let mut problems = Vec::new();
    if raw.version != NODE_SUPPORTED_VERSION {
        problems.push(format!(
            "version: {} is not supported; this build understands node version {NODE_SUPPORTED_VERSION}",
            raw.version
        ));
    }
    let mut projects = BTreeMap::new();
    for (name, dir) in &raw.projects {
        let folder = PathBuf::from(dir.trim());
        if name.trim().is_empty() || !folder.is_absolute() {
            problems.push(format!(
                "projects.{name}: needs a name and an absolute folder, such as /home/ana/code/shop"
            ));
        }
        projects.insert(name.trim().to_string(), folder);
    }
    if problems.is_empty() {
        Ok(Node {
            version: raw.version,
            projects,
            source: path.to_path_buf(),
        })
    } else {
        Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            problems,
        })
    }
}
