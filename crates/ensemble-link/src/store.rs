//! The hub server's state that changes while it runs: its accounts and their
//! tokens (`users`), and the tracker relay's deliveries. One Turso database,
//! `database.db`, in the hub's folder (`~/.config/ensemble/hub/`), which only
//! whoever runs the hub may enter.
//!
//! Turso locks the file while it is open, and another process cannot open it
//! then. So each read or change opens it, does its work (a change in one
//! transaction), and closes it; an open that finds the file locked tries again
//! for a few seconds. That is what lets `ensemble hub user` change accounts on
//! the server while the hub runs.

use ensemble_core::runtime;
use std::path::Path;
use std::time::{Duration, Instant};
pub use turso::Value;

/// The database file in the hub's state directory.
pub const DB_FILE: &str = "database.db";
/// How long an open waits for another process to let go of the file.
const WAIT: Duration = Duration::from_secs(5);

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS users (
    name TEXT PRIMARY KEY,
    role TEXT NOT NULL,
    status TEXT NOT NULL,
    registered TEXT NOT NULL,
    password_rounds INTEGER,
    password_salt TEXT,
    password_hash TEXT,
    tracker_user TEXT
);
CREATE TABLE IF NOT EXISTS tokens (
    sha256 TEXT PRIMARY KEY,
    user TEXT NOT NULL,
    id TEXT NOT NULL,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    scopes TEXT,
    expires TEXT,
    made TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS deliveries (
    issue TEXT PRIMARY KEY,
    data TEXT NOT NULL
);
";

/// Open the database in `dir`, made with its tables when it is not there,
/// and run `work` on it. The folder is made, and kept, readable only by its
/// owner.
pub fn with<T>(
    dir: &Path,
    work: impl AsyncFnOnce(&turso::Connection) -> Result<T, String>,
) -> Result<T, String> {
    private_dir(dir)?;
    let path = dir.join(DB_FILE);
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
        conn.execute_batch(SCHEMA)
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

    #[test]
    fn a_transaction_is_kept_whole_and_the_folder_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        let dir = tmp.0.join("hub");
        with(&dir, async |c| {
            transaction(
                c,
                vec![(
                    "INSERT INTO deliveries (issue, data) VALUES (?1, ?2)",
                    vec![Value::Text("7".into()), Value::Text("{}".into())],
                )],
            )
            .await
        })
        .unwrap();
        // A statement that fails takes the ones before it back with it.
        let failed = with(&dir, async |c| {
            transaction(
                c,
                vec![
                    (
                        "INSERT INTO deliveries (issue, data) VALUES (?1, ?2)",
                        vec![Value::Text("8".into()), Value::Text("{}".into())],
                    ),
                    (
                        "INSERT INTO deliveries (issue, data) VALUES (?1, ?2)",
                        vec![Value::Text("7".into()), Value::Text("{}".into())],
                    ),
                ],
            )
            .await
        });
        assert!(failed.is_err());
        let issues = with(&dir, async |c| {
            rows(c, "SELECT issue FROM deliveries ORDER BY issue", vec![]).await
        })
        .unwrap();
        let issues: Vec<Option<String>> = issues.iter().map(|r| text(&r[0])).collect();
        assert_eq!(issues, vec![Some("7".to_string())]);
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
