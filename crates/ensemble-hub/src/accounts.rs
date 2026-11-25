//! The hub's operations ([`operations`]): its accounts, their tokens and
//! sessions, and its settings, for every interface to reach: the CLI on
//! the hub's computer, and the REST API.
//!
//! | Operation | Who |
//! |---|---|
//! | `user.list`, `user.show`, `user.add`, `user.approve`, `user.role`, `user.password`, `user.tracker`, `user.token`, `user.join-code`, `user.revoke`, `user.remove`, `role.list` | admin |
//! | `settings.*`, `backend.*` and the goal operations (`crate::goals`) | admin |
//! | `me.show` | any signed-in account |
//! | `me.tracker`, `me.password`, `me.tokens`, `me.token`, `me.join-code`, `me.revoke`, `account.logout` | a session |
//! | `runner.backends` | admin, of the running hub |
//! | `account.register`, `account.login`, `token.login`, `token.join` | anyone |
//!
//! A **session** comes from logging in with the password, and only a session
//! may change the password or make and revoke access tokens, so a stolen access
//! token cannot make more. An **access token** may be narrowed by scopes, and
//! is what clients use: a runner, an agent, a script. A client gets one of its
//! own, named after it, with `token.login` (a password) or `token.join` (a
//! **join code**, made by an admin or by the account's session and spent
//! once), over the REST API or over the link, on a connection without a token.

use crate::users::{JOIN_HOURS, Kind, Roles, Token, User, Users, UsersError};
use ensemble_backends::models::Found;
use ensemble_backends::ops::{Side, WithPool};
use ensemble_config::kept::{Change, Kept, WithKept};
use ensemble_config::{AnnPool, AnnPoolSetting, HubSettings, ModelKind, Permission, Registration};
use ensemble_core::state::HubDir;
use ensemble_ops::{
    Access, Caller, Code, Error, Field, Kind as FieldKind, Operation, Registry, Said, Spec,
    operation,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

/// The hub's accounts, in its database, and what changing them needs.
pub struct Accounts {
    /// The hub's core, with its database.
    dir: PathBuf,
    /// The roles, and how people join, how long a login lasts and when failed
    /// logins lock a name: as the hub's settings say now ([`Accounts::apply`]).
    roles: RwLock<Roles>,
    rules: RwLock<ensemble_config::AccountRules>,
    /// Held while the accounts are read, changed and written, so two requests
    /// at once cannot lose a change.
    changing: Mutex<()>,
    /// Failed logins by name: how many, and until when the name is refused.
    failures: Mutex<HashMap<String, (u32, Option<Instant>)>>,
}

impl Accounts {
    pub fn new(dir: &Path, roles: Roles, rules: ensemble_config::AccountRules) -> Self {
        Accounts {
            dir: dir.to_path_buf(),
            roles: RwLock::new(roles),
            rules: RwLock::new(rules),
            changing: Mutex::new(()),
            failures: Mutex::new(HashMap::new()),
        }
    }

    /// Use `roles` and `rules`, as the hub's settings say them now.
    pub fn apply(&self, roles: Roles, rules: ensemble_config::AccountRules) {
        *self.roles.write().unwrap_or_else(|e| e.into_inner()) = roles;
        *self.rules.write().unwrap_or_else(|e| e.into_inner()) = rules;
    }

    pub fn roles(&self) -> Roles {
        self.roles.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn rules(&self) -> ensemble_config::AccountRules {
        *self.rules.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn users(&self) -> Result<Users, Error> {
        Ok(Users::load(&self.dir)?)
    }

    /// Read the accounts, change them, and write them, while no other request
    /// does.
    fn change<T>(&self, f: impl FnOnce(&mut Users) -> Result<T, UsersError>) -> Result<T, Error> {
        let _held = self.changing.lock().unwrap_or_else(|e| e.into_inner());
        let mut users = self.users()?;
        let out = f(&mut users)?;
        users.save()?;
        Ok(out)
    }

    /// Who a request signed in with `token` is: an account, with the
    /// permissions of its role and token, or no one.
    pub fn caller(&self, token: Option<&str>) -> Caller {
        let signed = token.and_then(|t| self.users().ok()?.authenticate(t, &self.roles()));
        match signed {
            Some(s) => Caller::Account {
                name: s.user.name,
                permissions: names(&s.permissions)
                    .into_iter()
                    .map(String::from)
                    .collect(),
                session: s.kind == Kind::Session,
                token: s.token_id,
            },
            None => Caller::Anyone,
        }
    }
}

impl From<UsersError> for Error {
    fn from(e: UsersError) -> Error {
        let code = match e {
            UsersError::NotFound(_) => Code::NotFound,
            UsersError::Conflict(_) => Code::Conflict,
            UsersError::Invalid(_) => Code::Invalid,
            UsersError::Unauthorized(_) => Code::Unauthorized,
            UsersError::Forbidden(_) => Code::Forbidden,
            UsersError::Io(_) => Code::Failed,
        };
        Error::new(code, e.to_string())
    }
}

/// What the hub's operations are given: its accounts, when its settings
/// load, and its settings, which may be changed even when they do not.
pub struct HubContext {
    dir: HubDir,
    /// Its accounts, made the first time an operation asks for them.
    accounts: OnceLock<Result<Accounts, String>>,
    /// Its settings, loaded: its roles and account rules, or why not.
    load: Box<Load>,
    settings: Kept<HubSettings>,
    /// What the running hub does once its settings changed, and says of it:
    /// set by the hub ([`HubContext::when_changed`]); nothing otherwise.
    changed: Arc<OnceLock<Box<Changed>>>,
    /// The models the connected runners share, by provider, as the running hub
    /// lists them ([`HubContext::when_serving`]); nothing otherwise.
    offered: OnceLock<Box<Offered>>,
    /// Where the running hub listens ([`HubContext::listening`]); nothing
    /// otherwise.
    listen: OnceLock<SocketAddr>,
    /// How the running hub answers a request of the link for an account
    /// ([`HubContext::when_linked`]); nothing otherwise.
    linked: OnceLock<Box<Linked>>,
}

type Offered = dyn Fn() -> Value + Send + Sync;
/// A request of the link, `method` with `params`, asked by an account with
/// these permissions, and its answer.
pub type Linked = dyn Fn(&str, &[Permission], &str, Value) -> Result<Value, String> + Send + Sync;
type Load = dyn Fn() -> Result<(Roles, ensemble_config::AccountRules), String> + Send + Sync;
type Changed = dyn Fn() -> Option<String> + Send + Sync;

impl HubContext {
    /// The hub in `dir`: its accounts, from what `load` reads of its settings
    /// when an operation first needs them, and its settings, in its database.
    pub fn new(
        dir: &HubDir,
        load: impl Fn() -> Result<(Roles, ensemble_config::AccountRules), String>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        let checked = dir.clone();
        let changed: Arc<OnceLock<Box<Changed>>> = Arc::default();
        let settings = Kept::new(
            crate::kept::settings(dir),
            crate::kept::SETTINGS_NAME,
            "`ensemble-hub init`",
            move |s: &HubSettings| {
                s.resolve(&checked, &ensemble_config::SystemEnv)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            },
        )
        .on_change({
            let changed = changed.clone();
            move || changed.get().and_then(|apply| apply())
        });
        HubContext {
            dir: dir.clone(),
            accounts: OnceLock::new(),
            load: Box::new(load),
            settings,
            changed,
            offered: OnceLock::new(),
            listen: OnceLock::new(),
            linked: OnceLock::new(),
        }
    }

    /// Have the operations that the running hub answers (`crate::served`)
    /// ask `answer`, as the link's request of the same name.
    pub fn when_linked(
        &self,
        answer: impl Fn(&str, &[Permission], &str, Value) -> Result<Value, String>
        + Send
        + Sync
        + 'static,
    ) {
        let _ = self.linked.set(Box::new(answer));
    }

    /// Ask the running hub the link's request `method` with `params`, for
    /// `caller`: its answer, or why not.
    pub(crate) fn ask(&self, caller: &Caller, method: &str, params: Value) -> Result<Value, Error> {
        let Caller::Account {
            name, permissions, ..
        } = caller
        else {
            return Err(Error::new(
                Code::Forbidden,
                "this is done by a signed-in account, for itself",
            ));
        };
        let answer = self.linked.get().ok_or_else(|| {
            Error::new(
                Code::Unavailable,
                "only the running hub answers this; start it with `ensemble-hub serve`",
            )
        })?;
        let permissions: Vec<Permission> = permissions
            .iter()
            .filter_map(|p| serde_json::from_value(json!(p)).ok())
            .collect();
        answer(name, &permissions, method, params).map_err(|e| {
            if e.contains("do not allow") {
                Error::new(Code::Forbidden, e)
            } else {
                Error::invalid(e)
            }
        })
    }

    /// The hub's folder.
    pub(crate) fn dir(&self) -> &HubDir {
        &self.dir
    }

    /// Say that the running hub listens on `listen`, which the hints of new
    /// tokens and join codes name when no request does.
    pub fn listening(&self, listen: SocketAddr) {
        let _ = self.listen.set(listen);
    }

    /// Its settings as they are now, with the keys they name.
    pub(crate) fn hub(&self) -> Result<ensemble_config::Hub, Error> {
        crate::kept::load(&self.dir).map_err(|e| {
            Error::new(Code::Unavailable, e)
                .hint("`ensemble-hub settings show` shows them; the operations change them")
        })
    }

    /// Have `runner.models` list what `offered` returns: the models the
    /// runners connected to the running hub share, `[{runner, provider,
    /// max_concurrent, models: [{id, kind}]}]`.
    pub fn when_serving(&self, offered: impl Fn() -> Value + Send + Sync + 'static) {
        let _ = self.offered.set(Box::new(offered));
    }

    /// Have `apply` run once its settings changed, and say what it returns:
    /// what the running hub does with the change.
    pub fn when_changed(&self, apply: impl Fn() -> Option<String> + Send + Sync + 'static) {
        let _ = self.changed.set(Box::new(apply));
    }

    pub fn accounts(&self) -> Result<&Accounts, Error> {
        let made = self.accounts.get_or_init(|| {
            (self.load)().map(|(roles, rules)| Accounts::new(&self.dir.core(), roles, rules))
        });
        made.as_ref().map_err(|why| {
            Error::new(Code::Unavailable, format!("the hub's accounts: {why}"))
                .hint("`ensemble-hub settings show` shows its settings; the operations change them")
        })
    }
}

impl WithKept<HubSettings> for HubContext {
    fn kept(&self) -> Result<&Kept<HubSettings>, Error> {
        Ok(&self.settings)
    }
}

/// Its keys are kept sealed in its database ([`crate::kept`]).
impl WithPool for HubContext {
    fn can_keep_keys(&self) -> Result<(), Error> {
        crate::kept::keys(&self.dir).ready().map_err(Error::failed)
    }

    fn keep_key(&self, name: &str, key: Option<&str>) -> Result<(), Error> {
        crate::kept::keys(&self.dir)
            .keep(name, key)
            .map_err(Error::failed)
    }

    fn variable(&self, name: &str) -> Option<String> {
        use ensemble_config::Env;
        crate::kept::variables(&self.dir).ok()?.get(name)
    }

    fn kept_pool(&self) -> Result<AnnPoolSetting, Error> {
        Ok(self.settings.read()?.ann_pool)
    }

    fn change_pool(
        &self,
        change: Change,
        edit: &mut dyn FnMut(&mut AnnPoolSetting) -> Result<(), Error>,
    ) -> Result<Option<String>, Error> {
        self.settings.change(change, |s| edit(&mut s.ann_pool))
    }

    fn pool_changed(&self) -> Option<String> {
        self.settings.changed()
    }

    fn pool(&self) -> Result<AnnPool, Error> {
        Ok(self.hub()?.llm)
    }

    /// As the runner's relay socket offered them, while the hub runs.
    fn shared_by(&self, runner: &str) -> Result<Vec<Found>, Error> {
        let offered = self.offered.get().ok_or_else(|| {
            Error::new(
                Code::Unavailable,
                "only the running hub knows what the connected runners share; start it with `ensemble-hub serve`",
            )
        })?();
        let providers: Vec<&Value> = offered
            .as_array()
            .into_iter()
            .flatten()
            .filter(|o| o["runner"] == runner)
            .collect();
        if providers.is_empty() {
            return Err(Error::new(
                Code::Unavailable,
                format!("{runner} is not connected now, or shares no model"),
            ));
        }
        Ok(providers
            .iter()
            .flat_map(|o| {
                let provider = o["provider"].as_str().unwrap_or_default();
                o["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(move |m| Found {
                        id: format!("{provider}:{}", m["id"].as_str().unwrap_or_default()),
                        kind: serde_json::from_value(m["kind"].clone()).ok(),
                    })
            })
            .collect())
    }
}

/// Every operation of the hub.
pub fn operations() -> Registry<HubContext> {
    let mut all = user_operations();
    all.extend(own_operations());
    all.extend(sign_in_operations());
    all.push(runner_models());
    all.extend(crate::served::operations());
    all.extend(ensemble_backends::ops::operations::<HubContext>(
        Side::Hub,
        Access::Permission("admin"),
    ));
    all.extend(crate::goals::operations());
    all.extend(crate::task_picker::operations());
    Registry::new(all)
}

#[derive(Deserialize)]
struct Nothing {}

#[derive(Deserialize)]
struct Named {
    name: String,
}

#[derive(Deserialize)]
struct NamedRole {
    name: String,
    role: String,
}

#[derive(Deserialize)]
struct Add {
    name: String,
    role: String,
    password: Option<String>,
}

#[derive(Deserialize)]
struct Password {
    name: String,
    password: String,
}

#[derive(Deserialize)]
struct Tracker {
    name: String,
    tracker_user: Option<String>,
}

const ADMIN: Access = Access::Permission("admin");
const NAME: Field = Field::text("name", "The account's name.")
    .required()
    .positional();
const ROLE: Field = Field::text("role", "One of the hub's roles.").required();
const NAMED: &[Field] = &[NAME];
const NAMED_ROLE: &[Field] = &[NAME, ROLE];
const ADD: &[Field] = &[
    NAME,
    ROLE,
    Field::secret(
        "password",
        "Their password; left out, the account gets a token to sign in with instead.",
    ),
];
const PASSWORD: &[Field] = &[
    NAME,
    Field::secret("password", "The new password.").required(),
];
const TRACKER: &[Field] = &[
    NAME,
    Field::text(
        "tracker_user",
        "Who the account is in the tracker; left out, its own name.",
    )
    .positional(),
];

/// What a new token's owner is told.
fn token_note(c: &HubContext, name: &str, token: &str) -> String {
    format!(
        "This is {name}'s token, shown once: a client of the hub at {} signs in with it. For their agent, as its API key for the hub's MCP: {}",
        hub_base(c),
        mcp_line(c, token)
    )
}

/// What a new join code's owner is told.
fn join_note(c: &HubContext, name: &str, code: &str) -> String {
    format!(
        "This is {name}'s join code, shown once and good for one sign-in within {JOIN_HOURS} hours: a client of the hub at {} spends it for a token of its own, as `ensemble-runner join {} {code}` does.",
        hub_base(c),
        hub_url(c)
    )
}

thread_local! {
    /// The address the request being answered reached the hub at.
    static REACHED_AT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Run `f` for a request that reached the hub at `base`, such as
/// `https://hub.example.com`: the hints a new token or join code comes with
/// name it.
pub(crate) fn reached_at<T>(base: Option<String>, f: impl FnOnce() -> T) -> T {
    let before = REACHED_AT.replace(base);
    let out = f();
    REACHED_AT.set(before);
    out
}

/// The address clients reach the hub at: the one the request being answered
/// reached it at; else where it listens, when that is one address; else a
/// stand-in for the person to fill.
pub(crate) fn hub_base(c: &HubContext) -> String {
    if let Some(base) = REACHED_AT.with_borrow(Clone::clone) {
        return base;
    }
    match c.listen.get() {
        Some(listen) if !listen.ip().is_unspecified() => format!("http://{listen}"),
        _ => "<hub-url>".into(),
    }
}

/// The address a runner names its hub by: the hub's, as a WebSocket's.
fn hub_url(c: &HubContext) -> String {
    hub_base(c)
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1)
}

/// The line that adds the hub's MCP to an agent with `token`: what a person
/// pastes, or asks their agent to run.
fn mcp_line(c: &HubContext, token: &str) -> String {
    format!(
        "claude mcp add --transport http ensemble {}{} --header \"Authorization: Bearer {token}\"",
        hub_base(c),
        crate::mcp::PATH
    )
}

fn user_operations() -> Vec<Operation<HubContext>> {
    vec![
        operation(
            Spec {
                name: "user.list",
                summary: "Every account: its status, role and tokens.",
                access: ADMIN,
                input: &[],
            },
            |c: &HubContext, _: &Caller, _: Nothing| {
                let users = c.accounts()?.users()?;
                Ok(json!({ "users": users.list().map(view).collect::<Vec<_>>() }))
            },
        )
        .shown(|out| Said {
            out: out["users"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|u| {
                    let role = u["role"].as_str().filter(|r| !r.is_empty()).unwrap_or("-");
                    format!(
                        "{:<16} {:<8} {:<12} {} token(s)",
                        u["name"].as_str().unwrap_or(""),
                        u["status"].as_str().unwrap_or(""),
                        role,
                        u["tokens"]
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            note: None,
        }),
        operation(
            Spec {
                name: "user.show",
                summary: "One account: its status, role, tracker name and tokens.",
                access: ADMIN,
                input: NAMED,
            },
            |c: &HubContext, _: &Caller, input: Named| {
                let users = c.accounts()?.users()?;
                let user = users.get(&input.name).ok_or_else(|| {
                    Error::not_found(format!("no user called {}", input.name)).on("name")
                })?;
                Ok(view(user))
            },
        ),
        operation(
            Spec {
                name: "user.add",
                summary: "Add an account; without a password, it gets a token, shown once.",
                access: ADMIN,
                input: ADD,
            },
            |c: &HubContext, _: &Caller, input: Add| {
                let accounts = c.accounts()?;
                let (name, role) = (input.name.as_str(), input.role.as_str());
                match &input.password {
                    Some(password) => {
                        accounts.change(|u| u.create(name, role, password, &accounts.roles()))?;
                        Ok(json!({
                            "name": name, "role": role, "status": "active",
                            "said": format!("added {name} as {role}; they log in with their password"),
                        }))
                    }
                    None => {
                        let token = accounts.change(|u| u.add(name, role, &accounts.roles()))?;
                        Ok(json!({
                            "name": name, "role": role, "status": "active", "token": token,
                            "said": token, "note": token_note(c, name, &token),
                        }))
                    }
                }
            },
        ),
        operation(
            Spec {
                name: "user.approve",
                summary: "Let in someone who signed up, with a role.",
                access: ADMIN,
                input: NAMED_ROLE,
            },
            |c: &HubContext, _: &Caller, input: NamedRole| {
                let accounts = c.accounts()?;
                accounts.change(|u| u.approve(&input.name, &input.role, &accounts.roles()))?;
                Ok(json!({
                    "name": input.name, "role": input.role, "status": "active",
                    "said": format!("approved {} as {}; their token works now", input.name, input.role),
                }))
            },
        ),
        operation(
            Spec {
                name: "user.role",
                summary: "Give an account another role.",
                access: ADMIN,
                input: NAMED_ROLE,
            },
            |c: &HubContext, _: &Caller, input: NamedRole| {
                let accounts = c.accounts()?;
                accounts.change(|u| u.set_role(&input.name, &input.role, &accounts.roles()))?;
                Ok(json!({
                    "name": input.name, "role": input.role,
                    "said": format!("{} is {} now", input.name, input.role),
                }))
            },
        ),
        operation(
            Spec {
                name: "user.password",
                summary: "Set an account's password; its sessions end.",
                access: ADMIN,
                input: PASSWORD,
            },
            |c: &HubContext, _: &Caller, input: Password| {
                c.accounts()?
                    .change(|u| u.set_password(&input.name, &input.password))?;
                Ok(json!({
                    "name": input.name, "changed": true,
                    "said": format!("set {}'s password; their sessions were ended", input.name),
                }))
            },
        ),
        operation(
            Spec {
                name: "user.tracker",
                summary: "Who the account is in the tracker; without one, its own name.",
                access: ADMIN,
                input: TRACKER,
            },
            |c: &HubContext, _: &Caller, input: Tracker| {
                let name = &input.name;
                let tracker = input
                    .tracker_user
                    .as_deref()
                    .filter(|t| !t.trim().is_empty());
                c.accounts()?
                    .change(|u| u.set_tracker_user(name, tracker))?;
                Ok(json!({
                    "name": name, "tracker_user": tracker,
                    "said": match tracker {
                        Some(t) => format!("{name} is {t} in the tracker"),
                        None => format!("{name} is {name} in the tracker, the account's own name"),
                    },
                }))
            },
        ),
        operation(
            Spec {
                name: "user.token",
                summary: "Make an account a new token, shown once.",
                access: ADMIN,
                input: NAMED,
            },
            |c: &HubContext, _: &Caller, input: Named| {
                let token = c.accounts()?.change(|u| u.token(&input.name))?;
                Ok(
                    json!({ "name": input.name, "token": token, "said": token, "note": token_note(c, &input.name, &token) }),
                )
            },
        ),
        operation(
            Spec {
                name: "user.join-code",
                summary: "Make an account a join code, shown once, that signs one client in.",
                access: ADMIN,
                input: NAMED,
            },
            |c: &HubContext, _: &Caller, input: Named| join_code(c, &input.name),
        ),
        operation(
            Spec {
                name: "user.revoke",
                summary: "Revoke every token of an account.",
                access: ADMIN,
                input: NAMED,
            },
            |c: &HubContext, _: &Caller, input: Named| {
                let revoked = c.accounts()?.change(|u| u.revoke(&input.name))?;
                Ok(json!({
                    "name": input.name, "revoked": revoked,
                    "said": format!(
                        "revoked {revoked} token(s) of {}; their runners and agents are signed out at their next request",
                        input.name
                    ),
                }))
            },
        ),
        operation(
            Spec {
                name: "user.remove",
                summary: "Remove an account, or turn down a sign-up.",
                access: ADMIN,
                input: NAMED,
            },
            |c: &HubContext, _: &Caller, input: Named| {
                c.accounts()?.change(|u| u.remove(&input.name))?;
                Ok(
                    json!({ "name": input.name, "removed": true, "said": format!("removed {}", input.name) }),
                )
            },
        ),
        operation(
            Spec {
                name: "role.list",
                summary: "The hub's roles, and what each may do.",
                access: ADMIN,
                input: &[],
            },
            |c: &HubContext, _: &Caller, _: Nothing| {
                let roles: serde_json::Map<String, Value> = c
                    .accounts()?
                    .roles()
                    .iter()
                    .map(|(name, perms)| (name.clone(), json!(names(perms))))
                    .collect();
                Ok(json!({ "roles": roles }))
            },
        ),
    ]
}

#[derive(Deserialize)]
struct Login {
    name: String,
    password: String,
}

#[derive(Deserialize)]
struct OwnTracker {
    tracker_user: Option<String>,
}

#[derive(Deserialize)]
struct OwnPassword {
    current: String,
    new: String,
}

#[derive(Deserialize)]
struct MakeToken {
    name: String,
    expires_days: Option<u32>,
    scopes: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct TokenId {
    id: String,
}

const LOGIN: &[Field] = &[
    Field::text("name", "Your account's name.").required(),
    Field::secret("password", "Your password.").required(),
];
const OWN_TRACKER: &[Field] = &[Field::text(
    "tracker_user",
    "Who you are in the tracker; empty, your own name.",
)];
const OWN_PASSWORD: &[Field] = &[
    Field::secret("current", "Your password now.").required(),
    Field::secret("new", "The new one.").required(),
];
const MAKE_TOKEN: &[Field] = &[
    Field::text("name", "What it is for, such as laptop.").required(),
    Field::new(
        "expires_days",
        FieldKind::Number,
        "Days until it stops working; left out, never.",
    ),
    Field::new(
        "scopes",
        FieldKind::Choices(ensemble_config::Permission::NAMES),
        "What it may do, within your role: share, recall, tasks, admin.",
    ),
];
const TOKEN_ID: &[Field] = &[Field::text("id", "The token's id.").required()];

/// The signed-in caller's account name.
fn own(caller: &Caller) -> Result<&str, Error> {
    caller
        .account()
        .ok_or_else(|| Error::new(Code::Unauthorized, "a signed-in account is needed"))
}

fn own_operations() -> Vec<Operation<HubContext>> {
    vec![
        operation(
            Spec {
                name: "account.register",
                summary: "Sign up, to be let in by an admin.",
                access: Access::Anyone,
                input: LOGIN,
            },
            |c: &HubContext, _: &Caller, input: Login| {
                let accounts = c.accounts()?;
                let Registration::Approval { max_pending } = accounts.rules().registration else {
                    return Err(Error::new(
                        Code::Forbidden,
                        "this hub does not take sign-ups; ask its admin to add you",
                    ));
                };
                accounts.change(|u| u.register(&input.name, &input.password, max_pending))?;
                Ok(json!({
                    "name": input.name, "status": "pending",
                    "note": "Log in once an admin approves you.",
                }))
            },
        ),
        operation(
            Spec {
                name: "account.login",
                summary: "Log in with a password, for a session.",
                access: Access::Anyone,
                input: LOGIN,
            },
            |c: &HubContext, _: &Caller, input: Login| login(c.accounts()?, &input),
        ),
        operation(
            Spec {
                name: "account.logout",
                summary: "End this session.",
                access: Access::Session,
                input: &[],
            },
            |c: &HubContext, caller: &Caller, _: Nothing| {
                let Caller::Account { name, token, .. } = caller else {
                    return Err(Error::new(Code::Unauthorized, "no session to end"));
                };
                c.accounts()?.change(|u| u.revoke_token(name, token))?;
                Ok(json!({ "signed_out": true }))
            },
        ),
        operation(
            Spec {
                name: "me.show",
                summary: "Your account, and what this token may do.",
                access: Access::Signed,
                input: &[],
            },
            |c: &HubContext, caller: &Caller, _: Nothing| {
                let Caller::Account {
                    name,
                    permissions,
                    session,
                    token,
                } = caller
                else {
                    return Err(Error::new(
                        Code::Unauthorized,
                        "a signed-in account is needed",
                    ));
                };
                let users = c.accounts()?.users()?;
                let user = users
                    .get(name)
                    .ok_or_else(|| Error::not_found(format!("no user called {name}")))?;
                let mut out = view(user);
                out["permissions"] = json!(permissions);
                out["token"] = json!({
                    "id": token,
                    "kind": if *session { Kind::Session } else { Kind::Personal }.as_str(),
                });
                Ok(out)
            },
        ),
        operation(
            Spec {
                name: "me.tracker",
                summary: "Who you are in the tracker.",
                access: Access::Session,
                input: OWN_TRACKER,
            },
            |c: &HubContext, caller: &Caller, input: OwnTracker| {
                let name = own(caller)?;
                let tracker = input
                    .tracker_user
                    .as_deref()
                    .filter(|t| !t.trim().is_empty());
                c.accounts()?
                    .change(|u| u.set_tracker_user(name, tracker))?;
                Ok(json!({ "name": name, "tracker_user": tracker }))
            },
        ),
        operation(
            Spec {
                name: "me.password",
                summary: "Change your password; every session ends.",
                access: Access::Session,
                input: OWN_PASSWORD,
            },
            |c: &HubContext, caller: &Caller, input: OwnPassword| {
                let name = own(caller)?;
                c.accounts()?.change(|users| {
                    users.verify(name, &input.current).map_err(|_| {
                        UsersError::Unauthorized("the current password is wrong".into())
                    })?;
                    users.set_password(name, &input.new)
                })?;
                Ok(json!({ "changed": true, "note": "Every session was ended; log in again." }))
            },
        ),
        operation(
            Spec {
                name: "me.tokens",
                summary: "Your access tokens.",
                access: Access::Session,
                input: &[],
            },
            |c: &HubContext, caller: &Caller, _: Nothing| {
                let users = c.accounts()?.users()?;
                let tokens = users.tokens(own(caller)?)?;
                Ok(json!({ "tokens": tokens.into_iter().map(token_view).collect::<Vec<_>>() }))
            },
        ),
        operation(
            Spec {
                name: "me.token",
                summary: "Make yourself an access token, shown once.",
                access: Access::Session,
                input: MAKE_TOKEN,
            },
            |c: &HubContext, caller: &Caller, input: MakeToken| {
                let name = own(caller)?;
                let scopes = match &input.scopes {
                    None => None,
                    Some(list) => Some(
                        serde_json::from_value::<Vec<Permission>>(json!(list)).map_err(|_| {
                            Error::invalid(
                                "`scopes` must be a list of share, recall, tasks or admin",
                            )
                            .on("scopes")
                        })?,
                    ),
                };
                let accounts = c.accounts()?;
                let (id, token, expires) = accounts.change(|users| {
                    users.create_token(
                        name,
                        &input.name,
                        input.expires_days,
                        scopes.clone(),
                        &accounts.roles(),
                    )
                })?;
                Ok(json!({
                    "id": id,
                    "name": input.name,
                    "token": token,
                    "scopes": scopes.as_deref().map(names),
                    "expires": expires,
                    "note": format!(
                        "Keep this token: it is shown once. For your agent, as its API key for the hub's MCP: {}",
                        mcp_line(c, &token)
                    ),
                }))
            },
        ),
        operation(
            Spec {
                name: "me.join-code",
                summary: "Make yourself a join code, shown once, that signs one client in.",
                access: Access::Session,
                input: &[],
            },
            |c: &HubContext, caller: &Caller, _: Nothing| join_code(c, own(caller)?),
        ),
        operation(
            Spec {
                name: "me.revoke",
                summary: "Revoke one of your access tokens.",
                access: Access::Session,
                input: TOKEN_ID,
            },
            |c: &HubContext, caller: &Caller, input: TokenId| {
                let name = own(caller)?;
                c.accounts()?.change(|u| u.revoke_token(name, &input.id))?;
                Ok(json!({ "id": input.id, "revoked": true }))
            },
        ),
    ]
}

#[derive(Deserialize)]
struct ClientLogin {
    name: String,
    password: String,
    client: String,
}

#[derive(Deserialize)]
struct Join {
    code: String,
    client: String,
}

const CLIENT: Field = Field::text(
    "client",
    "What signs in, such as `runner laptop`: its new access token is named after it.",
)
.required();
const CLIENT_LOGIN: &[Field] = &[
    Field::text("name", "Your account's name.").required(),
    Field::secret("password", "Your password.").required(),
    CLIENT,
];
const JOIN: &[Field] = &[
    Field::text("code", "A join code made for your account.").required(),
    CLIENT,
];

/// How a client signs in, without a token: for an access token of its own.
fn sign_in_operations() -> Vec<Operation<HubContext>> {
    vec![
        operation(
            Spec {
                name: "token.login",
                summary: "Sign a client in with a password, for an access token of its own.",
                access: Access::Anyone,
                input: CLIENT_LOGIN,
            },
            |c: &HubContext, _: &Caller, input: ClientLogin| {
                let accounts = c.accounts()?;
                let token = guarded(accounts, &input.name, |users| {
                    users.verify(&input.name, &input.password)?;
                    client_token(users, &input.name, &input.client, &accounts.roles())
                })?;
                Ok(json!({ "name": input.name, "token": token }))
            },
        ),
        operation(
            Spec {
                name: "token.join",
                summary: "Sign a client in with a join code, for an access token of its own.",
                access: Access::Anyone,
                input: JOIN,
            },
            |c: &HubContext, _: &Caller, input: Join| {
                let accounts = c.accounts()?;
                let (name, token) = accounts.change(|users| {
                    let name = users.spend(&input.code)?;
                    let token = client_token(users, &name, &input.client, &accounts.roles())?;
                    Ok((name, token))
                })?;
                Ok(json!({ "name": name, "token": token }))
            },
        ),
    ]
}

/// The models the connected runners share.
fn runner_models() -> Operation<HubContext> {
    operation(
            Spec {
                name: "runner.models",
                summary: "The models the connected runners share, each as a provider of type runner names it: what the hub's ANN pool may reach.",
                access: ADMIN,
                input: &[],
            },
            |c: &HubContext, _: &Caller, _: Nothing| match c.offered.get() {
                Some(offered) => Ok(json!({ "shared": offered() })),
                None => Err(Error::new(
                    Code::Unavailable,
                    "only the running hub knows which runners are connected; start it with `ensemble-hub serve`",
                )),
            },
        )
        .shown(|out| {
            let lines: Vec<String> = out["shared"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|s| {
                    let runner = s["runner"].as_str().unwrap_or_default().to_string();
                    let provider = s["provider"].as_str().unwrap_or_default().to_string();
                    let most = s["max_concurrent"].clone();
                    s["models"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .map(move |m| {
                            let kind = serde_json::from_value::<ModelKind>(m["kind"].clone())
                                .map(|k| k.describe())
                                .unwrap_or_default();
                            format!(
                                "{runner}  {provider}:{}  {kind}  ({most} call(s) at once)",
                                m["id"].as_str().unwrap_or_default()
                            )
                        })
                })
                .collect();
            Said {
                out: if lines.is_empty() {
                    "No connected runner shares a model.".into()
                } else {
                    lines.join("\n")
                },
                note: None,
            }
        })
}

/// A new access token of `name` for `client`, named after it, with the
/// account's whole role and no expiry.
fn client_token(
    users: &mut Users,
    name: &str,
    client: &str,
    roles: &Roles,
) -> Result<String, UsersError> {
    users
        .create_token(name, client, None, None, roles)
        .map(|(_, token, _)| token)
}

/// A new join code for `name`, and what to do with it.
fn join_code(c: &HubContext, name: &str) -> Result<Value, Error> {
    let (code, expires) = c.accounts()?.change(|u| u.join_code(name))?;
    Ok(json!({
        "name": name, "code": code, "expires": expires,
        "said": code, "note": join_note(c, name, &code),
    }))
}

/// Check a name and password, minding the lockout, and start a session.
fn login(accounts: &Accounts, input: &Login) -> Result<Value, Error> {
    let hours = accounts.rules().session_hours;
    let (token, expires) = guarded(accounts, &input.name, |users| {
        users.login(&input.name, &input.password, hours)
    })?;
    Ok(json!({ "name": input.name, "token": token, "expires": expires }))
}

/// Change the accounts with `check`, which checks the password of `name`,
/// minding the lockout: refused while the name is locked, and a wrong
/// password counted against it.
fn guarded<T>(
    accounts: &Accounts,
    name: &str,
    check: impl FnOnce(&mut Users) -> Result<T, UsersError>,
) -> Result<T, Error> {
    {
        let failures = accounts.failures.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, Some(until))) = failures.get(name)
            && Instant::now() < *until
        {
            return Err(Error::new(
                Code::TooMany,
                "too many failed logins for this name; try again later",
            ));
        }
    }
    match accounts.change(check) {
        Ok(out) => {
            accounts
                .failures
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(name);
            Ok(out)
        }
        Err(e) if e.code == Code::Unauthorized => {
            let mut failures = accounts.failures.lock().unwrap_or_else(|e| e.into_inner());
            let entry = failures.entry(name.to_string()).or_insert((0, None));
            entry.0 += 1;
            if entry.0 >= accounts.rules().lockout_failures {
                let minutes = u64::from(accounts.rules().lockout_minutes);
                *entry = (0, Some(Instant::now() + Duration::from_secs(minutes * 60)));
            }
            Err(e)
        }
        Err(e) => Err(e),
    }
}

pub(crate) fn names(permissions: &[Permission]) -> Vec<&'static str> {
    permissions.iter().map(|p| p.as_str()).collect()
}

/// An account as an interface shows it: never a password, a token or a hash.
pub(crate) fn view(user: &User) -> Value {
    json!({
        "name": user.name,
        "role": user.role,
        "status": user.status.as_str(),
        "registered": user.registered,
        "has_password": user.password.is_some(),
        "tracker_user": user.tracker_user,
        "tracker_name": user.tracker_name(),
        "tokens": user.tokens.iter().filter(|t| t.kind == Kind::Personal).count(),
    })
}

/// An access token as an interface lists it: never the token or its hash.
fn token_view(token: &Token) -> Value {
    json!({
        "id": token.id,
        "name": token.name,
        "scopes": token.scopes.as_deref().map(names),
        "made": token.made,
        "expires": token.expires,
    })
}
