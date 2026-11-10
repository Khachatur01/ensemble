//! `hub.yaml`: the hub role's settings, kept apart from the helper team's
//! `config.yaml`, which a company may share. The hub is the people layer: here,
//! embedded in one person's node, it keeps that person's knowledge vault and
//! names the models that distill it.
//!
//! The same rules as `config.yaml`: no default values, unknown keys refused,
//! secrets named by the variable that holds them. A block that is absent means
//! the feature is not used: a hub without `knowledge` keeps no knowledge.

use super::raw::{RawDirectAdapter, RawLlmBackend};
use super::validate::{direct_adapter, explain, read_env_file, resolve_backend};
use super::*;
use serde::Deserialize;

pub const HUB_FILENAME: &str = "hub.yaml";
pub const HUB_SUPPORTED_VERSION: u32 = 1;

/// The JSON Schema for `hub.yaml`, for editors. The tests keep it in step with
/// the structs below.
pub const HUB_SCHEMA: &str = include_str!("../../../hub.schema.json");

/// The hub file `ensemble hub init` writes, with what each setting does.
pub const HUB_SAMPLE: &str = include_str!("hub_sample.yaml");

/// The hub's settings, resolved.
#[derive(Debug, Clone)]
pub struct Hub {
    pub version: u32,
    /// The person the hub works for, named in everything they share.
    pub owner: String,
    /// Model endpoints the hub itself uses.
    pub llm_backends: BTreeMap<String, LlmBackend>,
    /// Absent: the hub keeps no knowledge.
    pub knowledge: Option<Knowledge>,
    /// Where it was loaded from, for error messages.
    pub source: PathBuf,
}

/// The knowledge vault, and how shares become notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Knowledge {
    /// The vault's directory, made absolute against the hub's directory.
    pub vault: PathBuf,
    /// The models that turn shares into notes, and the most tokens of an answer.
    pub distill: DirectAdapter,
    pub recall: Recall,
}

/// How much one recall returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recall {
    pub max_notes: u32,
    /// All the notes together.
    pub max_chars: u32,
}

/// The hub file in directory `hub`. Like the helper team, the hub is named,
/// never found.
pub fn hub_path(hub: &Path) -> PathBuf {
    hub.join(HUB_FILENAME)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawHub {
    pub(crate) version: u32,
    pub(crate) owner: String,
    pub(crate) llm_backends: BTreeMap<String, RawLlmBackend>,
    pub(crate) knowledge: Option<RawKnowledge>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawKnowledge {
    pub(crate) vault: String,
    pub(crate) distill: RawDistill,
    pub(crate) recall: RawRecall,
}

/// A `direct` adapter without its `type`: the distilling model is one chat
/// request, with no tools.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawDistill {
    #[serde(rename = "try")]
    pub(crate) candidates: Vec<raw::RawDirectCandidate>,
    pub(crate) when_none: WhenNone,
    pub(crate) max_tokens: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRecall {
    pub(crate) max_notes: u32,
    pub(crate) max_chars: u32,
}

/// Load the hub file at `path`, with variables the environment does not set
/// taken from `env_file`.
pub fn load_hub(path: &Path, env_file: Option<&Path>, env: &dyn Env) -> Result<Hub, ConfigError> {
    if !path.exists() {
        return Err(ConfigError::HubNotFound {
            path: path.to_path_buf(),
        });
    }
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Unreadable {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;
    let file_vars = read_env_file(env_file)?;
    parse_hub(
        &text,
        path,
        &env::Layered {
            first: env,
            then: &file_vars,
        },
    )
}

pub fn parse_hub(text: &str, path: &Path, env: &dyn Env) -> Result<Hub, ConfigError> {
    let raw: RawHub = serde_norway::from_str(text).map_err(|e| ConfigError::Syntax {
        path: path.to_path_buf(),
        detail: explain(&e),
    })?;
    let mut problems = Vec::new();
    if raw.version != HUB_SUPPORTED_VERSION {
        problems.push(format!(
            "version: {} is not supported; this build understands hub version {HUB_SUPPORTED_VERSION}",
            raw.version
        ));
    }
    if raw.owner.trim().is_empty() {
        problems.push("owner: is empty; name the person this hub works for".into());
    }
    let mut backends = BTreeMap::new();
    for (name, rb) in &raw.llm_backends {
        match resolve_backend(name, rb, env) {
            Ok(b) => {
                backends.insert(name.clone(), b);
            }
            Err(mut errs) => problems.append(&mut errs),
        }
    }
    let knowledge = raw.knowledge.as_ref().map(|k| {
        if k.vault.trim().is_empty() {
            problems.push("knowledge.vault: is empty".into());
        }
        let base = path.parent().unwrap_or(Path::new("."));
        let distill = direct_adapter(
            "knowledge.distill",
            &RawDirectAdapter {
                candidates: k.distill.candidates.clone(),
                when_none: k.distill.when_none,
                max_tokens: k.distill.max_tokens,
            },
            &raw.llm_backends,
            &mut problems,
        );
        if k.recall.max_notes == 0 {
            problems.push("knowledge.recall: `max_notes` must be greater than 0".into());
        }
        if k.recall.max_chars == 0 {
            problems.push("knowledge.recall: `max_chars` must be greater than 0".into());
        }
        Knowledge {
            vault: base.join(k.vault.trim()),
            distill,
            recall: Recall {
                max_notes: k.recall.max_notes,
                max_chars: k.recall.max_chars,
            },
        }
    });
    for name in backends.keys() {
        let used = knowledge.as_ref().is_some_and(|k| {
            k.distill
                .fallback
                .candidates
                .iter()
                .any(|c| &c.backend == name)
        });
        if !used {
            eprintln!(
                "ensemble: note: LLM backend '{name}' is defined but nothing in the hub uses it"
            );
        }
    }
    if problems.is_empty() {
        Ok(Hub {
            version: raw.version,
            owner: raw.owner.trim().to_string(),
            llm_backends: backends,
            knowledge,
            source: path.to_path_buf(),
        })
    } else {
        Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            problems,
        })
    }
}
