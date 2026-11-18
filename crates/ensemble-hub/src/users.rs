//! A hub server's accounts, and the tokens that sign in as them. Accounts are
//! state, not config: they live in the hub's database (`store`) in its state
//! directory, which only the admin who runs the hub can read.
//!
//! An account has a name, a role and a password, kept as a PBKDF2-HMAC-SHA256
//! hash. Logging in with the password gives a **session** token, which lasts
//! the hub's `sessions.hours`. From a session, a person makes **access
//! tokens** for their runners, agents and scripts: each named, with an optional expiry,
//! and optional scopes that narrow what the account's role allows. A token is
//! shown once, when it is made; only its SHA-256 is kept.
//!
//! Someone who signs up is `pending`, and cannot log in until an admin
//! approves them with a role.

use ensemble_config::Permission;
use ensemble_store::{self as store, Value};
use ring::digest::{SHA256, digest};
use ring::pbkdf2;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The shortest password an account may have.
pub const MIN_PASSWORD: usize = 12;
/// PBKDF2 rounds for a new password hash: OWASP's figure for HMAC-SHA256.
const ROUNDS: u32 = 600_000;

/// Where an account stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Signed up, waiting for an admin.
    Pending,
    /// Approved, with a role. Accounts from before sign-up existed are this.
    #[default]
    Active,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Active => "active",
        }
    }
}

/// What a token is for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// From a login: proves the password, expires after `sessions.hours`.
    Session,
    /// Made from a session, for a runner, an agent or a script.
    #[default]
    Personal,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Session => "session",
            Kind::Personal => "personal",
        }
    }
}

/// A password, as kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Password {
    pub rounds: u32,
    pub salt: String,
    pub hash: String,
}

/// One account of a hub server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub name: String,
    /// One of the hub's `roles`; empty while pending.
    pub role: String,
    pub status: Status,
    /// When the account was made, once known.
    pub registered: String,
    /// None until a password is set.
    pub password: Option<Password>,
    /// Who the account is in the task tracker, when that differs from its
    /// name.
    pub tracker_user: Option<String>,
    pub tokens: Vec<Token>,
}

impl User {
    /// Who the account is in the task tracker: its `tracker_user`, or its name.
    pub fn tracker_name(&self) -> &str {
        self.tracker_user.as_deref().unwrap_or(&self.name)
    }
}

/// A token, as kept: its hash and what it may do. Never the token itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub sha256: String,
    pub made: String,
    /// For listing and revoking.
    pub id: String,
    pub name: String,
    pub kind: Kind,
    /// What it may do, within the account's role; none is all of it.
    pub scopes: Option<Vec<Permission>>,
    /// When it stops working, in UTC; none is never.
    pub expires: Option<String>,
}

/// Who a token signs in as, and what it may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    pub user: User,
    pub token_id: String,
    pub kind: Kind,
    /// The role's permissions, narrowed by the token's scopes.
    pub permissions: Vec<Permission>,
}

/// Why a change or a sign-in was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsersError {
    /// No account by that name, or no token by that id.
    NotFound(String),
    /// A name taken, a queue full, an account in the wrong state.
    Conflict(String),
    /// A name, role, password or scope that cannot be.
    Invalid(String),
    /// A wrong name or password.
    Unauthorized(String),
    /// The right password, but the account may not sign in yet.
    Forbidden(String),
    /// The file could not be read or written.
    Io(String),
}

impl fmt::Display for UsersError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UsersError::NotFound(m)
            | UsersError::Conflict(m)
            | UsersError::Invalid(m)
            | UsersError::Unauthorized(m)
            | UsersError::Forbidden(m)
            | UsersError::Io(m) => f.write_str(m),
        }
    }
}

impl From<UsersError> for String {
    fn from(e: UsersError) -> String {
        e.to_string()
    }
}

/// The accounts of one hub server.
pub struct Users {
    /// The hub's state directory, with its database.
    dir: PathBuf,
    users: BTreeMap<String, User>,
}

pub type Roles = BTreeMap<String, Vec<Permission>>;

impl Users {
    /// The accounts kept in `dir`, none when there are none yet.
    pub fn load(dir: &Path) -> Result<Users, UsersError> {
        let users = store::with(dir, async |c| read(c).await).map_err(UsersError::Io)?;
        Ok(Users {
            dir: dir.to_path_buf(),
            users,
        })
    }

    /// Write the accounts, in one transaction, without the tokens that have
    /// expired.
    pub fn save(&mut self) -> Result<(), UsersError> {
        let now = now();
        for user in self.users.values_mut() {
            user.tokens
                .retain(|t| t.expires.as_ref().is_none_or(|e| *e > now));
        }
        let statements = statements(&self.users);
        store::with(&self.dir, async |c| store::transaction(c, statements).await)
            .map_err(UsersError::Io)
    }

    pub fn list(&self) -> impl Iterator<Item = &User> {
        self.users.values()
    }

    pub fn get(&self, name: &str) -> Option<&User> {
        self.users.get(name)
    }

    /// Add an active account with `role`, one of `roles`, and return a first
    /// access token for it: what the admin does on the server.
    pub fn add(&mut self, name: &str, role: &str, roles: &Roles) -> Result<String, UsersError> {
        let name = new_name(name)?;
        self.free(&name)?;
        known_role(role, roles)?;
        self.insert(&name, role, Status::Active, None);
        self.token(&name)
    }

    /// Add an active account with a password: what an admin does through the
    /// API.
    pub fn create(
        &mut self,
        name: &str,
        role: &str,
        password: &str,
        roles: &Roles,
    ) -> Result<(), UsersError> {
        let name = new_name(name)?;
        self.free(&name)?;
        known_role(role, roles)?;
        let password = hash_password(password)?;
        self.insert(&name, role, Status::Active, Some(password));
        Ok(())
    }

    /// Sign `name` up with `password`: a pending account that can log in once
    /// an admin approves it. Refused while `max_pending` sign-ups wait.
    pub fn register(
        &mut self,
        name: &str,
        password: &str,
        max_pending: u32,
    ) -> Result<(), UsersError> {
        let name = new_name(name)?;
        self.free(&name)?;
        let password = hash_password(password)?;
        let waiting = self
            .users
            .values()
            .filter(|u| u.status == Status::Pending)
            .count();
        if waiting >= max_pending as usize {
            return Err(UsersError::Conflict(
                "too many sign-ups wait for approval; try again later".into(),
            ));
        }
        self.insert(&name, "", Status::Pending, Some(password));
        Ok(())
    }

    /// Set an account's password, and end its sessions.
    pub fn set_password(&mut self, name: &str, password: &str) -> Result<(), UsersError> {
        let hashed = hash_password(password)?;
        let user = self.user_mut(name)?;
        user.password = Some(hashed);
        user.tokens.retain(|t| t.kind != Kind::Session);
        Ok(())
    }

    /// Check `password` and return a session token and when it expires.
    pub fn login(
        &mut self,
        name: &str,
        password: &str,
        hours: u32,
    ) -> Result<(String, String), UsersError> {
        let wrong = || UsersError::Unauthorized("wrong name or password".into());
        let user = self.users.get(name).ok_or_else(wrong)?;
        let kept = user.password.as_ref().ok_or_else(wrong)?;
        if !verify_password(kept, password) {
            return Err(wrong());
        }
        if user.status != Status::Active {
            return Err(UsersError::Forbidden(
                "your sign-up waits for an admin's approval".into(),
            ));
        }
        let expires = later(Duration::from_secs(u64::from(hours) * 3600));
        let (_, token) = self.mint(name, "login", Kind::Session, None, Some(expires.clone()))?;
        Ok((token, expires))
    }

    /// Approve a pending account with `role`.
    pub fn approve(&mut self, name: &str, role: &str, roles: &Roles) -> Result<(), UsersError> {
        known_role(role, roles)?;
        let user = self.user_mut(name)?;
        if user.status != Status::Pending {
            return Err(UsersError::Conflict(format!(
                "{name} is not waiting for approval"
            )));
        }
        user.status = Status::Active;
        user.role = role.to_string();
        Ok(())
    }

    /// Say who an account is in the task tracker; none makes it its name.
    pub fn set_tracker_user(
        &mut self,
        name: &str,
        tracker_user: Option<&str>,
    ) -> Result<(), UsersError> {
        let tracker_user = match tracker_user.map(str::trim) {
            None | Some("") => None,
            Some(t) if t.len() > 128 || t.contains(char::is_control) => {
                return Err(UsersError::Invalid(
                    "`tracker_user` must be a name of at most 128 characters".into(),
                ));
            }
            Some(t) => Some(t.to_string()),
        };
        if let Some(t) = &tracker_user
            && let Some(other) = self
                .users
                .values()
                .find(|u| u.name != name && u.tracker_name() == t)
        {
            return Err(UsersError::Conflict(format!(
                "{} is {t} in the tracker already",
                other.name
            )));
        }
        self.user_mut(name)?.tracker_user = tracker_user;
        Ok(())
    }

    /// The active account that is `tracker_name` in the task tracker.
    pub fn assigned_to(&self, tracker_name: &str) -> Option<&User> {
        self.users
            .values()
            .find(|u| u.status == Status::Active && u.tracker_name() == tracker_name)
    }

    /// Give an active account another role.
    pub fn set_role(&mut self, name: &str, role: &str, roles: &Roles) -> Result<(), UsersError> {
        known_role(role, roles)?;
        let user = self.user_mut(name)?;
        if user.status != Status::Active {
            return Err(UsersError::Conflict(format!(
                "{name} waits for approval; approve them with a role"
            )));
        }
        user.role = role.to_string();
        Ok(())
    }

    /// A new access token for `name`, with the account's whole role and no
    /// expiry: what the admin's CLI hands out.
    pub fn token(&mut self, name: &str) -> Result<String, UsersError> {
        self.mint(name, "cli", Kind::Personal, None, None)
            .map(|(_, t)| t)
    }

    /// A new access token for `name`, called `label`, expiring after
    /// `expires_days` when given, and allowed only `scopes` when given, which
    /// must be within the account's role. Returns its id, the token, and when
    /// it expires.
    pub fn create_token(
        &mut self,
        name: &str,
        label: &str,
        expires_days: Option<u32>,
        scopes: Option<Vec<Permission>>,
        roles: &Roles,
    ) -> Result<(String, String, Option<String>), UsersError> {
        let label = label.trim();
        if label.is_empty() || label.len() > 64 {
            return Err(UsersError::Invalid(
                "a token needs a name of 1 to 64 characters, such as laptop".into(),
            ));
        }
        let user = self.user_mut(name)?;
        let allowed = roles.get(&user.role).cloned().unwrap_or_default();
        if let Some(scopes) = &scopes {
            if scopes.is_empty() {
                return Err(UsersError::Invalid(
                    "`scopes` is empty; leave it out for the whole role".into(),
                ));
            }
            if let Some(extra) = scopes.iter().find(|s| !allowed.contains(s)) {
                return Err(UsersError::Invalid(format!(
                    "your role does not allow `{}`, so no token of yours can",
                    extra.as_str()
                )));
            }
        }
        if expires_days == Some(0) {
            return Err(UsersError::Invalid(
                "`expires_days` must be greater than 0; leave it out for no expiry".into(),
            ));
        }
        let expires = expires_days.map(|d| later(Duration::from_secs(u64::from(d) * 24 * 3600)));
        let (id, token) = self.mint(name, label, Kind::Personal, scopes, expires.clone())?;
        Ok((id, token, expires))
    }

    /// An account's access tokens, without sessions; never their hashes.
    pub fn tokens(&self, name: &str) -> Result<Vec<&Token>, UsersError> {
        let user = self
            .users
            .get(name)
            .ok_or_else(|| UsersError::NotFound(format!("no user called {name}")))?;
        let now = now();
        Ok(user
            .tokens
            .iter()
            .filter(|t| t.kind == Kind::Personal)
            .filter(|t| t.expires.as_ref().is_none_or(|e| *e > now))
            .collect())
    }

    /// Revoke one token of `name`, by id.
    pub fn revoke_token(&mut self, name: &str, id: &str) -> Result<(), UsersError> {
        let user = self.user_mut(name)?;
        let before = user.tokens.len();
        user.tokens.retain(|t| t.id != id);
        if user.tokens.len() == before {
            return Err(UsersError::NotFound(format!("{name} has no token {id}")));
        }
        Ok(())
    }

    /// Revoke every token of `name`, and say how many there were.
    pub fn revoke(&mut self, name: &str) -> Result<usize, UsersError> {
        Ok(std::mem::take(&mut self.user_mut(name)?.tokens).len())
    }

    /// Remove an account, or turn down a sign-up.
    pub fn remove(&mut self, name: &str) -> Result<(), UsersError> {
        self.users
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| UsersError::NotFound(format!("no user called {name}")))
    }

    /// Who `token` signs in as, and what it may do, when its account is active
    /// and it has not expired.
    pub fn authenticate(&self, token: &str, roles: &Roles) -> Option<Signed> {
        let (user, t) = self.find(token)?;
        if user.status != Status::Active {
            return None;
        }
        let role = roles.get(&user.role).cloned().unwrap_or_default();
        let permissions = match &t.scopes {
            Some(scopes) => role.into_iter().filter(|p| scopes.contains(p)).collect(),
            None => role,
        };
        Some(Signed {
            user: user.clone(),
            token_id: t.id.clone(),
            kind: t.kind,
            permissions,
        })
    }

    /// The account `token` belongs to, pending or active, unless it expired.
    pub fn identify(&self, token: &str) -> Option<&User> {
        self.find(token).map(|(u, _)| u)
    }

    fn find(&self, token: &str) -> Option<(&User, &Token)> {
        let hash = sha256(token.trim());
        let now = now();
        // Every hash is compared, all the way, so the time taken says nothing.
        let mut found = None;
        for user in self.users.values() {
            for t in &user.tokens {
                if same(t.sha256.as_bytes(), hash.as_bytes()) {
                    found = Some((user, t));
                }
            }
        }
        found.filter(|(_, t)| t.expires.as_ref().is_none_or(|e| *e > now))
    }

    fn mint(
        &mut self,
        name: &str,
        label: &str,
        kind: Kind,
        scopes: Option<Vec<Permission>>,
        expires: Option<String>,
    ) -> Result<(String, String), UsersError> {
        let token = format!("ens_{}", hex(&random::<32>()?));
        let id = hex(&random::<4>()?);
        self.user_mut(name)?.tokens.push(Token {
            sha256: sha256(&token),
            made: now(),
            id: id.clone(),
            name: label.to_string(),
            kind,
            scopes,
            expires,
        });
        Ok((id, token))
    }

    fn insert(&mut self, name: &str, role: &str, status: Status, password: Option<Password>) {
        self.users.insert(
            name.to_string(),
            User {
                name: name.to_string(),
                role: role.to_string(),
                status,
                registered: now(),
                password,
                tracker_user: None,
                tokens: Vec::new(),
            },
        );
    }

    fn free(&self, name: &str) -> Result<(), UsersError> {
        if self.users.contains_key(name) {
            Err(UsersError::Conflict(format!("the name {name} is taken")))
        } else {
            Ok(())
        }
    }

    fn user_mut(&mut self, name: &str) -> Result<&mut User, UsersError> {
        self.users
            .get_mut(name)
            .ok_or_else(|| UsersError::NotFound(format!("no user called {name}")))
    }
}

/// An account name: one word of letters, digits, `.`, `_` or `-`, at most 64.
fn new_name(name: &str) -> Result<String, UsersError> {
    let name = name.trim();
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || "._-".contains(c));
    if ok {
        Ok(name.to_string())
    } else {
        Err(UsersError::Invalid(format!(
            "'{name}' must be one word of letters, digits, '.', '_' or '-', such as ana"
        )))
    }
}

fn known_role(role: &str, roles: &Roles) -> Result<(), UsersError> {
    if roles.contains_key(role) {
        return Ok(());
    }
    let known: Vec<&str> = roles.keys().map(String::as_str).collect();
    Err(UsersError::Invalid(format!(
        "the hub has no role '{role}'. Its roles: {}.",
        known.join(", ")
    )))
}

fn hash_password(password: &str) -> Result<Password, UsersError> {
    if password.chars().count() < MIN_PASSWORD {
        return Err(UsersError::Invalid(format!(
            "a password needs at least {MIN_PASSWORD} characters"
        )));
    }
    let salt = random::<16>()?;
    let mut hash = [0u8; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(ROUNDS).expect("not zero"),
        &salt,
        password.as_bytes(),
        &mut hash,
    );
    Ok(Password {
        rounds: ROUNDS,
        salt: hex(&salt),
        hash: hex(&hash),
    })
}

fn verify_password(kept: &Password, password: &str) -> bool {
    let (Some(salt), Some(hash), Some(rounds)) = (
        unhex(&kept.salt),
        unhex(&kept.hash),
        NonZeroU32::new(kept.rounds),
    ) else {
        return false;
    };
    pbkdf2::verify(
        pbkdf2::PBKDF2_HMAC_SHA256,
        rounds,
        &salt,
        password.as_bytes(),
        &hash,
    )
    .is_ok()
}

fn random<const N: usize>() -> Result<[u8; N], UsersError> {
    let mut bytes = [0u8; N];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| UsersError::Io("the system gave no random bytes".into()))?;
    Ok(bytes)
}

/// Every account in the database, with its tokens in the order they were made.
async fn read(c: &store::Connection) -> Result<BTreeMap<String, User>, String> {
    let text = |v: &Value| store::text(v).unwrap_or_default();
    let mut users = BTreeMap::new();
    let rows = store::rows(
        c,
        "SELECT name, role, status, registered, password_rounds, password_salt, password_hash, tracker_user FROM users",
        vec![],
    )
    .await?;
    for r in rows {
        let password = match (&r[4], store::text(&r[5]), store::text(&r[6])) {
            (Value::Integer(rounds), Some(salt), Some(hash)) => Some(Password {
                rounds: u32::try_from(*rounds).map_err(|e| e.to_string())?,
                salt,
                hash,
            }),
            _ => None,
        };
        let name = text(&r[0]);
        users.insert(
            name.clone(),
            User {
                name,
                role: text(&r[1]),
                status: match text(&r[2]).as_str() {
                    "pending" => Status::Pending,
                    _ => Status::Active,
                },
                registered: text(&r[3]),
                password,
                tracker_user: store::text(&r[7]),
                tokens: Vec::new(),
            },
        );
    }
    let rows = store::rows(
        c,
        "SELECT user, sha256, id, name, kind, scopes, expires, made FROM tokens ORDER BY rowid",
        vec![],
    )
    .await?;
    for r in rows {
        let scopes = match store::text(&r[5]) {
            Some(json) => Some(serde_json::from_str(&json).map_err(|e| e.to_string())?),
            None => None,
        };
        if let Some(user) = users.get_mut(&text(&r[0])) {
            user.tokens.push(Token {
                sha256: text(&r[1]),
                id: text(&r[2]),
                name: text(&r[3]),
                kind: match text(&r[4]).as_str() {
                    "session" => Kind::Session,
                    _ => Kind::Personal,
                },
                scopes,
                expires: store::text(&r[6]),
                made: text(&r[7]),
            });
        }
    }
    Ok(users)
}

/// What makes the database hold exactly `users`.
fn statements(users: &BTreeMap<String, User>) -> Vec<(&'static str, Vec<Value>)> {
    let t = |s: &str| Value::Text(s.to_string());
    let mut out = vec![
        ("DELETE FROM tokens", vec![]),
        ("DELETE FROM users", vec![]),
    ];
    for user in users.values() {
        let p = user.password.as_ref();
        out.push((
            "INSERT INTO users (name, role, status, registered, password_rounds, password_salt, password_hash, tracker_user) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            vec![
                t(&user.name),
                t(&user.role),
                t(user.status.as_str()),
                t(&user.registered),
                p.map_or(Value::Null, |p| Value::Integer(i64::from(p.rounds))),
                store::optional(p.map(|p| p.salt.as_str())),
                store::optional(p.map(|p| p.hash.as_str())),
                store::optional(user.tracker_user.as_deref()),
            ],
        ));
        for token in &user.tokens {
            let scopes = token
                .scopes
                .as_ref()
                .map(|s| serde_json::to_string(s).unwrap_or_default());
            out.push((
                "INSERT INTO tokens (sha256, user, id, name, kind, scopes, expires, made) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                vec![
                    t(&token.sha256),
                    t(&user.name),
                    t(&token.id),
                    t(&token.name),
                    t(token.kind.as_str()),
                    store::optional(scopes.as_deref()),
                    store::optional(token.expires.as_deref()),
                    t(&token.made),
                ],
            ));
        }
    }
    out
}

fn now() -> String {
    ensemble_core::time::utc(SystemTime::now())
}

/// The UTC time `after` from now.
fn later(after: Duration) -> String {
    ensemble_core::time::utc(SystemTime::now() + after)
}

fn sha256(token: &str) -> String {
    hex(digest(&SHA256, token.as_bytes()).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Equal, compared in time that does not depend on where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_core::workspace::testing::TempDir;

    fn roles() -> Roles {
        BTreeMap::from([
            (
                "member".to_string(),
                vec![Permission::Share, Permission::Recall],
            ),
            ("reader".to_string(), vec![Permission::Recall]),
        ])
    }

    const PASSWORD: &str = "correct horse battery";

    #[test]
    fn a_token_signs_in_as_its_account_until_it_is_revoked() {
        let tmp = TempDir::new();
        let mut users = Users::load(&tmp.0).unwrap();
        let ana = users.add("ana", "member", &roles()).unwrap();
        let second = users.token("ana").unwrap();
        assert!(ana.starts_with("ens_") && ana.len() == 4 + 64);
        assert_ne!(ana, second);
        users.save().unwrap();

        let users = Users::load(&tmp.0).unwrap();
        let signed = users.authenticate(&ana, &roles()).unwrap();
        assert_eq!(signed.user.name, "ana");
        assert_eq!(
            signed.permissions,
            vec![Permission::Share, Permission::Recall]
        );
        assert!(users.authenticate("ens_nope", &roles()).is_none());
        let kept = String::from_utf8_lossy(&std::fs::read(tmp.0.join(store::DB_FILE)).unwrap())
            .into_owned();
        assert!(!kept.contains(&ana[4..]), "no token is kept, only hashes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&tmp.0).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "only the hub's owner may enter");
        }

        let mut users = users;
        assert_eq!(users.revoke("ana").unwrap(), 2);
        assert!(users.authenticate(&ana, &roles()).is_none());
        users.remove("ana").unwrap();
        assert!(users.get("ana").is_none());
    }

    #[test]
    fn an_account_needs_a_new_name_a_known_role_and_a_long_password() {
        let tmp = TempDir::new();
        let mut users = Users::load(&tmp.0).unwrap();
        users.add("ana", "member", &roles()).unwrap();
        assert!(matches!(
            users.add("ana", "reader", &roles()),
            Err(UsersError::Conflict(_))
        ));
        let err = users.add("bob", "owner", &roles()).unwrap_err().to_string();
        assert!(
            err.contains("no role 'owner'") && err.contains("member, reader"),
            "{err}"
        );
        assert!(matches!(
            users.add("two words", "member", &roles()),
            Err(UsersError::Invalid(_))
        ));
        assert!(matches!(users.token("carol"), Err(UsersError::NotFound(_))));
        let short = users.register("dan", "short", 5).unwrap_err().to_string();
        assert!(short.contains("at least 12 characters"), "{short}");
    }

    #[test]
    fn the_password_is_kept_as_a_hash_and_logs_in_once_approved() {
        let tmp = TempDir::new();
        let mut users = Users::load(&tmp.0).unwrap();
        users.register("ana", PASSWORD, 2).unwrap();
        users.save().unwrap();
        let kept = String::from_utf8_lossy(&std::fs::read(tmp.0.join(store::DB_FILE)).unwrap())
            .into_owned();
        assert!(!kept.contains(PASSWORD), "the password is never kept");
        let mut users = Users::load(&tmp.0).unwrap();
        assert!(matches!(
            users.login("ana", PASSWORD, 1),
            Err(UsersError::Forbidden(_))
        ));
        users.approve("ana", "member", &roles()).unwrap();
        assert!(matches!(
            users.login("ana", "wrong password!!", 1),
            Err(UsersError::Unauthorized(_))
        ));
        assert!(matches!(
            users.login("nobody", PASSWORD, 1),
            Err(UsersError::Unauthorized(_))
        ));
        let (session, expires) = users.login("ana", PASSWORD, 1).unwrap();
        let signed = users.authenticate(&session, &roles()).unwrap();
        assert_eq!(signed.kind, Kind::Session);
        assert!(expires > now());
        assert!(
            users.tokens("ana").unwrap().is_empty(),
            "sessions are not listed"
        );

        users.set_password("ana", "another long secret").unwrap();
        assert!(
            users.authenticate(&session, &roles()).is_none(),
            "sessions end"
        );
        assert!(users.login("ana", PASSWORD, 1).is_err());
        assert!(users.login("ana", "another long secret", 1).is_ok());
    }

    #[test]
    fn an_access_token_is_named_scoped_within_the_role_and_may_expire() {
        let tmp = TempDir::new();
        let mut users = Users::load(&tmp.0).unwrap();
        users.create("ana", "member", PASSWORD, &roles()).unwrap();
        let (id, token, expires) = users
            .create_token(
                "ana",
                "laptop",
                Some(30),
                Some(vec![Permission::Recall]),
                &roles(),
            )
            .unwrap();
        assert!(expires.is_some());
        let signed = users.authenticate(&token, &roles()).unwrap();
        assert_eq!(signed.permissions, vec![Permission::Recall]);
        assert_eq!(signed.kind, Kind::Personal);
        let listed = users.tokens("ana").unwrap();
        assert_eq!(
            (listed[0].id.as_str(), listed[0].name.as_str()),
            (id.as_str(), "laptop")
        );

        let err = users
            .create_token("ana", "x", None, Some(vec![Permission::Admin]), &roles())
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not allow `admin`"), "{err}");
        assert!(
            users
                .create_token("ana", "x", Some(0), None, &roles())
                .is_err()
        );

        users.revoke_token("ana", &id).unwrap();
        assert!(users.authenticate(&token, &roles()).is_none());
        assert!(matches!(
            users.revoke_token("ana", &id),
            Err(UsersError::NotFound(_))
        ));
    }

    #[test]
    fn an_expired_token_signs_in_to_nothing_and_is_dropped_on_save() {
        let tmp = TempDir::new();
        let mut users = Users::load(&tmp.0).unwrap();
        let token = users.add("ana", "member", &roles()).unwrap();
        users.users.get_mut("ana").unwrap().tokens[0].expires = Some("2000-01-01T00:00:00Z".into());
        assert!(users.authenticate(&token, &roles()).is_none());
        users.save().unwrap();
        assert!(users.get("ana").unwrap().tokens.is_empty());
    }

    #[test]
    fn an_account_is_found_by_its_tracker_name_or_its_own() {
        let tmp = TempDir::new();
        let mut users = Users::load(&tmp.0).unwrap();
        users.add("ana", "member", &roles()).unwrap();
        users.add("bob", "member", &roles()).unwrap();
        assert_eq!(users.assigned_to("ana").unwrap().name, "ana");
        users
            .set_tracker_user("ana", Some("ana.petrosyan"))
            .unwrap();
        assert_eq!(users.assigned_to("ana.petrosyan").unwrap().name, "ana");
        assert!(users.assigned_to("ana").is_none());
        assert!(matches!(
            users.set_tracker_user("bob", Some("ana.petrosyan")),
            Err(UsersError::Conflict(_))
        ));
        users.set_tracker_user("ana", None).unwrap();
        assert_eq!(users.assigned_to("ana").unwrap().name, "ana");
        users.register("carol", PASSWORD, 5).unwrap();
        assert!(
            users.assigned_to("carol").is_none(),
            "a pending account gets no tasks"
        );
    }

    #[test]
    fn sign_ups_wait_at_most_max_pending_and_take_no_name_twice() {
        let tmp = TempDir::new();
        let mut users = Users::load(&tmp.0).unwrap();
        users.register("ana", PASSWORD, 2).unwrap();
        assert!(matches!(
            users.register("ana", PASSWORD, 2),
            Err(UsersError::Conflict(_))
        ));
        users.register("bob", PASSWORD, 2).unwrap();
        let full = users
            .register("carol", PASSWORD, 2)
            .unwrap_err()
            .to_string();
        assert!(full.contains("too many sign-ups"), "{full}");
        users.remove("bob").unwrap();
        assert!(users.register("carol", PASSWORD, 2).is_ok());
    }
}
