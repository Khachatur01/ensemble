//! A hub's settings, one YAML document kept in its database
//! (`ensemble_hub::kept`); `hub.yaml` in `~/.config/ensemble/hub/` before
//! that, which a hub takes in the first time it reads them. A hub is always
//! a server (`ensemble-hub serve`), for a team or for one person on their own
//! computer. Its core is its accounts, their roles and what each may do, the
//! port it listens on (`http`) and the LLM backends it uses (`llm`).
//! Everything else reaches it through a **connector**, each always on and
//! each written in `connectors`: the REST API, MCP, the runners' link,
//! knowledge, tasks and sources. A connector may hold **adapters** the admin
//! picks, such as the trackers of `tasks`; a map of them may be empty.
//!
//! The same rules as `config.yaml`: no default values, unknown keys refused,
//! secrets named by the variable that holds them.

use super::llm::{RawHubLlm, hub_llm, kinds};
use super::raw::RawDirectAdapter;
use super::validate::{check_candidates, direct_adapter, explain};
use super::*;
use serde::Deserialize;

/// The file a hub kept its settings in before its database did.
pub const HUB_FILENAME: &str = "hub.yaml";
pub const HUB_SUPPORTED_VERSION: u32 = 1;

/// The JSON Schema for the hub's settings, for editors. The tests keep it in step with
/// the structs below.
pub const HUB_SCHEMA: &str = include_str!("../../../hub.schema.json");

/// The settings `ensemble-hub init` writes, with what each setting does.
pub const HUB_SAMPLE: &str = include_str!("hub_sample.yaml");

/// The hub's settings, resolved.
#[derive(Debug, Clone)]
pub struct Hub {
    pub version: u32,
    /// The hub's name, for its log and its listings.
    pub owner: String,
    pub http: Http,
    /// The LLM backends the hub uses: its own, and runners' it reaches over
    /// their links.
    pub llm: HubLlm,
    pub accounts: Accounts,
    pub connectors: Connectors,
    /// Where it was loaded from, for error messages.
    pub source: PathBuf,
}

/// The port every connector over HTTP is served on: the REST API at
/// `/api/v1/`, MCP at `/mcp`, and the runners' link at `/link` and `/relay`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Http {
    pub listen: std::net::SocketAddr,
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
    /// The vault's directory, made absolute against the hub's directory.
    pub vault: PathBuf,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawHub {
    pub(crate) version: u32,
    pub(crate) owner: String,
    pub(crate) http: RawHttp,
    pub(crate) llm: RawHubLlm,
    pub(crate) accounts: RawAccounts,
    pub(crate) connectors: RawConnectors,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawHttp {
    pub(crate) listen: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAccounts {
    pub(crate) registration: RawRegistration,
    pub(crate) sessions: RawSessions,
    pub(crate) lockout: RawLockout,
    pub(crate) roles: BTreeMap<String, Vec<Permission>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawConnectors {
    // Never read: parsed so that each is written, and holds nothing.
    #[allow(dead_code)]
    pub(crate) rest: RawNoSettings,
    #[allow(dead_code)]
    pub(crate) mcp: RawNoSettings,
    #[allow(dead_code)]
    pub(crate) runners: RawNoSettings,
    pub(crate) knowledge: RawKnowledge,
    pub(crate) tasks: RawTasks,
    pub(crate) sources: RawSources,
}

/// A connector with no settings: `{}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawNoSettings {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawSources {
    pub(crate) filter: Option<RawSourceFilter>,
    /// No source adapter is built yet, so the one value it takes is `{}`.
    pub(crate) adapters: BTreeMap<String, serde_norway::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawSourceFilter {
    #[serde(rename = "try")]
    pub(crate) candidates: Vec<raw::RawDirectCandidate>,
    pub(crate) when_none: WhenNone,
    pub(crate) max_tokens: u32,
    pub(crate) prompt: String,
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
    pub(crate) trackers: BTreeMap<String, RawIssueTracker>,
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
    pub(crate) label: String,
    pub(crate) poll_seconds: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawKnowledge {
    pub(crate) vault: String,
    pub(crate) embed: Option<RawEmbed>,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawEmbed {
    pub(crate) backend: String,
    pub(crate) model: String,
    pub(crate) min_similarity: f64,
}

pub fn parse_hub(text: &str, path: &Path, env: &dyn Env) -> Result<Hub, ConfigError> {
    // Say what moved rather than which key is unknown.
    if let Ok(v) = serde_norway::from_str::<serde_norway::Value>(text) {
        let old: Vec<&str> = ["server", "roles", "interfaces", "knowledge", "tasks"]
            .into_iter()
            .filter(|k| v.get(k).is_some())
            .collect();
        if !old.is_empty() {
            return Err(ConfigError::Invalid {
                path: path.to_path_buf(),
                problems: vec![format!(
                    "{}: hub.yaml is laid out anew: `server.listen` is `http.listen`; `server.registration`, `server.sessions`, `server.lockout` and `roles` go under `accounts`; `knowledge` and `tasks` go under `connectors`, with `rest: {{}}`, `mcp: {{}}`, `runners: {{}}` and `sources: {{ adapters: {{}} }}`; a tracker is named under `connectors.tasks.trackers`, with its `label` and `poll_seconds`; `interfaces` is gone, and MCP needs no `url`. `ensemble-hub init --stdout` prints the whole file",
                    old.join(", ")
                )],
            });
        }
    }
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
        problems.push("owner: is empty; name the hub, such as acme or ana".into());
    }
    let http = match raw.http.listen.trim().parse() {
        Ok(listen) => Some(Http { listen }),
        Err(_) => {
            problems.push(format!(
                "http.listen: '{}' must be an address and a port, such as 127.0.0.1:7700",
                raw.http.listen
            ));
            None
        }
    };
    let llm = hub_llm(&raw.llm, env, &mut problems);
    let kinds = kinds(&llm.direct, &llm.runners);
    let accounts = {
        let a = &raw.accounts;
        let registration = match &a.registration {
            RawRegistration::Approval(r) => {
                if r.max_pending == 0 {
                    problems
                        .push("accounts.registration: `max_pending` must be greater than 0".into());
                }
                Registration::Approval {
                    max_pending: r.max_pending,
                }
            }
            RawRegistration::Closed(_) => Registration::Closed,
        };
        if a.sessions.hours == 0 {
            problems.push("accounts.sessions: `hours` must be greater than 0".into());
        }
        if a.lockout.failures == 0 || a.lockout.minutes == 0 {
            problems
                .push("accounts.lockout: `failures` and `minutes` must be greater than 0".into());
        }
        if a.roles.is_empty() {
            problems.push(
                "accounts.roles: name at least one role, such as member: [share, recall]".into(),
            );
        }
        for name in a.roles.keys() {
            if name.trim().is_empty() || name.contains(char::is_whitespace) {
                problems.push(format!(
                    "accounts.roles: the role name '{name}' must be one word"
                ));
            }
        }
        Accounts {
            rules: AccountRules {
                registration,
                session_hours: a.sessions.hours,
                lockout_failures: a.lockout.failures,
                lockout_minutes: a.lockout.minutes,
            },
            roles: a.roles.clone(),
        }
    };
    let base = path.parent().unwrap_or(Path::new("."));
    let knowledge = {
        let k = &raw.connectors.knowledge;
        if k.vault.trim().is_empty() {
            problems.push("connectors.knowledge.vault: is empty".into());
        }
        let distill = direct_adapter(
            "connectors.knowledge.distill",
            &RawDirectAdapter {
                candidates: k.distill.candidates.clone(),
                when_none: k.distill.when_none,
                max_tokens: k.distill.max_tokens,
            },
            &mut problems,
        );
        check_candidates(
            "connectors.knowledge.distill.try",
            &distill.fallback.candidates,
            false,
            &kinds,
            " in `llm`",
            &mut problems,
        );
        if k.recall.max_notes == 0 {
            problems.push("connectors.knowledge.recall: `max_notes` must be greater than 0".into());
        }
        if k.recall.max_chars == 0 {
            problems.push("connectors.knowledge.recall: `max_chars` must be greater than 0".into());
        }
        let embed = k.embed.as_ref().map(|e| {
            match kinds.get(&e.backend) {
                None => problems.push(format!(
                    "connectors.knowledge.embed.backend: '{}' is not one of the backends of `llm`",
                    e.backend
                )),
                Some(&OPENAI_COMPATIBLE_API) => {}
                Some(_) => problems.push(format!(
                    "connectors.knowledge.embed.backend: '{}' is not an openai-compatible-api backend, which is what serves /embeddings",
                    e.backend
                )),
            }
            if e.model.trim().is_empty() {
                problems.push("connectors.knowledge.embed.model: is empty".into());
            }
            if !(0.0..=1.0).contains(&e.min_similarity) {
                problems.push(
                    "connectors.knowledge.embed.min_similarity: must be from 0 to 1, such as 0.5"
                        .into(),
                );
            }
            Embed {
                backend: e.backend.clone(),
                model: e.model.trim().to_string(),
                min_similarity_permille: (e.min_similarity.clamp(0.0, 1.0) * 1000.0).round()
                    as u32,
            }
        });
        Knowledge {
            vault: base.join(k.vault.trim()),
            embed,
            distill,
            recall: Recall {
                max_notes: k.recall.max_notes,
                max_chars: k.recall.max_chars,
            },
        }
    };
    let trackers = &raw.connectors.tasks.trackers;
    let tasks = Tasks {
        trackers: trackers
            .iter()
            .map(|(name, t)| {
                let at = format!("connectors.tasks.trackers.{name}");
                if !super::runner::valid_name(name) {
                    problems.push(format!(
                        "{at}: the name must be letters, digits, '-', '_' or '.'"
                    ));
                }
                let RawIssueTracker::LocalMarkdown(m) = t;
                if m.dir.trim().is_empty() {
                    problems.push(format!("{at}.dir: is empty"));
                }
                let label = m.label.trim();
                if label.is_empty() || label.contains(char::is_whitespace) || label.contains(':') {
                    problems.push(format!(
                        "{at}.label: '{label}' must be one word with no ':', such as ensemble"
                    ));
                }
                if m.poll_seconds == 0 {
                    problems.push(format!("{at}: `poll_seconds` must be greater than 0"));
                }
                let tracker = Tracker {
                    kind: IssueTracker::LocalMarkdown {
                        dir: base.join(m.dir.trim()),
                    },
                    label: label.to_string(),
                    poll_seconds: m.poll_seconds,
                };
                (name.clone(), tracker)
            })
            .collect(),
    };
    // Two inboxes on one folder would each take its issues.
    let mut watched: BTreeMap<&Path, &str> = BTreeMap::new();
    for (name, tracker) in &tasks.trackers {
        let IssueTracker::LocalMarkdown { dir } = &tracker.kind;
        if let Some(first) = watched.insert(dir, name) {
            problems.push(format!(
                "connectors.tasks.trackers.{name}: watches {}, as {first} does; give each tracker a folder of its own",
                dir.display()
            ));
        }
    }
    for name in raw.connectors.sources.adapters.keys() {
        problems.push(format!(
            "connectors.sources.adapters.{name}: no source adapter is built yet; leave `adapters: {{}}`"
        ));
    }
    let sources = Sources {
        filter: raw.connectors.sources.filter.as_ref().map(|f| {
            let at = "connectors.sources.filter";
            let model = direct_adapter(
                at,
                &RawDirectAdapter {
                    candidates: f.candidates.clone(),
                    when_none: f.when_none,
                    max_tokens: f.max_tokens,
                },
                &mut problems,
            );
            check_candidates(
                &format!("{at}.try"),
                &model.fallback.candidates,
                false,
                &kinds,
                " in `llm`",
                &mut problems,
            );
            if f.prompt.trim().is_empty() {
                problems.push(format!(
                    "{at}.prompt: is empty; say what is confidential, such as salaries and clients' names"
                ));
            }
            SourceFilter {
                model,
                prompt: f.prompt.trim().to_string(),
            }
        }),
    };
    for name in kinds.keys() {
        let uses = |a: &DirectAdapter| a.fallback.candidates.iter().any(|c| &c.backend == name);
        let used = uses(&knowledge.distill)
            || knowledge.embed.as_ref().is_some_and(|e| &e.backend == name)
            || sources.filter.as_ref().is_some_and(|f| uses(&f.model));
        if !used {
            eprintln!(
                "ensemble: note: LLM backend '{name}' is defined but nothing in the hub uses it"
            );
        }
    }
    match (problems.is_empty(), http) {
        (true, Some(http)) => Ok(Hub {
            version: raw.version,
            owner: raw.owner.trim().to_string(),
            http,
            llm,
            accounts,
            connectors: Connectors {
                knowledge,
                tasks,
                sources,
            },
            source: path.to_path_buf(),
        }),
        _ => Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            problems,
        }),
    }
}
