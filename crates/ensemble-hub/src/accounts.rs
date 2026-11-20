//! The hub's operations ([`operations`]): its accounts, their tokens and
//! sessions, and its settings file, for every interface to reach: the CLI on
//! the hub's computer, and the REST API.
//!
//! | Operation | Who |
//! |---|---|
//! | `user.list`, `user.show`, `user.add`, `user.approve`, `user.role`, `user.password`, `user.tracker`, `user.token`, `user.join-code`, `user.revoke`, `user.remove`, `role.list` | admin |
//! | `config.get`, `config.set`, `config.unset`, `config.path` | admin |
//! | `me.show` | any signed-in account |
//! | `me.tracker`, `me.password`, `me.tokens`, `me.token`, `me.join-code`, `me.revoke`, `account.logout` | a session |
//! | `runner.backends` | admin, of the running hub |
//! | `account.register`, `account.login`, `runner.login`, `runner.join` | anyone |
//!
//! A **session** comes from logging in with the password, and only a session
//! may change the password or make and revoke access tokens, so a stolen access
//! token cannot make more. An **access token** may be narrowed by scopes, and
//! is what runners, client agents and scripts use. A runner gets its own over
//! the link, which runs `runner.login` and `runner.join` for a connection
//! without a token: with a password, or with a **join code**, made by an admin
//! or by the account's session and spent once.

use crate::users::{JOIN_HOURS, Kind, Roles, Token, User, Users, UsersError};
use ensemble_backends::ops::{Backends, Side, WithBackends};
use ensemble_config::settings_file::{self, SettingsFile, WithSettings};
use ensemble_config::{Permission, Registration};
use ensemble_ops::{
    Access, Caller, Code, Error, Field, Kind as FieldKind, Operation, Registry, Said, Spec,
    operation,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

/// The hub's accounts, in its database, and what changing them needs.
pub struct Accounts {
    /// The hub's folder, with its database.
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

    fn rules(&self) -> ensemble_config::AccountRules {
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
    dir: PathBuf,
    /// Its accounts, made the first time an operation asks for them.
    accounts: OnceLock<Result<Accounts, String>>,
    /// Its settings, loaded: its roles and account rules, or why not.
    load: Box<Load>,
    settings: SettingsFile,
    /// What the running hub does once its settings changed, and says of it:
    /// set by the hub ([`HubContext::when_changed`]); nothing otherwise.
    changed: Arc<OnceLock<Box<Changed>>>,
    /// The backends the connected runners share, as the running hub lists
    /// them ([`HubContext::when_serving`]); nothing otherwise.
    offered: OnceLock<Box<Offered>>,
    /// The file variables are read from when one is given (`--env-file`), in
    /// place of its own `.env` ([`HubContext::keys_from`]).
    env_file: OnceLock<PathBuf>,
}

type Offered = dyn Fn() -> Value + Send + Sync;
type Load = dyn Fn() -> Result<(Roles, ensemble_config::AccountRules), String> + Send + Sync;
type Changed = dyn Fn() -> Option<String> + Send + Sync;

impl HubContext {
    /// The hub in `dir`: its accounts, from what `load` reads of its settings
    /// when an operation first needs them, and its settings, in its database.
    pub fn new(
        dir: &Path,
        load: impl Fn() -> Result<(Roles, ensemble_config::AccountRules), String>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        let checked = crate::kept::source(dir);
        let changed: Arc<OnceLock<Box<Changed>>> = Arc::default();
        let settings = SettingsFile::kept_in(
            crate::kept::settings(dir),
            crate::kept::SETTINGS_NAME,
            crate::kept::source(dir),
            "`ensemble-hub init`",
            move |text| {
                ensemble_config::parse_hub(text, &checked, &ensemble_config::SystemEnv)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            },
        )
        .on_change({
            let changed = changed.clone();
            move || changed.get().and_then(|apply| apply())
        });
        HubContext {
            dir: dir.to_path_buf(),
            accounts: OnceLock::new(),
            load: Box::new(load),
            settings,
            changed,
            offered: OnceLock::new(),
            env_file: OnceLock::new(),
        }
    }

    /// Read variables from `given` (`--env-file`) in place of its own `.env`.
    pub fn keys_from(&self, given: Option<PathBuf>) {
        if let Some(file) = given {
            let _ = self.env_file.set(file);
        }
    }

    /// The file variables are read from now, if there is one.
    fn keys_file(&self) -> Option<PathBuf> {
        ensemble_ops::cli::env_file(self.env_file.get().map(PathBuf::as_path), &self.dir)
    }

    /// Its settings as they are now, with the keys they name.
    pub(crate) fn hub(&self) -> Result<ensemble_config::Hub, Error> {
        crate::kept::load(&self.dir, self.keys_file().as_deref()).map_err(|e| {
            Error::new(Code::Unavailable, e)
                .hint("fix the hub's settings with `ensemble-hub config`")
        })
    }

    /// Have `runner.backends` list what `offered` returns: the backends the
    /// runners connected to the running hub share.
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
            (self.load)().map(|(roles, rules)| Accounts::new(&self.dir, roles, rules))
        });
        made.as_ref().map_err(|why| {
            Error::new(Code::Unavailable, format!("the hub's accounts: {why}"))
                .hint("fix the hub's settings with `ensemble-hub config`")
        })
    }
}

impl WithSettings for HubContext {
    fn settings_file(&self) -> Result<&SettingsFile, Error> {
        Ok(&self.settings)
    }
}

/// Its keys are kept sealed in its database ([`crate::kept`]).
impl WithBackends for HubContext {
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
        crate::kept::variables(&self.dir, self.keys_file().as_deref())
            .ok()?
            .get(name)
    }

    fn backends(&self) -> Result<Backends, Error> {
        let hub = self.hub()?;
        Ok(Backends {
            direct: hub.llm.direct,
            relayed: hub.llm.runners,
            shares: None,
        })
    }
}

/// Every operation of the hub.
pub fn operations() -> Registry<HubContext> {
    let mut all = settings_file::operations::<HubContext>(Access::Permission("admin"));
    all.extend(user_operations());
    all.extend(own_operations());
    all.extend(runner_operations());
    all.extend(ensemble_backends::ops::operations::<HubContext>(
        Side::Hub,
        Access::Permission("admin"),
    ));
    all.extend(crate::goals::operations());
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
    let mut note = format!(
        "This is {name}'s token, shown once. On their computer: ensemble-runner login {} --token <token>",
        hub_url(c)
    );
    note.push_str(&format!(
        ". For their agent, as its API key for the hub's MCP: {}",
        mcp_line(c, token)
    ));
    note
}

/// What a new join code's owner is told.
fn join_note(c: &HubContext, name: &str, code: &str) -> String {
    format!(
        "This is {name}'s join code, shown once and good for one runner within {JOIN_HOURS} hours. On the runner's computer: ensemble-runner join {} {code}",
        hub_url(c)
    )
}

/// Its settings as they are now, when they load.
fn hub_file(c: &HubContext) -> Option<ensemble_config::Hub> {
    let text = c.settings.read().ok()?;
    ensemble_config::parse_hub(
        &text,
        &crate::kept::source(&c.dir),
        &ensemble_config::SystemEnv,
    )
    .ok()
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
    match hub_file(c) {
        Some(hub) if !hub.http.listen.ip().is_unspecified() => {
            format!("http://{}", hub.http.listen)
        }
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
                summary: "Make an account a join code, shown once, that signs one runner in.",
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
        FieldKind::List,
        "What it may do, within your role: share, recall, tasks, admin, compute, host.",
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
                                "`scopes` must be a list of share, recall, tasks, admin, compute or host",
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
                summary: "Make yourself a join code, shown once, that signs one runner in.",
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
struct RunnerLogin {
    name: String,
    password: String,
    runner: String,
}

#[derive(Deserialize)]
struct Join {
    code: String,
    runner: String,
}

const RUNNER: Field = Field::text(
    "runner",
    "The runner's name, which its new access token is named after.",
)
.required();
const RUNNER_LOGIN: &[Field] = &[
    Field::text("name", "Your account's name.").required(),
    Field::secret("password", "Your password.").required(),
    RUNNER,
];
const JOIN: &[Field] = &[
    Field::text("code", "A join code made for your account.").required(),
    RUNNER,
];

/// How a runner signs in: over the link, on a connection without a token.
fn runner_operations() -> Vec<Operation<HubContext>> {
    vec![
        operation(
            Spec {
                name: "runner.login",
                summary: "Sign a runner in with a password, for an access token of its own.",
                access: Access::Anyone,
                input: RUNNER_LOGIN,
            },
            |c: &HubContext, _: &Caller, input: RunnerLogin| {
                let accounts = c.accounts()?;
                let token = guarded(accounts, &input.name, |users| {
                    users.verify(&input.name, &input.password)?;
                    runner_token(users, &input.name, &input.runner, &accounts.roles())
                })?;
                Ok(json!({ "name": input.name, "token": token }))
            },
        ),
        operation(
            Spec {
                name: "runner.join",
                summary: "Sign a runner in with a join code, for an access token of its own.",
                access: Access::Anyone,
                input: JOIN,
            },
            |c: &HubContext, _: &Caller, input: Join| {
                let accounts = c.accounts()?;
                let (name, token) = accounts.change(|users| {
                    let name = users.spend(&input.code)?;
                    let token = runner_token(users, &name, &input.runner, &accounts.roles())?;
                    Ok((name, token))
                })?;
                Ok(json!({ "name": name, "token": token }))
            },
        ),
        operation(
            Spec {
                name: "runner.backends",
                summary: "The backends the connected runners share, with their models: what the hub's `llm.runners` may name.",
                access: ADMIN,
                input: &[],
            },
            |c: &HubContext, _: &Caller, _: Nothing| match c.offered.get() {
                Some(offered) => Ok(json!({ "backends": offered() })),
                None => Err(Error::new(
                    Code::Unavailable,
                    "only the running hub knows which runners are connected; start it with `ensemble-hub serve`",
                )),
            },
        )
        .shown(|out| {
            let lines: Vec<String> = out["backends"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|b| {
                    let models: Vec<&str> = b["models"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    format!(
                        "{}  {}  (works for {}, {} call(s) at once)  models: {}",
                        b["runner"].as_str().unwrap_or(""),
                        b["backend"].as_str().unwrap_or(""),
                        b["works_for"].as_str().unwrap_or(""),
                        b["max_calls"],
                        if models.is_empty() {
                            "-".to_string()
                        } else {
                            models.join(", ")
                        }
                    )
                })
                .collect();
            Said {
                out: if lines.is_empty() {
                    "No connected runner shares a backend.".into()
                } else {
                    lines.join("\n")
                },
                note: None,
            }
        }),
    ]
}

/// A new access token of `name` for the runner `runner`, named after it,
/// with the account's whole role and no expiry.
fn runner_token(
    users: &mut Users,
    name: &str,
    runner: &str,
    roles: &Roles,
) -> Result<String, UsersError> {
    let label = format!("runner {}", runner.trim());
    users
        .create_token(name, &label, None, None, roles)
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
