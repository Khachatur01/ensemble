//! A hub's settings ([`HubSettings`]), kept in typed tables of its
//! database (`ensemble_store::kept`) and changed only through its operations,
//! as a GitLab instance keeps its application settings. A hub is always a
//! server (`ensemble-hub serve`), for a team or for one person on their own
//! computer; where it listens is what `serve` is given, not a setting. Its
//! core is its accounts, their roles and what each may do, and the LLM
//! backends it uses. Everything else
//! reaches it through a **connector**, each always on: the REST API, MCP, the
//! runners' link, knowledge, tasks and sources.
//!
//! No setting has a default value: a new hub starts with every one written
//! ([`HubSettings::starting`]), and secrets are named by the variable that
//! holds them.

use super::llm::BackendSetting;
use super::raw::{self, RawDirectAdapter};
use super::validate::{check_candidates, direct_adapter};
use super::*;
use ensemble_core::state::HubDir;
use serde::{Deserialize, Serialize};

/// The hub's settings, resolved.
#[derive(Debug, Clone)]
pub struct Hub {
    /// The hub's name, for its log and its listings.
    pub owner: String,
    /// The LLM backends the hub uses: its own, and runners' it reaches over
    /// their links.
    pub llm: HubLlm,
    pub accounts: Accounts,
    pub connectors: Connectors,
    /// Where it was loaded from, for error messages.
    pub source: PathBuf,
}

/// The hub's accounts: what each role may do, and how people join and sign
/// in. The accounts themselves are in its database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accounts {
    pub rules: AccountRules,
    /// What each role may do, by the role's name.
    pub roles: BTreeMap<String, Vec<Permission>>,
}

/// How people join, how long a login lasts and when failed logins lock a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountRules {
    pub registration: Registration,
    /// Hours a login lasts.
    pub session_hours: u32,
    /// Failed logins before a name is refused, and for how many minutes.
    pub lockout_failures: u32,
    pub lockout_minutes: u32,
}

/// The connectors with settings. The REST API, MCP and the runners' link have
/// none: they are written as `{}`, and are always on all the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connectors {
    pub knowledge: Knowledge,
    pub tasks: Tasks,
    pub sources: Sources,
}

/// Knowledge brought in from outside: how what a source brings is collected
/// and filtered before the distiller. No source adapter is built yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sources {
    /// Absent: no source may be connected.
    pub filter: Option<SourceFilter>,
}

/// The one model that collects and filters what every source brings in,
/// and what the admin counts as confidential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFilter {
    pub model: DirectAdapter,
    /// What is confidential, in the admin's words: it is removed before what
    /// a source brought becomes a share the whole hub may recall.
    pub prompt: String,
}

/// The knowledge vault, and how shares become notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Knowledge {
    /// The vault's directory, in the hub's folder (`knowledge/vault`).
    pub vault: PathBuf,
    /// The notes' vectors, a cache beside the vault
    /// (`knowledge/embeddings.db`).
    pub embeddings: PathBuf,
    /// The embedding model: each note's vector when it is written, and
    /// finding notes by what they mean, for the distiller and for recall.
    /// Absent: notes are found by their words alone.
    pub embed: Option<Embed>,
    /// The models that turn shares into notes, and the most tokens of an answer.
    pub distill: DirectAdapter,
    pub recall: Recall,
}

/// The tasks the hub keeps, and the trackers whose issues become tasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tasks {
    /// By the name the admin gives each; empty for no inbox.
    pub trackers: BTreeMap<String, Tracker>,
}

/// A tracker: issues in it that carry the label become tasks for the helper
/// team of the person they are assigned to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracker {
    pub kind: IssueTracker,
    /// The signal: an issue with this label is for a helper team;
    /// `<label>:<agent>` gives it to that agent.
    pub label: String,
    pub poll_seconds: u32,
}

/// Where issues are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueTracker {
    /// A folder of `<id>.md` issues, made absolute against the hub's directory.
    LocalMarkdown { dir: PathBuf },
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
    /// Receive the tasks the hub's inbox finds assigned to you.
    Tasks,
    /// Manage users: approve sign-ups, give roles, revoke tokens.
    Admin,
    /// Have your tasks worked on by other people's runners that work for the
    /// hub, with the agents they lend, and use their models.
    Compute,
    /// Connect a runner that works for the hub: for others whose role has
    /// `compute`.
    Host,
}

impl Permission {
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::Share => "share",
            Permission::Recall => "recall",
            Permission::Tasks => "tasks",
            Permission::Admin => "admin",
            Permission::Compute => "compute",
            Permission::Host => "host",
        }
    }

    /// Every permission's name.
    pub const NAMES: &'static [&'static str] =
        &["share", "recall", "tasks", "admin", "compute", "host"];
}

/// The issue tracker types that ship.
pub const LOCAL_MARKDOWN_TRACKER: &str = "local_markdown";

/// How much one recall returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recall {
    pub max_notes: u32,
    /// All the notes together.
    pub max_chars: u32,
}

/// An embedding model, which finds notes by what they mean as well as by
/// their words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Embed {
    /// One of the hub's LLM backends, OpenAI-compatible: it serves
    /// `/embeddings`.
    pub backend: String,
    pub model: String,
    /// How alike a note must be to the query to be found by its meaning, in
    /// thousandths of cosine similarity.
    pub min_similarity_permille: u32,
}

impl Embed {
    pub fn min_similarity(&self) -> f32 {
        self.min_similarity_permille as f32 / 1000.0
    }
}

/// The hub's settings as they are kept, in typed tables of its database
/// (`ensemble_store::kept`), and as `export` and `import` write them: every
/// setting written, none with a default. [`HubSettings::resolve`] checks them
/// and makes the [`Hub`] that runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubSettings {
    /// The hub's name, for its log and its listings.
    pub owner: String,
    pub registration: RegistrationSetting,
    /// Hours a login lasts.
    pub session_hours: u32,
    /// Failed logins before a name is refused, and for how many minutes.
    pub lockout_failures: u32,
    pub lockout_minutes: u32,
    /// What each role may do, by the role's name.
    pub roles: BTreeMap<String, Vec<Permission>>,
    /// The LLM backends the hub uses, by name.
    pub backends: BTreeMap<String, BackendSetting>,
    /// The models that turn shares into notes. An empty `try` keeps shares
    /// waiting until one is chosen.
    pub distill: ModelsSetting,
    /// None: notes are found by their words alone.
    pub embed: Option<EmbedSetting>,
    pub recall_max_notes: u32,
    pub recall_max_chars: u32,
    /// The trackers whose issues become tasks, by the name given each.
    pub trackers: BTreeMap<String, TrackerSetting>,
    /// None: no source may be connected.
    pub source_filter: Option<SourceFilterSetting>,
}

/// How people join, as kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RegistrationSetting {
    Approval { max_pending: u32 },
    Closed,
}

/// Models tried in order, and what happens when none can be used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsSetting {
    #[serde(rename = "try")]
    pub candidates: Vec<ModelSetting>,
    pub when_none: WhenNone,
    /// The most tokens of an answer.
    pub max_tokens: u32,
}

/// One model, by the backend that serves it and its id there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSetting {
    pub backend: String,
    pub model: String,
}

/// The embedding model, as kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbedSetting {
    pub backend: String,
    pub model: String,
    /// How alike a note must be to be found by its meaning: 0 to 1.
    pub min_similarity: f64,
}

/// A tracker, as kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TrackerSetting {
    LocalMarkdown {
        /// The folder of issues; a relative path is in the hub's
        /// `trackers/`.
        dir: String,
        label: String,
        poll_seconds: u32,
    },
}

/// The source filter, as kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFilterSetting {
    pub models: ModelsSetting,
    /// What is confidential, in the admin's words.
    pub prompt: String,
}

impl HubSettings {
    /// The settings a new hub starts with: `owner`'s, on this computer
    /// alone, closed to sign-ups, with no backend, model or tracker yet.
    /// Shares wait until a model is chosen; recall finds notes by their words.
    pub fn starting(owner: &str) -> Self {
        let roles = [
            (
                "admin",
                vec![
                    Permission::Admin,
                    Permission::Share,
                    Permission::Recall,
                    Permission::Tasks,
                    Permission::Compute,
                    Permission::Host,
                ],
            ),
            (
                "member",
                vec![
                    Permission::Share,
                    Permission::Recall,
                    Permission::Tasks,
                    Permission::Compute,
                ],
            ),
            ("reader", vec![Permission::Recall]),
        ];
        HubSettings {
            owner: owner.to_string(),
            registration: RegistrationSetting::Closed,
            session_hours: 12,
            lockout_failures: 5,
            lockout_minutes: 15,
            roles: roles
                .into_iter()
                .map(|(name, can)| (name.to_string(), can))
                .collect(),
            backends: BTreeMap::new(),
            distill: ModelsSetting {
                candidates: Vec::new(),
                when_none: WhenNone::Wait,
                max_tokens: 4000,
            },
            embed: None,
            recall_max_notes: 8,
            recall_max_chars: 6000,
            trackers: BTreeMap::new(),
            source_filter: None,
        }
    }

    /// The hub in `dir` these settings make, checked, with the keys they name
    /// from `env`.
    pub fn resolve(&self, dir: &HubDir, env: &dyn Env) -> Result<Hub, ConfigError> {
        let source = &dir.database();
        let mut problems = Vec::new();
        if self.owner.trim().is_empty() {
            problems.push("owner: is empty; name the hub, such as acme or ana".into());
        }
        let (direct, runners) = crate::llm::backends(&self.backends, env, &mut problems);
        let llm = HubLlm { direct, runners };
        let kinds = crate::llm::kept_kinds(&self.backends);
        let registration = match self.registration {
            RegistrationSetting::Approval { max_pending } => {
                if max_pending == 0 {
                    problems.push("registration: `max_pending` must be greater than 0".into());
                }
                Registration::Approval { max_pending }
            }
            RegistrationSetting::Closed => Registration::Closed,
        };
        if self.session_hours == 0 {
            problems.push("session_hours: must be greater than 0".into());
        }
        if self.lockout_failures == 0 || self.lockout_minutes == 0 {
            problems.push("lockout: failures and minutes must be greater than 0".into());
        }
        if self.roles.is_empty() {
            problems.push("roles: name at least one role, such as member: [share, recall]".into());
        }
        for name in self.roles.keys() {
            if name.trim().is_empty() || name.contains(char::is_whitespace) {
                problems.push(format!("roles: the role name '{name}' must be one word"));
            }
        }
        let accounts = Accounts {
            rules: AccountRules {
                registration,
                session_hours: self.session_hours,
                lockout_failures: self.lockout_failures,
                lockout_minutes: self.lockout_minutes,
            },
            roles: self.roles.clone(),
        };
        let distill = models("distill", &self.distill, true, &kinds, &mut problems);
        if self.recall_max_notes == 0 || self.recall_max_chars == 0 {
            problems.push("recall: the most notes and characters must be greater than 0".into());
        }
        let embed = self.embed.as_ref().map(|e| {
            match kinds.get(&e.backend) {
                None => problems.push(format!(
                    "embed.backend: '{}' is not one of the hub's backends",
                    e.backend
                )),
                Some(&OPENAI_COMPATIBLE_API) => {}
                Some(_) => problems.push(format!(
                    "embed.backend: '{}' is not an openai-compatible-api backend, which is what serves /embeddings",
                    e.backend
                )),
            }
            if e.model.trim().is_empty() {
                problems.push("embed.model: is empty".into());
            }
            if !(0.0..=1.0).contains(&e.min_similarity) {
                problems.push("embed.min_similarity: must be from 0 to 1, such as 0.5".into());
            }
            Embed {
                backend: e.backend.clone(),
                model: e.model.trim().to_string(),
                min_similarity_permille: (e.min_similarity.clamp(0.0, 1.0) * 1000.0).round()
                    as u32,
            }
        });
        let knowledge = Knowledge {
            vault: dir.vault(),
            embeddings: dir.knowledge().join("embeddings.db"),
            embed,
            distill,
            recall: Recall {
                max_notes: self.recall_max_notes,
                max_chars: self.recall_max_chars,
            },
        };
        let trackers_dir = dir.trackers();
        let mut trackers = BTreeMap::new();
        for (name, t) in &self.trackers {
            let at = format!("trackers.{name}");
            if !super::runner::valid_name(name) {
                problems.push(format!(
                    "{at}: the name must be letters, digits, '-', '_' or '.'"
                ));
            }
            let TrackerSetting::LocalMarkdown {
                dir,
                label,
                poll_seconds,
            } = t;
            if dir.trim().is_empty() {
                problems.push(format!("{at}.dir: is empty"));
            }
            let label = label.trim();
            if label.is_empty() || label.contains(char::is_whitespace) || label.contains(':') {
                problems.push(format!(
                    "{at}.label: '{label}' must be one word with no ':', such as ensemble"
                ));
            }
            if *poll_seconds == 0 {
                problems.push(format!("{at}: `poll_seconds` must be greater than 0"));
            }
            let tracker = Tracker {
                kind: IssueTracker::LocalMarkdown {
                    dir: trackers_dir.join(dir.trim()),
                },
                label: label.to_string(),
                poll_seconds: *poll_seconds,
            };
            trackers.insert(name.clone(), tracker);
        }
        // Two inboxes on one folder would each take its issues.
        let mut watched: BTreeMap<&Path, &str> = BTreeMap::new();
        for (name, tracker) in &trackers {
            let IssueTracker::LocalMarkdown { dir } = &tracker.kind;
            if let Some(first) = watched.insert(dir, name) {
                problems.push(format!(
                    "trackers.{name}: watches {}, as {first} does; give each tracker a folder of its own",
                    dir.display()
                ));
            }
        }
        let tasks = Tasks { trackers };
        let sources = Sources {
            filter: self.source_filter.as_ref().map(|f| {
                let model = models("source_filter", &f.models, false, &kinds, &mut problems);
                if f.prompt.trim().is_empty() {
                    problems.push(
                        "source_filter.prompt: is empty; say what is confidential, such as salaries and clients' names".into(),
                    );
                }
                SourceFilter {
                    model,
                    prompt: f.prompt.trim().to_string(),
                }
            }),
        };
        match problems.is_empty() {
            true => Ok(Hub {
                owner: self.owner.trim().to_string(),
                llm,
                accounts,
                connectors: Connectors {
                    knowledge,
                    tasks,
                    sources,
                },
                source: source.to_path_buf(),
            }),
            _ => Err(ConfigError::Invalid {
                path: source.to_path_buf(),
                problems,
            }),
        }
    }
}

/// `kept` as a model list, checked against the backends of `kinds`; with
/// `may_be_empty`, an empty `try` is no problem.
fn models(
    at: &str,
    kept: &ModelsSetting,
    may_be_empty: bool,
    kinds: &BTreeMap<String, &'static str>,
    problems: &mut Vec<String>,
) -> DirectAdapter {
    let raw = RawDirectAdapter {
        candidates: kept
            .candidates
            .iter()
            .map(|c| raw::RawDirectCandidate {
                backend: c.backend.clone(),
                model: c.model.clone(),
            })
            .collect(),
        when_none: kept.when_none,
        max_tokens: kept.max_tokens,
    };
    let mut found = Vec::new();
    let adapter = direct_adapter(at, &raw, &mut found);
    let empty = format!("{at}: `try` is empty");
    problems.extend(
        found
            .into_iter()
            .filter(|p| !(may_be_empty && p.starts_with(&empty))),
    );
    check_candidates(
        &format!("{at}.try"),
        &adapter.fallback.candidates,
        false,
        kinds,
        " among the backends",
        problems,
    );
    adapter
}
