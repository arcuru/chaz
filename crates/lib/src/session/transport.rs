//! Transport channel bindings — `(transport, login_id, channel)` recorded in
//! the **session DB**, plus the registry-walk lookup a dumb bridge uses to map
//! an inbound channel back to its session.
//!
//! The binding moved out of the peer-local `chaz_group` (the old
//! `external_channels` index) and into each session's own DB, so it syncs with
//! the session: whichever peer holds the session — the bridge that created it
//! or the daemon that runs it — sees the same binding. A session can carry
//! several bindings (fan-out: one session surfaced in multiple rooms), keyed by
//! the `(transport, login_id, channel)` triple.
//!
//! There is no central channel→session index anymore. A bridge finds the
//! session for an inbound channel by walking its owning agent's synced session
//! registry ([`crate::agent_db::SESSIONS_STORE`]) — the sessions exposed on
//! that bridge — and reading each session DB's bindings. O(n) in the agent's
//! exposed sessions; fine at current scale, indexable later if it bites.

use crate::hosted_index::DbEntry;
use crate::types::ConversationId;

use eidetica::Database;
use eidetica::store::DocStore;
use tracing::info;

use super::registry::SessionRegistry;

/// Per-session DocStore holding this session's transport bindings, keyed by
/// [`channel_key`] → the transport name (informational; the key is the
/// authoritative, reversible record).
pub(super) const STORE_TRANSPORT_BINDINGS: &str = "transport_bindings";

/// Encode a channel identity as a stable DocStore key: JSON of the
/// `(transport, login_id, channel)` triple. JSON is used because Matrix
/// addresses (MXIDs `@u:server`, room ids `!r:server`) contain `:`, so a
/// naive `:`-joined key would be ambiguous. Reversible via [`parse_channel_key`].
fn channel_key(transport: &str, login_id: &str, channel: &str) -> String {
    serde_json::to_string(&(transport, login_id, channel))
        .expect("serializing a tuple of &str is infallible")
}

/// Reverse of [`channel_key`]. Returns `None` for keys not in the expected
/// shape, so callers skip them rather than fail.
fn parse_channel_key(key: &str) -> Option<(String, String, String)> {
    serde_json::from_str(key).ok()
}

/// Record that `session_db` is bound to `(transport, login_id, channel)`.
/// Idempotent — re-binding the same channel overwrites the same key.
pub async fn bind_transport(
    session_db: &Database,
    transport: &str,
    login_id: &str,
    channel: &str,
) -> anyhow::Result<()> {
    let txn = session_db.new_transaction().await?;
    let store = txn.get_store::<DocStore>(STORE_TRANSPORT_BINDINGS).await?;
    store
        .set_string(channel_key(transport, login_id, channel), transport)
        .await?;
    txn.commit().await?;
    info!(
        transport,
        login_id,
        channel,
        session_db_id = %session_db.root_id(),
        "Channel bound to session"
    );
    Ok(())
}

/// Drop the binding for `(transport, login_id, channel)`. Returns true if a
/// binding was present.
pub async fn unbind_transport(
    session_db: &Database,
    transport: &str,
    login_id: &str,
    channel: &str,
) -> anyhow::Result<bool> {
    let txn = session_db.new_transaction().await?;
    let store = txn.get_store::<DocStore>(STORE_TRANSPORT_BINDINGS).await?;
    let key = channel_key(transport, login_id, channel);
    let existed = store.get_string(&key).await.is_ok();
    let _ = store.delete(&key).await;
    txn.commit().await?;
    Ok(existed)
}

/// Whether `session_db` is bound to `(transport, login_id, channel)`.
pub async fn is_bound(
    session_db: &Database,
    transport: &str,
    login_id: &str,
    channel: &str,
) -> anyhow::Result<bool> {
    let txn = session_db.new_transaction().await?;
    let store = txn.get_store::<DocStore>(STORE_TRANSPORT_BINDINGS).await?;
    Ok(store
        .get_string(&channel_key(transport, login_id, channel))
        .await
        .is_ok())
}

/// Every `(transport, login_id, channel)` triple `session_db` is bound to.
pub async fn transport_bindings(
    session_db: &Database,
) -> anyhow::Result<Vec<(String, String, String)>> {
    let txn = session_db.new_transaction().await?;
    let store = txn.get_store::<DocStore>(STORE_TRANSPORT_BINDINGS).await?;
    let doc = store.get_all().await?;
    Ok(doc
        .iter()
        .filter_map(|(k, _v)| parse_channel_key(k))
        .collect())
}

impl SessionRegistry {
    /// The `(transport, login_id, channel)` bindings recorded on a session.
    /// Compat shim for command surfaces (`/info`, `/channels`) that read a
    /// session's rooms; now sourced from the session DB instead of a central
    /// index.
    pub async fn channels_for_session(
        &self,
        session_db_id: &str,
    ) -> anyhow::Result<Vec<(String, String, String)>> {
        let (_conv, db) = self.open_session(session_db_id).await?;
        transport_bindings(&db).await
    }

    /// Find the session a `(transport, login_id, channel)` is bound to, by
    /// walking `agent`'s session registry (entries exposed on `bridge_label`)
    /// and reading each session DB's bindings. Returns the opened session, or
    /// `None` if no exposed session carries the binding.
    pub async fn find_channel_session(
        &self,
        agent: &DbEntry,
        bridge_label: &str,
        transport: &str,
        login_id: &str,
        channel: &str,
    ) -> anyhow::Result<Option<(ConversationId, Database)>> {
        // `None` → `find_key`: open the agent DB under whatever key this peer
        // holds for it — the agent's own key on the daemon, the `bridge` key on
        // a bridge. Passing the agent pubkey would fail on the bridge, which
        // doesn't hold the agent's private key.
        let Some(agent_db) = self.open_agent_db(&agent.db_id, None).await? else {
            return Ok(None);
        };
        self.find_channel_session_in(&agent_db, bridge_label, transport, login_id, channel)
            .await
    }

    /// Walk an already-opened agent DB's session registry for the session bound
    /// to `(transport, login_id, channel)` and exposed on `bridge_label`. Split
    /// from [`Self::find_channel_session`] so [`Self::get_or_create_channel_session`]
    /// can open the agent DB once and re-check under its create lock.
    async fn find_channel_session_in(
        &self,
        agent_db: &crate::agent_db::AgentDb,
        bridge_label: &str,
        transport: &str,
        login_id: &str,
        channel: &str,
    ) -> anyhow::Result<Option<(ConversationId, Database)>> {
        for r in agent_db.list_session_refs().await? {
            if !r.exposed_on.iter().any(|b| b == bridge_label) {
                continue;
            }
            let Ok((conv, db)) = self.open_session(&r.session_db_id).await else {
                continue;
            };
            if is_bound(&db, transport, login_id, channel)
                .await
                .unwrap_or(false)
            {
                return Ok(Some((conv, db)));
            }
        }
        Ok(None)
    }

    /// Bridge entry point: resolve (or create) the session for a transport
    /// channel.
    ///
    /// On a hit, returns the existing session. On a miss, creates a fresh
    /// session, binds the channel into its DB, attaches `agent` as the session
    /// host (which delegates session auth to the agent DB and lists the session
    /// in the agent registry), and exposes it on `bridge_label`. The daemon's
    /// registry watch then discovers the exposed session and runs the agent —
    /// the bridge itself never does.
    pub async fn get_or_create_channel_session(
        &self,
        agent: &DbEntry,
        bridge_label: &str,
        transport: &str,
        login_id: &str,
        channel: &str,
    ) -> anyhow::Result<(ConversationId, Database)> {
        // Open the agent DB once, up front. `None` → `find_key` so the bridge
        // opens it under its own `bridge` key (it doesn't hold the agent's key);
        // see find_channel_session. A `None` here means we hold no key for the
        // agent DB yet (not synced / wrong peer) — bail rather than create:
        // creating a session we can't register in the (unreadable) agent
        // registry would fragment a channel that may already have one.
        let Some(agent_db) = self.open_agent_db(&agent.db_id, None).await? else {
            anyhow::bail!(
                "agent DB {} not openable on this peer (key not synced yet?); \
                 refusing to create a channel session that could duplicate an existing one",
                agent.db_id
            );
        };

        // Serialize get-or-create per channel: two concurrent inbound messages
        // on a brand-new channel must not both miss the walk and both create a
        // session. The lock spans the re-check below and the create.
        let lock = self
            .channel_create_lock(&channel_key(transport, login_id, channel))
            .await;
        let _guard = lock.lock().await;

        // Look up under the lock — a racing caller may have created the session
        // while we waited on the lock.
        if let Some(found) = self
            .find_channel_session_in(&agent_db, bridge_label, transport, login_id, channel)
            .await?
        {
            return Ok(found);
        }

        let source = format!("{transport}:{channel}");
        let (conv, db) = self.create_session(Some(&source)).await?;
        let session_db_id = db.root_id().to_string();

        bind_transport(&db, transport, login_id, channel).await?;
        // Attach the owning agent as host: grants its pubkey Write, delegates
        // session auth to the agent DB, and lists the session in the agent
        // registry (exposed_on empty until the expose below).
        self.ensure_session_host(&session_db_id, agent).await?;
        agent_db
            .expose_session_on(&session_db_id, bridge_label)
            .await?;
        // Enable sync for the freshly created session DB. New DBs default to
        // `sync_enabled = false`; without this the bridge won't serve the daemon's
        // sync requests for this session, so the inbound message never reaches the
        // agent and no reply comes back. Best-effort — a sync-serve failure
        // shouldn't sink the inbound message path.
        if let Err(e) = self.enable_sync_for(db.root_id()).await {
            tracing::warn!(
                session_db_id,
                error = %e,
                "failed to enable sync for channel session DB; daemon may not see inbound"
            );
        }
        Ok((conv, db))
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
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
    fn parse_channel_key_rejects_foreign_shapes() {
        assert!(parse_channel_key("not json").is_none());
        assert!(parse_channel_key(r#"{"transport":"matrix"}"#).is_none());
        assert!(parse_channel_key(r#"["matrix","@bot:s"]"#).is_none()); // wrong arity
    }

    #[tokio::test]
    async fn bind_lookup_unbind_round_trip_on_session_db() {
        let (_inst, reg) = make_registry().await;
        let (_cv, db) = reg.create_session(Some("matrix:!r:s")).await.unwrap();

        assert!(!is_bound(&db, "matrix", "@bot:s", "!r:s").await.unwrap());
        bind_transport(&db, "matrix", "@bot:s", "!r:s")
            .await
            .unwrap();
        assert!(is_bound(&db, "matrix", "@bot:s", "!r:s").await.unwrap());
        // A different login on the same room is a distinct binding.
        assert!(!is_bound(&db, "matrix", "@other:s", "!r:s").await.unwrap());
        assert_eq!(
            transport_bindings(&db).await.unwrap(),
            vec![(
                "matrix".to_string(),
                "@bot:s".to_string(),
                "!r:s".to_string()
            )]
        );

        assert!(
            unbind_transport(&db, "matrix", "@bot:s", "!r:s")
                .await
                .unwrap()
        );
        assert!(!is_bound(&db, "matrix", "@bot:s", "!r:s").await.unwrap());
        // Unbinding a non-existent binding reports false.
        assert!(
            !unbind_transport(&db, "matrix", "@bot:s", "!r:s")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn get_or_create_binds_exposes_and_resolves() {
        let (_inst, reg) = make_registry().await;
        let agent = make_agent_entry(&reg, "ava").await;
        let login = "@ava:example.com";

        // First call creates the session, binds the channel, attaches the
        // agent as host, and exposes it on the login.
        let (_cv, db) = reg
            .get_or_create_channel_session(&agent, login, "matrix", login, "!room:s")
            .await
            .unwrap();
        let sid = db.root_id().to_string();
        assert!(is_bound(&db, "matrix", login, "!room:s").await.unwrap());

        // The agent registry lists the session, exposed on this bridge.
        let agent_db = reg
            .open_agent_db(&agent.db_id, Some(&agent.pubkey))
            .await
            .unwrap()
            .unwrap();
        let r = agent_db.find_session_ref(&sid).await.unwrap().unwrap();
        assert_eq!(r.exposed_on, vec![login.to_string()]);

        // A second call resolves the *same* session rather than creating a new one.
        let (_cv2, db2) = reg
            .get_or_create_channel_session(&agent, login, "matrix", login, "!room:s")
            .await
            .unwrap();
        assert_eq!(db2.root_id().to_string(), sid);

        // Direct lookup finds it too; an unbound channel does not.
        assert!(
            reg.find_channel_session(&agent, login, "matrix", login, "!room:s")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            reg.find_channel_session(&agent, login, "matrix", login, "!other:s")
                .await
                .unwrap()
                .is_none()
        );
    }
}
