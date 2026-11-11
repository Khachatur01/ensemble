//! A hub server's users, and the tokens their nodes sign in with. Users are
//! state, not config: they live in `users.json` in the hub's state directory,
//! which only the admin who runs the hub can read. A token is shown once, when
//! it is made, and only its SHA-256 is kept, so the file gives nobody a way in.

use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const USERS_FILE: &str = "users.json";

/// One user of a hub server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub name: String,
    /// One of the hub's `roles`.
    pub role: String,
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

/// The users of one hub server.
pub struct Users {
    path: PathBuf,
    users: BTreeMap<String, User>,
}

impl Users {
    /// The users kept in `dir`, none when there is no file yet.
    pub fn load(dir: &Path) -> Result<Users, String> {
        let path = dir.join(USERS_FILE);
        let users = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("{} is not readable: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        Ok(Users { path, users })
    }

    /// Write the users, readable only by their owner.
    pub fn save(&self) -> Result<(), String> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let json = serde_json::to_string_pretty(&self.users).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("tmp");
        write_private(&tmp, &json)
            .and_then(|()| std::fs::rename(&tmp, &self.path))
            .map_err(|e| format!("cannot write {}: {e}", self.path.display()))
    }

    pub fn list(&self) -> impl Iterator<Item = &User> {
        self.users.values()
    }

    pub fn get(&self, name: &str) -> Option<&User> {
        self.users.get(name)
    }

    /// Add a user with `role`, one of `roles`, and return their first token.
    pub fn add(
        &mut self,
        name: &str,
        role: &str,
        roles: &BTreeMap<String, Vec<ensemble_config::Permission>>,
    ) -> Result<String, String> {
        let name = name.trim();
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(format!("'{name}' must be one word, such as ana"));
        }
        if self.users.contains_key(name) {
            return Err(format!(
                "{name} is a user already; `ensemble hub user token {name}` makes another token"
            ));
        }
        if !roles.contains_key(role) {
            let known: Vec<&str> = roles.keys().map(String::as_str).collect();
            return Err(format!(
                "the hub has no role '{role}'. Its roles: {}.",
                known.join(", ")
            ));
        }
        self.users.insert(
            name.to_string(),
            User {
                name: name.to_string(),
                role: role.to_string(),
                tokens: Vec::new(),
            },
        );
        self.token(name)
    }

    /// A new token for `name`. Their other tokens keep working until revoked.
    pub fn token(&mut self, name: &str) -> Result<String, String> {
        let user = self
            .users
            .get_mut(name)
            .ok_or_else(|| format!("no user called {name}"))?;
        let mut bytes = [0u8; 32];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| "the system gave no random bytes for a token")?;
        let token = format!("ens_{}", hex(&bytes));
        user.tokens.push(Token {
            sha256: sha256(&token),
            made: ensemble_core::time::utc(std::time::SystemTime::now()),
        });
        Ok(token)
    }

    /// Revoke every token of `name`, and say how many there were.
    pub fn revoke(&mut self, name: &str) -> Result<usize, String> {
        let user = self
            .users
            .get_mut(name)
            .ok_or_else(|| format!("no user called {name}"))?;
        Ok(std::mem::take(&mut user.tokens).len())
    }

    pub fn remove(&mut self, name: &str) -> Result<(), String> {
        self.users
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| format!("no user called {name}"))
    }

    /// The user `token` signs in as, if any.
    pub fn authenticate(&self, token: &str) -> Option<&User> {
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
    use ensemble_config::Permission;
    use ensemble_core::workspace::testing::TempDir;

    fn roles() -> BTreeMap<String, Vec<Permission>> {
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
        assert!(
            users
                .add("ana", "reader", &roles())
                .unwrap_err()
                .contains("a user already")
        );
        let err = users.add("bob", "owner", &roles()).unwrap_err();
        assert!(
            err.contains("no role 'owner'") && err.contains("member, reader"),
            "{err}"
        );
        assert!(users.add("two words", "member", &roles()).is_err());
        assert!(users.token("carol").is_err());
    }
}
