//! A hub server's users, and the tokens their nodes sign in with. Users are
//! state, not config: they live in `users.json` in the hub's state directory,
//! which only the admin who runs the hub can read. A token is shown once, when
//! it is made, and only its SHA-256 is kept, so the file gives nobody a way in.
//!
//! Someone who signs up is `pending`: they get their token at once, and it
//! signs in to nothing until an admin approves them and gives them a role.

use ensemble_config::Permission;
use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

const USERS_FILE: &str = "users.json";

/// Where a user stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Signed up, waiting for an admin.
    Pending,
    /// Approved, with a role. Users from before sign-up existed are this.
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

/// One user of a hub server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub name: String,
    /// One of the hub's `roles`; empty while pending.
    pub role: String,
    #[serde(default)]
    pub status: Status,
    /// When they were added or signed up, once known.
    #[serde(default)]
    pub registered: String,
    /// The hashes of the tokens that sign in as this user, one per device or
    /// client, each revocable.
    pub tokens: Vec<Token>,
}

/// A token, as kept: its hash, and when it was made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub sha256: String,
    pub made: String,
}

/// Why a change to the users was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsersError {
    /// No user by that name.
    NotFound(String),
    /// A name taken, a queue full, a user in the wrong state.
    Conflict(String),
    /// A name or role that cannot be.
    Invalid(String),
    /// The file could not be read or written.
    Io(String),
}

impl fmt::Display for UsersError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UsersError::NotFound(m)
            | UsersError::Conflict(m)
            | UsersError::Invalid(m)
            | UsersError::Io(m) => f.write_str(m),
        }
    }
}

impl From<UsersError> for String {
    fn from(e: UsersError) -> String {
        e.to_string()
    }
}

/// The users of one hub server.
pub struct Users {
    path: PathBuf,
    users: BTreeMap<String, User>,
}

type Roles = BTreeMap<String, Vec<Permission>>;

impl Users {
    /// The users kept in `dir`, none when there is no file yet.
    pub fn load(dir: &Path) -> Result<Users, UsersError> {
        let path = dir.join(USERS_FILE);
        let users = match std::fs::read_to_string(&path) {
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
        Ok(Users { path, users })
    }

    /// Write the users, readable only by their owner.
    pub fn save(&self) -> Result<(), UsersError> {
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

    /// Add an active user with `role`, one of `roles`, and return their first
    /// token: what the admin does on the server.
    pub fn add(&mut self, name: &str, role: &str, roles: &Roles) -> Result<String, UsersError> {
        let name = new_name(name)?;
        self.free(&name)?;
        known_role(role, roles)?;
        self.insert(&name, role, Status::Active);
        self.token(&name)
    }

    /// Sign `name` up: a pending user, whose token works once an admin approves
    /// them. Refused while `max_pending` sign-ups wait.
    pub fn register(&mut self, name: &str, max_pending: u32) -> Result<String, UsersError> {
        let name = new_name(name)?;
        self.free(&name)?;
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
        self.insert(&name, "", Status::Pending);
        self.token(&name)
    }

    /// Approve a pending user with `role`.
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

    /// Give an active user another role.
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

    /// A new token for `name`. Their other tokens keep working until revoked.
    pub fn token(&mut self, name: &str) -> Result<String, UsersError> {
        let user = self.user_mut(name)?;
        let mut bytes = [0u8; 32];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| UsersError::Io("the system gave no random bytes for a token".into()))?;
        let token = format!("ens_{}", hex(&bytes));
        user.tokens.push(Token {
            sha256: sha256(&token),
            made: now(),
        });
        Ok(token)
    }

    /// Revoke every token of `name`, and say how many there were.
    pub fn revoke(&mut self, name: &str) -> Result<usize, UsersError> {
        Ok(std::mem::take(&mut self.user_mut(name)?.tokens).len())
    }

    /// Remove a user, or turn down a sign-up.
    pub fn remove(&mut self, name: &str) -> Result<(), UsersError> {
        self.users
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| UsersError::NotFound(format!("no user called {name}")))
    }

    /// The active user `token` signs in as, if any.
    pub fn authenticate(&self, token: &str) -> Option<&User> {
        self.identify(token).filter(|u| u.status == Status::Active)
    }

    /// The user `token` belongs to, pending or active.
    pub fn identify(&self, token: &str) -> Option<&User> {
        let hash = sha256(token.trim());
        // Every hash is compared, all the way, so the time taken says nothing.
        let mut found = None;
        for user in self.users.values() {
            for t in &user.tokens {
                if same(t.sha256.as_bytes(), hash.as_bytes()) {
                    found = Some(user);
                }
            }
        }
        found
    }

    fn insert(&mut self, name: &str, role: &str, status: Status) {
        self.users.insert(
            name.to_string(),
            User {
                name: name.to_string(),
                role: role.to_string(),
                status,
                registered: now(),
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

/// A user name: one word of letters, digits, `.`, `_` or `-`, at most 64.
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

fn now() -> String {
    ensemble_core::time::utc(std::time::SystemTime::now())
}

fn sha256(token: &str) -> String {
    hex(digest(&SHA256, token.as_bytes()).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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

    #[test]
    fn a_token_signs_in_as_its_user_until_it_is_revoked() {
        let tmp = TempDir::new();
        let mut users = Users::load(&tmp.0).unwrap();
        let ana = users.add("ana", "member", &roles()).unwrap();
        let second = users.token("ana").unwrap();
        assert!(ana.starts_with("ens_") && ana.len() == 4 + 64);
        assert_ne!(ana, second);
        users.save().unwrap();

        let users = Users::load(&tmp.0).unwrap();
        assert_eq!(users.authenticate(&ana).unwrap().name, "ana");
        assert_eq!(users.authenticate(&second).unwrap().role, "member");
        assert!(users.authenticate("ens_nope").is_none());
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
        assert!(users.authenticate(&ana).is_none());
        users.remove("ana").unwrap();
        assert!(users.get("ana").is_none());
    }

    #[test]
    fn a_user_needs_a_new_name_and_a_role_the_hub_has() {
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
    }

    #[test]
    fn a_sign_up_signs_in_to_nothing_until_an_admin_approves_it() {
        let tmp = TempDir::new();
        let mut users = Users::load(&tmp.0).unwrap();
        let token = users.register("ana", 2).unwrap();
        assert!(users.authenticate(&token).is_none());
        let ana = users.identify(&token).unwrap();
        assert_eq!((ana.status, ana.role.as_str()), (Status::Pending, ""));
        assert!(matches!(
            users.set_role("ana", "member", &roles()),
            Err(UsersError::Conflict(_))
        ));
        users.approve("ana", "member", &roles()).unwrap();
        assert_eq!(users.authenticate(&token).unwrap().role, "member");
        assert!(matches!(
            users.approve("ana", "member", &roles()),
            Err(UsersError::Conflict(_))
        ));
        users.set_role("ana", "reader", &roles()).unwrap();
        assert_eq!(users.get("ana").unwrap().role, "reader");
    }

    #[test]
    fn sign_ups_wait_at_most_max_pending_and_take_no_name_twice() {
        let tmp = TempDir::new();
        let mut users = Users::load(&tmp.0).unwrap();
        users.register("ana", 2).unwrap();
        assert!(matches!(
            users.register("ana", 2),
            Err(UsersError::Conflict(_))
        ));
        users.register("bob", 2).unwrap();
        let full = users.register("carol", 2).unwrap_err().to_string();
        assert!(full.contains("too many sign-ups"), "{full}");
        users.remove("bob").unwrap();
        assert!(
            users.register("carol", 2).is_ok(),
            "turning one down makes room"
        );
    }

    #[test]
    fn users_from_before_sign_up_existed_are_active() {
        let tmp = TempDir::new();
        let mut users = Users::load(&tmp.0).unwrap();
        let token = users.add("ana", "member", &roles()).unwrap();
        users.save().unwrap();
        let path = tmp.0.join(USERS_FILE);
        let mut old: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let ana = old["ana"].as_object_mut().unwrap();
        ana.remove("status");
        ana.remove("registered");
        std::fs::write(&path, old.to_string()).unwrap();
        let users = Users::load(&tmp.0).unwrap();
        assert_eq!(users.authenticate(&token).unwrap().status, Status::Active);
    }
}
