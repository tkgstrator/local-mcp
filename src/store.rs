//! Persistence for the OAuth flow.
//!
//! These rows used to live in `HashMap`s, which meant every registered client
//! and every issued token vanished when the process restarted — while the
//! clients holding those tokens carried on presenting them, and got 401s that
//! looked like a broken credential rather than a forgotten one.

use std::{
    path::Path,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

pub struct PendingCode {
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub expires_at: SystemTime,
}

pub struct Store {
    connection: Mutex<Connection>,
}

/// Seconds since the epoch. `Instant` cannot be written down and read back —
/// it is only meaningful within one run of the process, which is the whole
/// problem this module exists to fix.
fn to_epoch(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        // Only reachable if the clock is set before 1970, in which case
        // everything is already expired.
        .unwrap_or(0)
}

fn from_epoch(seconds: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds.max(0) as u64)
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }

        let connection =
            Connection::open(path).with_context(|| format!("cannot open {}", path.display()))?;

        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS clients (
                     client_id TEXT NOT NULL,
                     redirect_uri TEXT NOT NULL,
                     PRIMARY KEY (client_id, redirect_uri)
                 );
                 CREATE TABLE IF NOT EXISTS codes (
                     code TEXT PRIMARY KEY,
                     client_id TEXT NOT NULL,
                     redirect_uri TEXT NOT NULL,
                     code_challenge TEXT NOT NULL,
                     expires_at INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS tokens (
                     token TEXT PRIMARY KEY,
                     expires_at INTEGER NOT NULL
                 );",
            )
            .context("cannot create the OAuth tables")?;

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("the OAuth database lock is poisoned"))
    }

    pub fn register_client(&self, client_id: &str, redirect_uris: &[String]) -> Result<()> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        for uri in redirect_uris {
            transaction.execute(
                "INSERT OR IGNORE INTO clients (client_id, redirect_uri) VALUES (?1, ?2)",
                params![client_id, uri],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// `None` when the client was never registered, which callers report
    /// differently from a client whose redirect_uri simply does not match.
    pub fn redirect_uris(&self, client_id: &str) -> Result<Option<Vec<String>>> {
        let connection = self.lock()?;
        let mut statement =
            connection.prepare("SELECT redirect_uri FROM clients WHERE client_id = ?1")?;
        let uris = statement
            .query_map(params![client_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;

        Ok((!uris.is_empty()).then_some(uris))
    }

    pub fn store_code(&self, code: &str, pending: &PendingCode) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO codes (code, client_id, redirect_uri, code_challenge, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                code,
                pending.client_id,
                pending.redirect_uri,
                pending.code_challenge,
                to_epoch(pending.expires_at),
            ],
        )?;
        Ok(())
    }

    /// Removes the code as it reads it: an authorization code is single-use, so
    /// a replay finds nothing even if the first exchange is still in flight.
    pub fn take_code(&self, code: &str) -> Result<Option<PendingCode>> {
        let connection = self.lock()?;
        let pending = connection
            .query_row(
                "DELETE FROM codes WHERE code = ?1
                 RETURNING client_id, redirect_uri, code_challenge, expires_at",
                params![code],
                |row| {
                    Ok(PendingCode {
                        client_id: row.get(0)?,
                        redirect_uri: row.get(1)?,
                        code_challenge: row.get(2)?,
                        expires_at: from_epoch(row.get(3)?),
                    })
                },
            )
            .optional()?;

        Ok(pending)
    }

    pub fn store_token(&self, token: &str, expires_at: SystemTime) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO tokens (token, expires_at) VALUES (?1, ?2)",
            params![token, to_epoch(expires_at)],
        )?;
        Ok(())
    }

    pub fn token_is_valid(&self, token: &str) -> Result<bool> {
        let now = to_epoch(SystemTime::now());
        let connection = self.lock()?;
        // Expired rows are cleared on the way past rather than on a timer, so
        // the table cannot grow without bound in a long-lived process.
        connection.execute("DELETE FROM tokens WHERE expires_at <= ?1", params![now])?;
        connection.execute("DELETE FROM codes WHERE expires_at <= ?1", params![now])?;

        let valid = connection
            .query_row(
                "SELECT 1 FROM tokens WHERE token = ?1",
                params![token],
                |_| Ok(()),
            )
            .optional()?
            .is_some();

        Ok(valid)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(&dir.path().join("nested/oauth.db")).expect("open");
        (dir, store)
    }

    #[test]
    fn a_token_outlives_the_process_that_issued_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("oauth.db");

        let issued = Store::open(&path).expect("open");
        issued
            .store_token("token-abc", SystemTime::now() + Duration::from_secs(60))
            .expect("store");
        drop(issued);

        // A second Store over the same file stands in for a restart.
        let reopened = Store::open(&path).expect("reopen");
        assert!(reopened.token_is_valid("token-abc").expect("check"));
    }

    #[test]
    fn an_expired_token_is_refused_and_swept() {
        let (_dir, store) = store();
        store
            .store_token("stale", SystemTime::now() - Duration::from_secs(1))
            .expect("store");

        assert!(!store.token_is_valid("stale").expect("check"));
        // Swept on the first check, so the row is gone rather than merely
        // filtered out.
        let connection = store.lock().expect("lock");
        let remaining: i64 = connection
            .query_row("SELECT count(*) FROM tokens", [], |row| row.get(0))
            .expect("count");
        assert_eq!(remaining, 0);
    }

    #[test]
    fn a_code_can_only_be_taken_once() {
        let (_dir, store) = store();
        let pending = PendingCode {
            client_id: "client".to_string(),
            redirect_uri: "https://example.com/cb".to_string(),
            code_challenge: "challenge".to_string(),
            expires_at: SystemTime::now() + Duration::from_secs(60),
        };
        store.store_code("code-1", &pending).expect("store");

        assert!(store.take_code("code-1").expect("take").is_some());
        assert!(store.take_code("code-1").expect("take again").is_none());
    }

    #[test]
    fn an_unregistered_client_is_distinct_from_a_mismatched_redirect() {
        let (_dir, store) = store();
        store
            .register_client("client", &["https://example.com/cb".to_string()])
            .expect("register");

        assert_eq!(
            store.redirect_uris("client").expect("lookup"),
            Some(vec!["https://example.com/cb".to_string()])
        );
        assert_eq!(store.redirect_uris("nobody").expect("lookup"), None);
    }

    #[test]
    fn registering_the_same_client_twice_keeps_one_row_per_uri() {
        let (_dir, store) = store();
        let uris = ["https://example.com/cb".to_string()];
        store.register_client("client", &uris).expect("first");
        store.register_client("client", &uris).expect("second");

        assert_eq!(
            store.redirect_uris("client").expect("lookup"),
            Some(vec!["https://example.com/cb".to_string()])
        );
    }
}
