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
    /// Absent: no task inbox.
    pub tasks: Option<Tasks>,
    /// Absent: the hub is embedded only, and nothing listens.
    pub server: Option<Server>,
    /// What each role may do, by the role's name. Empty without a server.
    pub roles: BTreeMap<String, Vec<Permission>>,
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

/// The task inbox: issues assigned to the person in a tracker, with the label,
/// become tasks for the helper team.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tasks {
    pub tracker: IssueTracker,
    /// Who the person is in the tracker.
    pub user: String,
    /// The signal: an issue assigned to `user` with this label is for the
    /// helper team; `<label>:<agent>` gives it to that agent.
    pub label: String,
    pub poll_seconds: u32,
    /// An issue's `project`, and the folder its work happens in, made absolute
    /// against the hub's directory.
    pub projects: BTreeMap<String, PathBuf>,
}

/// Where issues are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueTracker {
    /// A folder of `<id>.md` issues, made absolute against the hub's directory.
    LocalMarkdown { dir: PathBuf },
}

/// A hub server: where it listens for nodes, and how people join.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Server {
    pub listen: std::net::SocketAddr,
    pub registration: Registration,
    /// Hours a login lasts.
    pub session_hours: u32,
    /// Failed logins before a name is refused, and for how many minutes.
    pub lockout_failures: u32,
    pub lockout_minutes: u32,
}

/// How people join a hub server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registration {
    /// Anyone who reaches the hub may sign up; an admin approves and gives a
    /// role. At most `max_pending` sign-ups wait at once.
    Approval { max_pending: u32 },
    /// Only an admin adds users.
    Closed,
}

/// What a role may let its users do on a hub server. Ensemble defines them;
/// each hub names its own roles as sets of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Permission {
    /// Keep knowledge in the organization vault.
    Share,
    /// Read the organization vault's notes.
    Recall,
    /// Manage users: approve sign-ups, give roles, revoke tokens.
    Admin,
}

impl Permission {
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::Share => "share",
            Permission::Recall => "recall",
            Permission::Admin => "admin",
        }
    }
}

/// The issue tracker types that ship.
pub const LOCAL_MARKDOWN_TRACKER: &str = "local_markdown";

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
    pub(crate) tasks: Option<RawTasks>,
    pub(crate) server: Option<RawServer>,
    pub(crate) roles: Option<BTreeMap<String, Vec<Permission>>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawServer {
    pub(crate) listen: String,
    pub(crate) registration: RawRegistration,
    pub(crate) sessions: RawSessions,
    pub(crate) lockout: RawLockout,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawSessions {
    pub(crate) hours: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawLockout {
    pub(crate) failures: u32,
    pub(crate) minutes: u32,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum RawRegistration {
    #[serde(rename = "approval")]
    Approval(RawApproval),
    #[serde(rename = "closed")]
    Closed(RawClosed),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawApproval {
    pub(crate) max_pending: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawClosed {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawTasks {
    pub(crate) tracker: RawIssueTracker,
    pub(crate) user: String,
    pub(crate) label: String,
    pub(crate) poll_seconds: u32,
    pub(crate) projects: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum RawIssueTracker {
    #[serde(rename = "local_markdown")]
    LocalMarkdown(RawLocalMarkdownIssues),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawLocalMarkdownIssues {
    pub(crate) dir: String,
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
    let base = path.parent().unwrap_or(Path::new("."));
    let tasks = raw.tasks.as_ref().map(|t| {
        let tracker = match &t.tracker {
            RawIssueTracker::LocalMarkdown(m) => {
                if m.dir.trim().is_empty() {
                    problems.push("tasks.tracker.dir: is empty".into());
                }
                IssueTracker::LocalMarkdown {
                    dir: base.join(m.dir.trim()),
                }
            }
        };
        if t.user.trim().is_empty() {
            problems.push("tasks.user: is empty; name yourself as the tracker knows you".into());
        }
        let label = t.label.trim();
        if label.is_empty() || label.contains(char::is_whitespace) || label.contains(':') {
            problems.push(format!(
                "tasks.label: '{label}' must be one word with no ':', such as ensemble"
            ));
        }
        if t.poll_seconds == 0 {
            problems.push("tasks: `poll_seconds` must be greater than 0".into());
        }
        let mut projects = BTreeMap::new();
        for (name, dir) in &t.projects {
            if name.trim().is_empty() || dir.trim().is_empty() {
                problems.push(format!(
                    "tasks.projects: '{name}' needs a name and a folder"
                ));
            }
            projects.insert(name.trim().to_string(), base.join(dir.trim()));
        }
        Tasks {
            tracker,
            user: t.user.trim().to_string(),
            label: label.to_string(),
            poll_seconds: t.poll_seconds,
            projects,
        }
    });
    let server = raw.server.as_ref().and_then(|s| {
        let registration = match &s.registration {
            RawRegistration::Approval(a) => {
                if a.max_pending == 0 {
                    problems
                        .push("server.registration: `max_pending` must be greater than 0".into());
                }
                Registration::Approval {
                    max_pending: a.max_pending,
                }
            }
            RawRegistration::Closed(_) => Registration::Closed,
        };
        if s.sessions.hours == 0 {
            problems.push("server.sessions: `hours` must be greater than 0".into());
        }
        if s.lockout.failures == 0 || s.lockout.minutes == 0 {
            problems.push("server.lockout: `failures` and `minutes` must be greater than 0".into());
        }
        match s.listen.trim().parse() {
            Ok(listen) => Some(Server {
                listen,
                registration,
                session_hours: s.sessions.hours,
                lockout_failures: s.lockout.failures,
                lockout_minutes: s.lockout.minutes,
            }),
            Err(_) => {
                problems.push(format!(
                    "server.listen: '{}' must be an address and a port, such as 127.0.0.1:7700",
                    s.listen
                ));
                None
            }
        }
    });
    let roles = raw.roles.clone().unwrap_or_default();
    match (&raw.server, &raw.roles) {
        (Some(_), None) => problems.push(
            "roles: a hub with a `server` needs `roles`, saying what each role may do".into(),
        ),
        (None, Some(_)) => problems
            .push("roles: only a hub with a `server` has roles; add one or remove them".into()),
        (Some(_), Some(r)) if r.is_empty() => {
            problems.push("roles: name at least one role, such as member: [share, recall]".into())
        }
        _ => {}
    }
    for name in roles.keys() {
        if name.trim().is_empty() || name.contains(char::is_whitespace) {
            problems.push(format!("roles: the role name '{name}' must be one word"));
        }
    }
    if raw.server.is_some() && raw.knowledge.is_none() {
        problems.push(
            "server: a hub server keeps the organization's knowledge, so it needs `knowledge`"
                .into(),
        );
    }
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
            tasks,
            server,
            roles,
            source: path.to_path_buf(),
        })
    } else {
        Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            problems,
        })
    }
}
