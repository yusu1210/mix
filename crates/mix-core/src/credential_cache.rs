//! Optional, read-only recovery sources. Mix never runs another manager's
//! scripts or changes its database, selection, routes, or credentials.
use crate::fs::{read_bounded, MAX_CREDENTIAL_BYTES};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

const MAX_CANDIDATES: usize = 128;
const MAX_CACHED_AUTH_BYTES: usize = 64 * 1024;

pub(crate) fn cc_switch_credentials(directory: &Path) -> Vec<Value> {
    let mut candidates = Vec::new();
    if !regular_directory(directory) {
        return candidates;
    }
    let cache = directory.join("provider-auth");
    if regular_directory(&cache) {
        if let Ok(entries) = std::fs::read_dir(cache) {
            for entry in entries.take(MAX_CANDIDATES).flatten() {
                if entry.path().extension().is_some_and(|ext| ext == "json") {
                    if let Ok(bytes) = read_bounded(
                        &entry.path(),
                        MAX_CACHED_AUTH_BYTES as u64,
                        "cannot read optional credential cache",
                    ) {
                        if let Ok(document) = serde_json::from_slice(&bytes) {
                            candidates.push(document);
                        }
                    }
                }
            }
        }
    }

    let database = directory.join("cc-switch.db");
    if !std::fs::symlink_metadata(&database).is_ok_and(|metadata| metadata.file_type().is_file()) {
        return candidates;
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        if std::fs::symlink_metadata(directory.join(format!("cc-switch.db{suffix}")))
            .is_ok_and(|metadata| !metadata.file_type().is_file())
        {
            return candidates;
        }
    }
    // READ_ONLY (not immutable) includes committed WAL data, but never creates
    // a database. Query only the known Codex credential column, not settings.
    let Ok(connection) = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return candidates;
    };
    let _ = connection.busy_timeout(Duration::from_millis(250));
    let Ok(mut statement) = connection.prepare(
        "SELECT settings_config FROM providers
         WHERE app_type = 'codex' AND length(CAST(settings_config AS BLOB)) <= ?1
         ORDER BY id LIMIT ?2",
    ) else {
        return candidates;
    };
    if let Ok(rows) = statement.query_map(
        [MAX_CREDENTIAL_BYTES as i64, MAX_CANDIDATES as i64],
        |row| row.get::<_, String>(0),
    ) {
        for row in rows.flatten() {
            if let Ok(mut document) = serde_json::from_str::<Value>(&row) {
                if let Some(auth) = document.get_mut("auth") {
                    if auth.to_string().len() <= MAX_CACHED_AUTH_BYTES {
                        candidates.push(auth.take());
                    }
                }
            }
        }
    }
    candidates
}

fn regular_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn absent_or_corrupt_optional_sources_do_not_block_switching() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("absent");
        assert!(cc_switch_credentials(&missing).is_empty());
        assert!(!missing.exists());
        std::fs::write(root.path().join("cc-switch.db"), b"not a database").unwrap();
        assert!(cc_switch_credentials(root.path()).is_empty());
        assert_eq!(
            std::fs::read(root.path().join("cc-switch.db")).unwrap(),
            b"not a database"
        );
    }

    #[test]
    fn reads_both_sources_without_copying_routes_or_changing_the_database() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("provider-auth");
        std::fs::create_dir(&cache).unwrap();
        let cached = json!({"tokens":{"account_id":"example", "refresh_token":"cached"}});
        std::fs::write(cache.join("arbitrary-provider.json"), cached.to_string()).unwrap();
        std::fs::write(cache.join("broken.json"), b"{").unwrap();
        let database = root.path().join("cc-switch.db");
        let writer = Connection::open(&database).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
            CREATE TABLE providers (id TEXT, app_type TEXT, settings_config TEXT);",
            )
            .unwrap();
        let stored = json!({"tokens":{"account_id":"example", "refresh_token":"stored"}});
        for app in ["codex", "claude"] {
            writer
                .execute(
                    "INSERT INTO providers VALUES ('anything', ?1, ?2)",
                    [
                        app,
                        &json!({"auth":stored,"config":"private route"}).to_string(),
                    ],
                )
                .unwrap();
        }
        let before = std::fs::read(&database).unwrap();
        let wal = root.path().join("cc-switch.db-wal");
        let before_wal = std::fs::read(&wal).unwrap();
        let candidates = cc_switch_credentials(root.path());
        assert_eq!(candidates, [cached, stored]);
        assert_eq!(std::fs::read(&database).unwrap(), before);
        assert_eq!(std::fs::read(wal).unwrap(), before_wal);
        assert!(candidates.iter().all(|item| item.get("config").is_none()));
    }

    #[cfg(unix)]
    #[test]
    fn ignores_symlinked_sources() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("auth.json"), b"{}").unwrap();
        symlink(outside.path(), root.path().join("provider-auth")).unwrap();
        symlink(
            outside.path().join("auth.json"),
            root.path().join("cc-switch.db"),
        )
        .unwrap();
        assert!(cc_switch_credentials(root.path()).is_empty());
    }
}
