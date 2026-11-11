//! A hub server's accounts, and the tokens that sign in as them. Accounts are
//! state, not config: they live in `users.json` in the hub's state directory,
//! which only the admin who runs the hub can read.
//!
//! An account has a name, a role and a password, kept as a PBKDF2-HMAC-SHA256
//! hash. Logging in with the password gives a **session** token, which lasts
//! the hub's `sessions.hours`. From a session, a person makes **access
//! tokens** for their nodes and scripts: each named, with an optional expiry,
//! and optional scopes that narrow what the account's role allows. A token is
//! shown once, when it is made; only its SHA-256 is kept.
//!
//! Someone who signs up is `pending`, and cannot log in until an admin
//! approves them with a role.

use ensemble_config::Permission;
use ring::digest::{SHA256, digest};
use ring::pbkdf2;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const USERS_FILE: &str = "users.json";
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
    /// Made from a session, for a node or a script.
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
    #[serde(default)]
    pub status: Status,
    /// When the account was made, once known.
    #[serde(default)]
    pub registered: String,
    /// None until a password is set.
    #[serde(default)]
    pub password: Option<Password>,
    pub tokens: Vec<Token>,
}

/// A token, as kept: its hash and what it may do. Never the token itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub sha256: String,
    pub made: String,
    /// For listing and revoking; tokens from before had none and get one from
    /// their hash.
    #[serde(default)]
    pub id: String,
    #[serde(default = "unnamed")]
    pub name: String,
    #[serde(default)]
    pub kind: Kind,
    /// What it may do, within the account's role; none is all of it.
    #[serde(default)]
    pub scopes: Option<Vec<Permission>>,
    /// When it stops working, in UTC; none is never.
    #[serde(default)]
    pub expires: Option<String>,
}

fn unnamed() -> String {
    "token".into()
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
    path: PathBuf,
    users: BTreeMap<String, User>,
}

pub type Roles = BTreeMap<String, Vec<Permission>>;

impl Users {
    /// The accounts kept in `dir`, none when there is no file yet.
    pub fn load(dir: &Path) -> Result<Users, UsersError> {
        let path = dir.join(USERS_FILE);
        let mut users: BTreeMap<String, User> = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| UsersError::Io(format!("{} is not readable: {e}", path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                return Err(UsersError::Io(format!(
                    "cannot read {}: {e}",
                    path.display()
                )));
            }
        };
        for token in users.values_mut().flat_map(|u| u.tokens.iter_mut()) {
            if token.id.is_empty() {
                token.id = token.sha256.chars().take(8).collect();
            }
        }
        Ok(Users { path, users })
    }

    /// Write the accounts, readable only by their owner, without the tokens
    /// that have expired.
    pub fn save(&mut self) -> Result<(), UsersError> {
        let now = now();
        for user in self.users.values_mut() {
            user.tokens
                .retain(|t| t.expires.as_ref().is_none_or(|e| *e > now));
        }
        let dir = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)
            .map_err(|e| UsersError::Io(format!("cannot create {}: {e}", dir.display())))?;
        let json =
            serde_json::to_string_pretty(&self.users).map_err(|e| UsersError::Io(e.to_string()))?;
        let tmp = self.path.with_extension("tmp");
        write_private(&tmp, &json)
            .and_then(|()| std::fs::rename(&tmp, &self.path))
            .map_err(|e| UsersError::Io(format!("cannot write {}: {e}", self.path.display())))
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
    if text.len() % 2 != 0 {
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

/// Write `text` to a new file only its owner can read.
pub(crate) fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(text.as_bytes())
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
        let file = std::fs::read_to_string(tmp.0.join(USERS_FILE)).unwrap();
        assert!(!file.contains(&ana[4..]), "no token is kept, only hashes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(tmp.0.join(USERS_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
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
        let file = std::fs::read_to_string(tmp.0.join(USERS_FILE)).unwrap();
        assert!(!file.contains(PASSWORD), "the password is never kept");
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

    #[test]
    fn accounts_and_tokens_from_before_still_work() {
        let tmp = TempDir::new();
        let mut users = Users::load(&tmp.0).unwrap();
        let token = users.add("ana", "member", &roles()).unwrap();
        users.save().unwrap();
        let path = tmp.0.join(USERS_FILE);
        let mut old: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let ana = old["ana"].as_object_mut().unwrap();
        for key in ["status", "registered", "password"] {
            ana.remove(key);
        }
        let t = ana["tokens"][0].as_object_mut().unwrap();
        for key in ["id", "name", "kind", "scopes", "expires"] {
            t.remove(key);
        }
        std::fs::write(&path, old.to_string()).unwrap();
        let users = Users::load(&tmp.0).unwrap();
        let signed = users.authenticate(&token, &roles()).unwrap();
        assert_eq!(signed.user.status, Status::Active);
        assert_eq!(signed.kind, Kind::Personal);
        assert_eq!(signed.token_id.len(), 8);
    }
}
