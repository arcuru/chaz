//! External transport channel bindings — `(transport, login_id, channel)` ↔
//! `session_db_id` — as `impl SessionRegistry`.
//!
//! Stored in the chaz_group DB's `external_channels` DocStore (formerly the
//! Matrix-only `matrix_channels`). The `login_id` component distinguishes two
//! logins running on the same transport — one shared across agents, one
//! dedicated — so their channels never collide on address alone.

use crate::types::ConversationId;

use eidetica::Database;
use eidetica::store::DocStore;
use tracing::{info, warn};

use super::registry::{STORE_EXTERNAL_CHANNELS, STORE_LEGACY_MATRIX_CHANNELS, SessionRegistry};

/// Encode a channel identity as a stable DocStore key: JSON of the
/// `(transport, login_id, channel)` triple. JSON is used because Matrix
/// addresses (MXIDs `@u:server`, room ids `!r:server`) contain `:`, so a
/// naive `:`-joined key would be ambiguous. Reversible via [`parse_channel_key`].
fn channel_key(transport: &str, login_id: &str, channel: &str) -> String {
    serde_json::to_string(&(transport, login_id, channel))
        .expect("serializing a tuple of &str is infallible")
}

/// Reverse of [`channel_key`]. Returns `None` for keys not in the expected
/// shape (e.g. left over from a future schema), so callers skip them rather
/// than fail.
fn parse_channel_key(key: &str) -> Option<(String, String, String)> {
    serde_json::from_str(key).ok()
}

impl SessionRegistry {
    /// Return the session bound to `(transport, login_id, channel)`, if any.
    pub async fn external_channel_session(
        &self,
        transport: &str,
        login_id: &str,
        channel: &str,
    ) -> anyhow::Result<Option<String>> {
        let txn = self.chaz_group.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_EXTERNAL_CHANNELS).await?;
        Ok(store
            .get_string(&channel_key(transport, login_id, channel))
            .await
            .ok())
    }

    /// Attach `(transport, login_id, channel)` to a session. Overwrites any
    /// existing binding for that channel.
    pub async fn attach_channel(
        &self,
        transport: &str,
        login_id: &str,
        channel: &str,
        session_db_id: &str,
    ) -> anyhow::Result<()> {
        let txn = self.chaz_group.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_EXTERNAL_CHANNELS).await?;
        store
            .set_string(channel_key(transport, login_id, channel), session_db_id)
            .await?;
        txn.commit().await?;
        info!(
            transport,
            login_id, channel, session_db_id, "channel attached to session"
        );
        Ok(())
    }

    /// Remove the binding for `(transport, login_id, channel)`.
    pub async fn detach_channel(
        &self,
        transport: &str,
        login_id: &str,
        channel: &str,
    ) -> anyhow::Result<()> {
        let txn = self.chaz_group.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_EXTERNAL_CHANNELS).await?;
        let _ = store
            .delete(&channel_key(transport, login_id, channel))
            .await;
        txn.commit().await?;
        Ok(())
    }

    /// Every `(transport, login_id, channel, session_db_id)` binding.
    pub async fn list_channels(&self) -> anyhow::Result<Vec<(String, String, String, String)>> {
        let txn = self.chaz_group.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE_EXTERNAL_CHANNELS).await?;
        let doc = store.get_all().await?;
        Ok(doc
            .iter()
            .filter_map(|(k, v)| {
                let session_db_id: String = v.try_into().ok()?;
                let (transport, login_id, channel) = parse_channel_key(k)?;
                Some((transport, login_id, channel, session_db_id))
            })
            .collect())
    }

    /// List the `(transport, login_id, channel)` triples attached to a session.
    pub async fn channels_for_session(
        &self,
        session_db_id: &str,
    ) -> anyhow::Result<Vec<(String, String, String)>> {
        Ok(self
            .list_channels()
            .await?
            .into_iter()
            .filter_map(|(transport, login_id, channel, sid)| {
                (sid == session_db_id).then_some((transport, login_id, channel))
            })
            .collect())
    }

    /// Convenience for a gateway: get (or create) the session bound to a
    /// channel on a given login.
    ///
    /// If no binding exists, creates a fresh session, attaches the channel to
    /// it, and returns it.
    pub async fn get_or_create_channel_session(
        &self,
        transport: &str,
        login_id: &str,
        channel: &str,
    ) -> anyhow::Result<(ConversationId, Database)> {
        if let Some(session_db_id) = self
            .external_channel_session(transport, login_id, channel)
            .await?
        {
            match self.open_session(&session_db_id).await {
                Ok(r) => return Ok(r),
                Err(e) => {
                    warn!(
                        transport,
                        login_id,
                        channel,
                        session_db_id,
                        "Dangling channel — session unreadable, recreating: {e}"
                    );
                    let _ = self.detach_channel(transport, login_id, channel).await;
                }
            }
        }
        let source = format!("{transport}:{channel}");
        let (conv_id, db) = self.create_session(Some(&source)).await?;
        let session_db_id = db.root_id().to_string();
        self.attach_channel(transport, login_id, channel, &session_db_id)
            .await?;
        Ok((conv_id, db))
    }

    /// One-time migration: fold legacy Matrix-only `matrix_channels`
    /// (`room_id → session_db_id`, no login dimension) into `external_channels`
    /// under `(matrix, login_id, room_id)`.
    ///
    /// Idempotent — migrated entries are removed from the legacy store, so a
    /// second call, or a deployment that never had legacy data, is a no-op.
    /// The gateway invokes this at startup with its own login id, which is
    /// correct: legacy data predates multi-login, so all of it belonged to the
    /// single configured login. Returns the number of bindings migrated.
    pub async fn migrate_legacy_matrix_channels(&self, login_id: &str) -> anyhow::Result<usize> {
        let txn = self.chaz_group.new_transaction().await?;
        let legacy = txn
            .get_store::<DocStore>(STORE_LEGACY_MATRIX_CHANNELS)
            .await?;
        let pairs: Vec<(String, String)> = legacy
            .get_all()
            .await?
            .iter()
            .filter_map(|(room_id, v)| {
                let sid: String = v.try_into().ok()?;
                Some((room_id.clone(), sid))
            })
            .collect();
        if pairs.is_empty() {
            return Ok(0);
        }
        let external = txn.get_store::<DocStore>(STORE_EXTERNAL_CHANNELS).await?;
        for (room_id, sid) in &pairs {
            external
                .set_string(channel_key("matrix", login_id, room_id), sid)
                .await?;
            let _ = legacy.delete(room_id).await;
        }
        txn.commit().await?;
        info!(
            count = pairs.len(),
            login_id, "Migrated legacy matrix_channels → external_channels"
        );
        Ok(pairs.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_key_round_trips_addresses_containing_colons() {
        let key = channel_key("matrix", "@bot:example.com", "!room:example.com");
        let (t, l, c) = parse_channel_key(&key).unwrap();
        assert_eq!(t, "matrix");
        assert_eq!(l, "@bot:example.com");
        assert_eq!(c, "!room:example.com");
    }

    #[test]
    fn channel_keys_differ_by_login() {
        // Same room, two of our logins joined → distinct keys, distinct sessions.
        let a = channel_key("matrix", "@ava:example.com", "!room:example.com");
        let b = channel_key("matrix", "@chaz:example.com", "!room:example.com");
        assert_ne!(a, b);
    }

    #[test]
    fn parse_channel_key_rejects_foreign_shapes() {
        assert!(parse_channel_key("not json").is_none());
        assert!(parse_channel_key(r#"{"transport":"matrix"}"#).is_none());
        assert!(parse_channel_key(r#"["matrix","@bot:s"]"#).is_none()); // wrong arity
    }

    #[tokio::test]
    async fn attach_lookup_detach_round_trip() {
        let (_inst, reg) = crate::session::test_helpers::make_registry().await;
        let (_cv, db) = reg.create_session(Some("matrix:!r:s")).await.unwrap();
        let sid = db.root_id().to_string();

        reg.attach_channel("matrix", "@bot:s", "!r:s", &sid)
            .await
            .unwrap();
        assert_eq!(
            reg.external_channel_session("matrix", "@bot:s", "!r:s")
                .await
                .unwrap()
                .as_deref(),
            Some(sid.as_str())
        );
        // A different login on the same room is a distinct binding.
        assert!(
            reg.external_channel_session("matrix", "@other:s", "!r:s")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            reg.channels_for_session(&sid).await.unwrap(),
            vec![(
                "matrix".to_string(),
                "@bot:s".to_string(),
                "!r:s".to_string()
            )]
        );

        reg.detach_channel("matrix", "@bot:s", "!r:s")
            .await
            .unwrap();
        assert!(
            reg.external_channel_session("matrix", "@bot:s", "!r:s")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn migrate_legacy_folds_in_login_and_is_idempotent() {
        use eidetica::store::DocStore;
        let (_inst, reg) = crate::session::test_helpers::make_registry().await;

        // Seed a legacy room→session binding directly in the old store.
        let txn = reg.chaz_group.new_transaction().await.unwrap();
        let legacy = txn
            .get_store::<DocStore>(STORE_LEGACY_MATRIX_CHANNELS)
            .await
            .unwrap();
        legacy.set_string("!legacy:s", "sess-db-id").await.unwrap();
        txn.commit().await.unwrap();

        // First migration folds it under (matrix, login, room).
        assert_eq!(
            reg.migrate_legacy_matrix_channels("@bot:s").await.unwrap(),
            1
        );
        assert_eq!(
            reg.external_channel_session("matrix", "@bot:s", "!legacy:s")
                .await
                .unwrap()
                .as_deref(),
            Some("sess-db-id")
        );
        // Legacy store drained → a second run is a no-op.
        assert_eq!(
            reg.migrate_legacy_matrix_channels("@bot:s").await.unwrap(),
            0
        );
    }
}
