//! Keeping a session id usable for longer than the worker behind it.
//!
//! The transport drops a session that has sat idle, and answers 404 to the next
//! request carrying its id. That is what the spec asks for, but it reads to a
//! client as "this server is gone" rather than "say hello again" — and it lands
//! exactly when someone returns to a conversation they left to work on another
//! one. Handing the SDK a store changes the ending: the handshake is reloaded
//! from `state_db` behind the client's back and the request is served on the id
//! it already had, so there is nothing for it to notice or recover from.

use std::sync::Arc;

use rmcp::transport::streamable_http_server::session::{
    SessionState, SessionStore, SessionStoreError,
};

use crate::store::Store;

pub struct PersistentSessions {
    store: Arc<Store>,
}

impl PersistentSessions {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

/// The handshake is stored as the JSON the client sent rather than as columns:
/// `InitializeRequestParams` is the SDK's type to change, and a schema of our
/// own would have to be migrated every time it did.
#[async_trait::async_trait]
impl SessionStore for PersistentSessions {
    async fn load(&self, session_id: &str) -> Result<Option<SessionState>, SessionStoreError> {
        let Some(params) = self.store.load_session(session_id)? else {
            return Ok(None);
        };
        Ok(Some(SessionState::new(serde_json::from_str(&params)?)))
    }

    async fn store(&self, session_id: &str, state: &SessionState) -> Result<(), SessionStoreError> {
        let params = serde_json::to_string(&state.initialize_params)?;
        self.store.save_session(session_id, &params)?;
        Ok(())
    }

    async fn delete(&self, session_id: &str) -> Result<(), SessionStoreError> {
        self.store.delete_session(session_id)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rmcp::model::ClientInfo;

    use super::*;

    fn sessions() -> (tempfile::TempDir, PersistentSessions) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Arc::new(Store::open(&dir.path().join("state.db")).expect("open"));
        (dir, PersistentSessions::new(store))
    }

    /// What the SDK does with the store: `store` on the handshake, `load` on a
    /// request whose session is no longer in memory. The params have to survive
    /// the round trip, because they are what the replayed handshake is built
    /// from.
    #[tokio::test]
    async fn a_stored_handshake_comes_back_as_it_went_in() {
        let (_dir, sessions) = sessions();
        let client_info = ClientInfo::default();

        sessions
            .store("session-abc", &SessionState::new(client_info.clone()))
            .await
            .expect("store");
        let loaded = sessions
            .load("session-abc")
            .await
            .expect("load")
            .expect("the id was just stored");

        assert_eq!(
            loaded.initialize_params.protocol_version,
            client_info.protocol_version
        );
        assert_eq!(
            loaded.initialize_params.client_info.name,
            client_info.client_info.name
        );
    }

    /// An id the store has never seen must read as absent so the transport falls
    /// through to its 404, rather than as an error that would become a 500.
    #[tokio::test]
    async fn an_unknown_session_is_absent_rather_than_an_error() {
        let (_dir, sessions) = sessions();
        assert!(sessions.load("never-issued").await.expect("load").is_none());
    }

    #[tokio::test]
    async fn a_deleted_session_is_no_longer_loadable() {
        let (_dir, sessions) = sessions();

        sessions
            .store("session-abc", &SessionState::new(ClientInfo::default()))
            .await
            .expect("store");
        sessions.delete("session-abc").await.expect("delete");

        assert!(sessions.load("session-abc").await.expect("load").is_none());
    }
}
