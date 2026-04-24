//! Matrix channel bindings (`room_id` ↔ `session_db_id`) as `impl SessionRegistry`.
//! Stored in the chazdb's `matrix_channels` DocStore.

use crate::types::ConversationId;

use eidetica::Database;
use eidetica::store::DocStore;
use tracing::{info, warn};

use super::registry::{STORE_MATRIX_CHANNELS, SessionRegistry};

impl SessionRegistry {
    /// Return the session bound to a Matrix room, if any.
    pub async fn matrix_channel_for_room(&self, room_id: &str) -> anyhow::Result<Option<String>> {
        let txn = self.chazdb.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_MATRIX_CHANNELS).await?;
        Ok(store.get_string(room_id).await.ok())
    }

    /// Attach a Matrix room to a session. Overwrites any existing binding for this room.
    pub async fn attach_matrix_room(
        &self,
        room_id: &str,
        session_db_id: &str,
    ) -> anyhow::Result<()> {
        let txn = self.chazdb.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_MATRIX_CHANNELS).await?;
        store.set_string(room_id, session_db_id).await?;
        txn.commit().await?;
        info!(room_id, session_db_id, "Matrix room attached to session");
        Ok(())
    }

    pub async fn detach_matrix_room(&self, room_id: &str) -> anyhow::Result<()> {
        let txn = self.chazdb.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_MATRIX_CHANNELS).await?;
        let _ = store.delete(room_id).await;
        txn.commit().await?;
        Ok(())
    }

    /// List every (room_id, session_db_id) pair.
    pub async fn list_matrix_channels(&self) -> anyhow::Result<Vec<(String, String)>> {
        let txn = self.chazdb.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_MATRIX_CHANNELS).await?;
        let doc = store.get_all().await?;
        Ok(doc
            .iter()
            .filter_map(|(k, v)| {
                let session_db_id: String = v.try_into().ok()?;
                Some((k.clone(), session_db_id))
            })
            .collect())
    }

    /// List all Matrix rooms currently attached to a session.
    pub async fn matrix_channels_for_session(
        &self,
        session_db_id: &str,
    ) -> anyhow::Result<Vec<String>> {
        Ok(self
            .list_matrix_channels()
            .await?
            .into_iter()
            .filter_map(|(room, sid)| {
                if sid == session_db_id {
                    Some(room)
                } else {
                    None
                }
            })
            .collect())
    }

    /// Convenience for the Matrix gateway: get (or create) the session bound to a room.
    ///
    /// If no binding exists, creates a fresh session, attaches the room to it, and
    /// returns it.
    pub async fn get_or_create_matrix_session(
        &self,
        room_id: &str,
    ) -> anyhow::Result<(ConversationId, Database)> {
        if let Some(session_db_id) = self.matrix_channel_for_room(room_id).await? {
            match self.open_session(&session_db_id).await {
                Ok(r) => return Ok(r),
                Err(e) => {
                    warn!(
                        room_id,
                        session_db_id,
                        "Dangling matrix channel — session unreadable, recreating: {e}"
                    );
                    let _ = self.detach_matrix_room(room_id).await;
                }
            }
        }
        let source = format!("matrix:{room_id}");
        let (conv_id, db) = self.create_session(Some(&source)).await?;
        let session_db_id = db.root_id().to_string();
        self.attach_matrix_room(room_id, &session_db_id).await?;
        Ok((conv_id, db))
    }
}
