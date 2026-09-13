//! Persistence for the things clients keep holding across a restart: the OAuth
//! flow's rows, and the handshake behind a session id.
//!
//! These rows used to live in `HashMap`s, which meant every registered client
//! and every issued token vanished when the process restarted — while the
//! clients holding those tokens carried on presenting them, and got 401s that
//! looked like a broken credential rather than a forgotten one. A session id
//! is the same story with a different status code: the client carries on
//! presenting one the server has forgotten, and gets a 404.

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
                 );
                 CREATE TABLE IF NOT EXISTS sessions (
                     session_id TEXT PRIMARY KEY,
                     initialize_params TEXT NOT NULL,
                     last_seen INTEGER NOT NULL
                 );",
            )
            .context("cannot create the state tables")?;

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

    /// Remember the handshake a session id was opened with, so the id still
    /// means something after the in-memory session behind it has been dropped.
    /// Re-registering an id is normal rather than a conflict: the transport
    /// writes this on every successful handshake.
    pub fn save_session(&self, session_id: &str, initialize_params: &str) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO sessions (session_id, initialize_params, last_seen)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE
                 SET initialize_params = excluded.initialize_params,
                     last_seen = excluded.last_seen",
            params![session_id, initialize_params, to_epoch(SystemTime::now())],
        )?;
        Ok(())
    }

    /// Reading a session touches it as well. The retention window is there to
    /// forget conversations nobody came back to, and a session being revived
    /// right now is the clearest evidence that this is not one of them.
    pub fn load_session(&self, session_id: &str) -> Result<Option<String>> {
        let params = self
            .lock()?
            .query_row(
                "UPDATE sessions SET last_seen = ?2 WHERE session_id = ?1
                 RETURNING initialize_params",
                params![session_id, to_epoch(SystemTime::now())],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        Ok(params)
    }

    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        self.lock()?.execute(
            "DELETE FROM sessions WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// Drop sessions nothing has touched for `retention`, and report how many
    /// went. Called at startup rather than on the way past like the token sweep
    /// above: these rows are read while serving tool calls, and a sweep per
    /// request would bill every one of them for the cleanup.
    pub fn sweep_sessions(&self, retention: Duration) -> Result<usize> {
        // Saturating rather than panicking: a clock far enough behind for this
        // to underflow should expire nothing, not bring the server down.
        let cutoff = SystemTime::now()
            .checked_sub(retention)
            .unwrap_or(UNIX_EPOCH);
        let dropped = self.lock()?.execute(
            "DELETE FROM sessions WHERE last_seen <= ?1",
            params![to_epoch(cutoff)],
        )?;

        Ok(dropped)
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

    /// The point of persisting sessions at all: a client that reconnects after a
    /// restart is still holding the id it was given, and the handshake behind it
    /// has to be there to replay.
    #[test]
    fn a_session_outlives_the_process_that_opened_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("state.db");

        let opened = Store::open(&path).expect("open");
        opened
            .save_session("session-abc", r#"{"protocolVersion":"2025-06-18"}"#)
            .expect("save");
        drop(opened);

        let reopened = Store::open(&path).expect("reopen");
        assert_eq!(
            reopened
                .load_session("session-abc")
                .expect("load")
                .as_deref(),
            Some(r#"{"protocolVersion":"2025-06-18"}"#)
        );
        assert!(
            reopened
                .load_session("never-issued")
                .expect("load")
                .is_none(),
            "an unknown id must read as absent, not as an error"
        );
    }

    /// The transport writes this on every successful handshake, so an id being
    /// registered again is routine rather than a conflict.
    #[test]
    fn re_registering_a_session_keeps_one_row_and_the_newer_handshake() {
        let (_dir, store) = store();

        store
            .save_session("session-abc", r#"{"v":1}"#)
            .expect("first");
        store
            .save_session("session-abc", r#"{"v":2}"#)
            .expect("second");

        assert_eq!(
            store.load_session("session-abc").expect("load").as_deref(),
            Some(r#"{"v":2}"#)
        );
        // Nothing was swept, so a second row would still be here to find.
        assert_eq!(store.sweep_sessions(Duration::ZERO).expect("sweep"), 1);
    }

    #[test]
    fn a_deleted_session_stops_being_revivable() {
        let (_dir, store) = store();

        store
            .save_session("session-abc", r#"{"v":1}"#)
            .expect("save");
        store.delete_session("session-abc").expect("delete");

        assert!(store.load_session("session-abc").expect("load").is_none());
        // A client that sent DELETE and then retried must not resurrect it.
        store
            .delete_session("session-abc")
            .expect("deleting twice is fine");
    }

    #[test]
    fn the_sweep_only_forgets_sessions_past_their_retention() {
        let (_dir, store) = store();

        store.save_session("fresh", r#"{"v":1}"#).expect("save");

        assert_eq!(
            store
                .sweep_sessions(Duration::from_secs(3600))
                .expect("sweep"),
            0,
            "a session saved just now is not 3600 seconds old"
        );
        assert!(store.load_session("fresh").expect("load").is_some());

        assert_eq!(store.sweep_sessions(Duration::ZERO).expect("sweep"), 1);
        assert!(store.load_session("fresh").expect("load").is_none());
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
