//! Turso databases, each a `database.db` in a folder only its owner may
//! enter ([`with`]), with the tables its schema makes: the hub has one for
//! its core and one for each connector that keeps anything, a runner one
//! for its core. What a hub's core and a runner's both keep, their settings'
//! audit log, the ANN pool and the keys of its providers, sealed, is [`kept`].
//! [`open`] opens another Turso file, such as the knowledge vault's cache of
//! its notes' vectors.
//!
//! Turso locks the file while it is open, and another process cannot open it
//! then. So each read or change opens it, does its work (a change in one
//! transaction), and closes it; an open that finds the file locked tries again
//! for a few seconds. That is what lets `ensemble-hub user` change accounts on
//! the server while the hub runs.

pub mod kept;

use ensemble_core::runtime;
use std::path::Path;
use std::time::{Duration, Instant};
pub use turso::{Connection, Value};

/// The database file in a folder.
pub use ensemble_core::state::DB_FILE;
/// How long an open waits for another process to let go of the file.
const WAIT: Duration = Duration::from_secs(5);

/// The tables a hub's core and a runner's both have: the keys kept,
/// sealed, the ANN pool, and the audit log of their settings.
const KEPT_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS secrets (
    name TEXT PRIMARY KEY,
    sealed TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS ann_providers (
    name TEXT PRIMARY KEY,
    type TEXT NOT NULL,
    base_url TEXT,
    api_key_env TEXT,
    timeout_seconds INTEGER,
    max_concurrent INTEGER,
    runner TEXT
);
CREATE TABLE IF NOT EXISTS ann_models (
    provider TEXT NOT NULL,
    id TEXT NOT NULL,
    kind TEXT NOT NULL,
    tier TEXT,
    context_window INTEGER,
    tools INTEGER,
    dimensions INTEGER,
    missing INTEGER NOT NULL,
    shared INTEGER NOT NULL,
    PRIMARY KEY (provider, id)
);
CREATE TABLE IF NOT EXISTS settings_audit (
    n INTEGER PRIMARY KEY,
    at TEXT NOT NULL,
    who TEXT NOT NULL,
    operation TEXT NOT NULL,
    input TEXT NOT NULL
);
";

/// Open the database in `dir`, made with the tables of `schema` when they
/// are not there, and run `work` on it. The folder is made, and kept,
/// readable only by its owner.
pub fn with<T>(
    dir: &Path,
    schema: &str,
    work: impl AsyncFnOnce(&turso::Connection) -> Result<T, String>,
) -> Result<T, String> {
    private_dir(dir)?;
    open(&dir.join(DB_FILE), schema, work)
}

/// Open the core database in `dir`, a hub's or a runner's: with the tables
/// both have, and those of `schema`, and run `work` on it.
pub fn kept_with<T>(
    dir: &Path,
    schema: &str,
    work: impl AsyncFnOnce(&turso::Connection) -> Result<T, String>,
) -> Result<T, String> {
    private_dir(dir)?;
    open(&dir.join(DB_FILE), KEPT_SCHEMA, async |c| {
        c.execute_batch(schema).await.map_err(|e| e.to_string())?;
        work(c).await
    })
}

/// Open the Turso file at `path`, made with the tables of `schema` when they
/// are not there, and run `work` on it, as [`with`] does a folder's database.
pub fn open<T>(
    path: &Path,
    schema: &str,
    work: impl AsyncFnOnce(&turso::Connection) -> Result<T, String>,
) -> Result<T, String> {
    let path = path
        .to_str()
        .ok_or_else(|| format!("{} is not a path Turso can open", path.display()))?;
    runtime::block_on(async {
        let started = Instant::now();
        let db = loop {
            match turso::Builder::new_local(path).build().await {
                Ok(db) => break db,
                Err(e) if is_locked(&e) && started.elapsed() < WAIT => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => return Err(format!("cannot open {path}: {e}")),
            }
        };
        let conn = db
            .connect()
            .map_err(|e| format!("cannot open {path}: {e}"))?;
        // Another process, or another request of this one, may be writing.
        conn.busy_timeout(WAIT)
            .map_err(|e| format!("cannot open {path}: {e}"))?;
        conn.execute_batch(schema)
            .await
            .map_err(|e| format!("{path}: {e}"))?;
        work(&conn).await.map_err(|e| format!("{path}: {e}"))
    })
}

/// Run `statements` as one transaction: all of them, or none.
pub async fn transaction(
    conn: &turso::Connection,
    statements: Vec<(&str, Vec<Value>)>,
) -> Result<(), String> {
    let tx = conn
        .unchecked_transaction()
        .await
        .map_err(|e| e.to_string())?;
    for (sql, params) in statements {
        tx.execute(sql, params).await.map_err(|e| e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())
}

/// Every secret kept, sealed, by its name.
pub fn secrets(dir: &Path) -> Result<std::collections::BTreeMap<String, String>, String> {
    let found = kept_with(dir, "", async |c| {
        rows(c, "SELECT name, sealed FROM secrets", vec![]).await
    })?;
    Ok(found
        .iter()
        .filter_map(|r| Some((text(&r[0])?, text(&r[1])?)))
        .collect())
}

/// Keep the secret `name`, sealed, in place of the one kept; none takes it out.
pub fn keep_secret(dir: &Path, name: &str, sealed: Option<&str>) -> Result<(), String> {
    let name = Value::Text(name.into());
    kept_with(dir, "", async |c| match sealed {
        Some(s) => execute(
            c,
            "INSERT OR REPLACE INTO secrets (name, sealed) VALUES (?1, ?2)",
            vec![name, Value::Text(s.into())],
        )
        .await
        .map(|_| ()),
        None => execute(c, "DELETE FROM secrets WHERE name = ?1", vec![name])
            .await
            .map(|_| ()),
    })
}

/// Run one statement, and return how many rows it changed.
pub async fn execute(
    conn: &turso::Connection,
    sql: &str,
    params: Vec<Value>,
) -> Result<u64, String> {
    conn.execute(sql, params).await.map_err(|e| e.to_string())
}

/// Every row `sql` returns, each as its values.
pub async fn rows(
    conn: &turso::Connection,
    sql: &str,
    params: Vec<Value>,
) -> Result<Vec<Vec<Value>>, String> {
    let mut rows = conn.query(sql, params).await.map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(|e| e.to_string())? {
        let values = (0..row.column_count())
            .map(|i| row.get_value(i).map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        out.push(values);
    }
    Ok(out)
}

/// A text value, or none for a null.
pub fn text(value: &Value) -> Option<String> {
    match value {
        Value::Text(t) => Some(t.clone()),
        _ => None,
    }
}

/// A text value for `text`, or a null for none.
pub fn optional(text: Option<&str>) -> Value {
    text.map_or(Value::Null, |t| Value::Text(t.to_string()))
}

fn is_locked(e: &turso::Error) -> bool {
    let said = e.to_string();
    said.contains("locked") || said.contains("Busy") || said.contains("busy")
}

/// Make `dir`, and keep it the owner's alone: it holds password and token
/// hashes.
fn private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("cannot keep {} private: {e}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ensemble_core::workspace::testing::TempDir;

    const DELIVERIES: &str = "CREATE TABLE IF NOT EXISTS tracker_deliveries (tracker TEXT NOT NULL, issue TEXT NOT NULL, data TEXT NOT NULL, PRIMARY KEY (tracker, issue));";

    #[test]
    fn a_transaction_is_kept_whole_and_the_folder_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        let dir = tmp.0.join("hub");
        with(&dir, DELIVERIES, async |c| {
            transaction(
                c,
                vec![(
                    "INSERT INTO tracker_deliveries (tracker, issue, data) VALUES ('t', ?1, ?2)",
                    vec![Value::Text("7".into()), Value::Text("{}".into())],
                )],
            )
            .await
        })
        .unwrap();
        // A statement that fails takes the ones before it back with it.
        let failed = with(&dir, DELIVERIES, async |c| {
            transaction(
                c,
                vec![
                    (
                        "INSERT INTO tracker_deliveries (tracker, issue, data) VALUES ('t', ?1, ?2)",
                        vec![Value::Text("8".into()), Value::Text("{}".into())],
                    ),
                    (
                        "INSERT INTO tracker_deliveries (tracker, issue, data) VALUES ('t', ?1, ?2)",
                        vec![Value::Text("7".into()), Value::Text("{}".into())],
                    ),
                ],
            )
            .await
        });
        assert!(failed.is_err());
        let issues = with(&dir, DELIVERIES, async |c| {
            rows(
                c,
                "SELECT issue FROM tracker_deliveries ORDER BY issue",
                vec![],
            )
            .await
        })
        .unwrap();
        let issues: Vec<Option<String>> = issues.iter().map(|r| text(&r[0])).collect();
        assert_eq!(issues, vec![Some("7".to_string())]);
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn secrets_are_kept_by_name() {
        let tmp = TempDir::new();
        let dir = tmp.0.join("hub");
        keep_secret(&dir, "K", Some("sealed-1")).unwrap();
        keep_secret(&dir, "K", Some("sealed-2")).unwrap();
        keep_secret(&dir, "L", Some("sealed-3")).unwrap();
        keep_secret(&dir, "L", None).unwrap();
        let kept = secrets(&dir).unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept["K"], "sealed-2");
    }
}
