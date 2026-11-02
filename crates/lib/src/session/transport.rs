//! Transport channel bindings — `(transport, login_id, channel)` recorded in
//! the **session DB**, plus the registry-walk lookup a dumb bridge uses to map
//! an inbound channel back to its session.
//!
//! The binding moved out of the peer-local `chaz_group` (the old
//! `external_channels` index) and into each session's own DB, so it syncs with
//! the session: whichever peer holds the session — the bridge that created it
//! or the daemon that runs it — sees the same binding. New bindings permit
//! only one external attachment, keyed by `(transport, login_id, channel)`;
//! ambiguous older sessions require explicit migration.
//!
//! There is no central channel→session index anymore. A bridge finds the
//! session for an inbound channel by walking its owning agent's synced session
//! registry ([`crate::agent_db::SESSIONS_STORE`]) — the sessions exposed on
//! that bridge — and reading each session DB's bindings. O(n) in the agent's
//! exposed sessions; fine at current scale, indexable later if it bites.

use crate::hosted_index::DbEntry;
use crate::types::ConversationId;

use eidetica::Database;
use eidetica::crdt::Doc;
use eidetica::crdt::doc::Value;
use eidetica::store::DocStore;
use serde::{Deserialize, Serialize};
use tracing::info;

use super::registry::SessionRegistry;

/// Per-session DocStore holding this session's transport bindings, keyed by
/// [`channel_key`] → the transport name (informational; the key is the
/// authoritative, reversible record).
pub(super) const STORE_TRANSPORT_BINDINGS: &str = "transport_bindings";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportAttachment {
    pub transport: String,
    pub login_id: String,
    pub channel: String,
    pub conversational_replies: bool,
}

#[derive(Serialize, Deserialize)]
struct BindingCapabilities {
    conversational_replies: bool,
}

/// The sole external attachment, if any. Never guess among ambiguous legacy
/// bindings. A pre-upgrade Matrix binding has conversational replies; an old
/// Discord binding remains on its unchanged delivery path.
pub async fn session_attachment(db: &Database) -> anyhow::Result<Option<TransportAttachment>> {
    let txn = db.new_transaction().await?;
    let store = txn.get_store::<DocStore>(STORE_TRANSPORT_BINDINGS).await?;
    let bindings = binding_keys(&store.get_all().await?);
    anyhow::ensure!(
        bindings.len() <= 1,
        "session has multiple external bindings; detach or migrate explicitly"
    );
    let Some((transport, login_id, channel)) = bindings.into_iter().next() else {
        return Ok(None);
    };
    let raw = store
        .get_string(&channel_key(&transport, &login_id, &channel))
        .await?;
    let conversational_replies = if raw == transport {
        transport == "matrix" // compatibility with the already-live pilot
    } else {
        serde_json::from_str::<BindingCapabilities>(&raw)?.conversational_replies
    };
    Ok(Some(TransportAttachment {
        transport,
        login_id,
        channel,
        conversational_replies,
    }))
}

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

/// DocStore interprets dots in keys as nested paths. MXIDs and room IDs
/// contain domains, so get_all() must walk leaves and reconstruct each path.
fn binding_keys(doc: &Doc) -> Vec<(String, String, String)> {
    fn walk(doc: &Doc, prefix: &str, out: &mut Vec<(String, String, String)>) {
        for (part, value) in doc.iter() {
            let key = if prefix.is_empty() {
                part.clone()
            } else {
                format!("{prefix}.{part}")
            };
            match value {
                Value::Doc(child) => walk(child, &key, out),
                Value::Text(_) => {
                    if let Some(binding) = parse_channel_key(&key) {
                        out.push(binding);
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(doc, "", &mut out);
    out
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
    let existing = binding_keys(&store.get_all().await?);
    anyhow::ensure!(
        existing.len() <= 1
            && existing.iter().all(|binding| binding
                == &(
                    transport.to_owned(),
                    login_id.to_owned(),
                    channel.to_owned()
                )),
        "session has a different or conflicting external attachment; detach or migrate explicitly"
    );
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

/// Declare a reply-capable attachment in the session's authoritative binding.
/// Existing ambiguous sessions require explicit migration, not an arbitrary
/// destination choice.
pub async fn bind_conversational_transport(
    db: &Database,
    transport: &str,
    login_id: &str,
    channel: &str,
) -> anyhow::Result<()> {
    let txn = db.new_transaction().await?;
    let store = txn.get_store::<DocStore>(STORE_TRANSPORT_BINDINGS).await?;
    let bindings = binding_keys(&store.get_all().await?);
    anyhow::ensure!(
        bindings.len() <= 1
            && bindings.iter().all(|binding| binding
                == &(
                    transport.to_owned(),
                    login_id.to_owned(),
                    channel.to_owned()
                )),
        "session has a different or conflicting external attachment; detach or migrate explicitly"
    );
    let key = channel_key(transport, login_id, channel);
    let value = serde_json::to_string(&BindingCapabilities {
        conversational_replies: true,
    })?;
    if store.get_string(&key).await.ok().as_deref() == Some(value.as_str()) {
        return Ok(());
    }
    store.set_string(key, value).await?;
    txn.commit().await?;
    info!(transport, login_id, channel, session_db_id = %db.root_id(),
        "Channel bound to session");
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
    // Explicit removal only reduces ambiguity. Do not require the legacy set
    // to be unique before letting the operator repair it.
    let existed = binding_keys(&store.get_all().await?)
        .iter()
        .any(|(t, l, c)| t == transport && l == login_id && c == channel);
    if existed {
        store.delete(&key).await?;
        txn.commit().await?;
    }
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
    if transport == "matrix" {
        let existing: Vec<_> = binding_keys(&store.get_all().await?)
            .into_iter()
            .filter(|(kind, _, _)| kind == "matrix")
            .collect();
        anyhow::ensure!(
            existing.len() <= 1,
            "conflicting legacy Matrix bindings; explicit migration required"
        );
    }
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
    Ok(binding_keys(&doc))
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
        let mut matches = self
            .channel_sessions_in(agent_db, bridge_label, transport, login_id, channel)
            .await?;
        anyhow::ensure!(
            matches.len() <= 1,
            "channel is bound to multiple sessions: {}; use !chaz detach <exact-session-db-id> in this room",
            matches
                .iter()
                .map(|(_, db)| db.root_id().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let found = matches.pop();
        if let Some((_, db)) = &found {
            session_attachment(db).await?;
        }
        Ok(found)
    }

    /// Raw exact matches, including ambiguous legacy sessions. Only explicit
    /// detach may select from this set; ordinary routing requires uniqueness.
    async fn channel_sessions_in(
        &self,
        agent_db: &crate::agent_db::AgentDb,
        bridge_label: &str,
        transport: &str,
        login_id: &str,
        channel: &str,
    ) -> anyhow::Result<Vec<(ConversationId, Database)>> {
        let mut matches = Vec::new();
        for r in agent_db.list_session_refs().await? {
            // Local CLI/TUI sessions are not this bridge's routing surface
            // and may not grant it any key. Preserve the exposure boundary.
            if !r.exposed_on.iter().any(|b| b == bridge_label) {
                continue;
            }
            // An unreadable *exposed* session is not proof the room is free.
            let (conv, db) = self.open_session(&r.session_db_id).await?;
            if transport_bindings(&db)
                .await?
                .iter()
                .any(|(t, l, c)| t == transport && l == login_id && c == channel)
            {
                matches.push((conv, db));
            }
        }
        Ok(matches)
    }

    /// Explicit attach shares the inbound create lock: a room already bound
    /// elsewhere refuses, rather than becoming a second route to its history.
    pub async fn attach_conversational_channel_session(
        &self,
        agent: &DbEntry,
        bridge_label: &str,
        transport: &str,
        login_id: &str,
        channel: &str,
        target_db: &Database,
    ) -> anyhow::Result<()> {
        let agent_db = self
            .open_agent_db(&agent.db_id, None)
            .await?
            .ok_or_else(|| anyhow::anyhow!("owning agent DB not available"))?;
        let lock = self
            .channel_create_lock(&channel_key(transport, login_id, channel))
            .await;
        let _guard = lock.lock().await;
        let session_lock = self
            .channel_create_lock(&format!("session:{}", target_db.root_id()))
            .await;
        let _session_guard = session_lock.lock().await;
        if let Some((_, existing)) = self
            .find_channel_session_in(&agent_db, bridge_label, transport, login_id, channel)
            .await?
        {
            anyhow::ensure!(
                existing.root_id() == target_db.root_id(),
                "room is already bound to session {}; detach first",
                existing.root_id()
            );
        }
        bind_conversational_transport(target_db, transport, login_id, channel).await?;
        self.ensure_session_host(&target_db.root_id().to_string(), agent)
            .await?;
        self.publish_channel_session(&agent_db, target_db.root_id(), bridge_label)
            .await
    }

    /// Remove exactly this channel, even from a legacy multi-bound session.
    /// A room bound to multiple sessions requires an exact DB ID, never a name
    /// or registry-order winner. Returns the removed session ID, if present.
    pub async fn detach_channel_session(
        &self,
        agent: &DbEntry,
        bridge_label: &str,
        transport: &str,
        login_id: &str,
        channel: &str,
        session_db_id: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        let agent_db = self
            .open_agent_db(&agent.db_id, None)
            .await?
            .ok_or_else(|| anyhow::anyhow!("owning agent DB not available"))?;
        let lock = self
            .channel_create_lock(&channel_key(transport, login_id, channel))
            .await;
        let _guard = lock.lock().await;
        let mut matches = self
            .channel_sessions_in(&agent_db, bridge_label, transport, login_id, channel)
            .await?;
        let selected = if let Some(id) = session_db_id {
            Some(
                matches
                    .iter()
                    .position(|(_, db)| db.root_id().to_string() == id)
                    .ok_or_else(|| {
                        anyhow::anyhow!("session {id} is not bound to this room on this login")
                    })?,
            )
        } else {
            anyhow::ensure!(
                matches.len() <= 1,
                "channel is bound to multiple sessions: {}; use !chaz detach <exact-session-db-id> in this room",
                matches
                    .iter()
                    .map(|(_, db)| db.root_id().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            (!matches.is_empty()).then_some(0)
        };
        let Some(index) = selected else {
            return Ok(None);
        };
        let (_, db) = matches.swap_remove(index);
        let sid = db.root_id().to_string();
        let session_lock = self.channel_create_lock(&format!("session:{sid}")).await;
        let _session_guard = session_lock.lock().await;
        unbind_transport(&db, transport, login_id, channel).await?;
        if !transport_bindings(&db)
            .await?
            .iter()
            .any(|(_, login, _)| login == login_id)
        {
            agent_db.unexpose_session_from(&sid, bridge_label).await?;
        }
        Ok(Some(sid))
    }

    /// Strict workspace lookup: prove a unique binding across the agent's
    /// explicitly exposed sessions. An unreadable candidate is not absence.
    /// Ordinary rooms retain their existing lookup policy.
    pub async fn find_unique_channel_session(
        &self,
        agent: &DbEntry,
        bridge_label: &str,
        transport: &str,
        login_id: &str,
        channel: &str,
    ) -> anyhow::Result<Option<(ConversationId, Database)>> {
        let agent_db = self
            .open_agent_db(&agent.db_id, None)
            .await?
            .ok_or_else(|| anyhow::anyhow!("owning agent DB is unavailable"))?;
        let mut found = None;
        for r in agent_db.list_session_refs().await? {
            if !r.exposed_on.iter().any(|b| b == bridge_label) {
                continue;
            }
            let (conv, db) = self.open_session(&r.session_db_id).await?;
            let attachment = session_attachment(&db).await?;
            if attachment.as_ref().is_some_and(|a| {
                a.transport == transport
                    && a.login_id == login_id
                    && a.channel == channel
                    && a.conversational_replies
            }) {
                anyhow::ensure!(
                    found.is_none(),
                    "multiple sessions bind this channel; refusing to choose"
                );
                found = Some((conv, db));
            }
        }
        Ok(found)
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
        self.get_or_create_channel_session_with_capabilities(
            agent,
            bridge_label,
            transport,
            login_id,
            channel,
            false,
        )
        .await
    }

    pub async fn get_or_create_conversational_channel_session(
        &self,
        agent: &DbEntry,
        bridge_label: &str,
        transport: &str,
        login_id: &str,
        channel: &str,
    ) -> anyhow::Result<(ConversationId, Database)> {
        self.get_or_create_channel_session_with_capabilities(
            agent,
            bridge_label,
            transport,
            login_id,
            channel,
            true,
        )
        .await
    }

    async fn get_or_create_channel_session_with_capabilities(
        &self,
        agent: &DbEntry,
        bridge_label: &str,
        transport: &str,
        login_id: &str,
        channel: &str,
        conversational_replies: bool,
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
            if conversational_replies {
                bind_conversational_transport(&found.1, transport, login_id, channel).await?;
            }
            self.publish_channel_session(&agent_db, found.1.root_id(), bridge_label)
                .await?;
            tracing::debug!(
                session_db_id = %found.1.root_id(),
                transport,
                login_id,
                channel,
                "Resolved existing transport channel session"
            );
            return Ok(found);
        }

        let source = format!("{transport}:{channel}");
        let (conv, db) = self.create_session(Some(&source)).await?;
        let session_db_id = db.root_id().to_string();

        tracing::debug!(
            session_db_id,
            transport,
            login_id,
            channel,
            "Created transport channel session"
        );

        if conversational_replies {
            bind_conversational_transport(&db, transport, login_id, channel).await?;
        } else {
            bind_transport(&db, transport, login_id, channel).await?;
        }
        // Attach the owning agent as host: grants its pubkey Write, delegates
        // session auth to the agent DB, and lists the session in the agent
        // registry (exposed_on empty until the expose below).
        self.ensure_session_host(&session_db_id, agent).await?;
        self.publish_channel_session(&agent_db, db.root_id(), bridge_label)
            .await?;
        Ok((conv, db))
    }

    /// Provision the one conversational session for a freshly created
    /// channel, identified by a caller-supplied unique provisioning `source`
    /// marker rather than by channel lookup alone.
    ///
    /// Recovery-safe: a DB already carrying `source` (for example one created
    /// before an interruption lost its ID) is reused instead of creating a
    /// second; more than one carrier, or a channel already bound to a
    /// different session, refuses rather than choosing. On success the
    /// channel is bound, `agent` hosts the session and it is published on
    /// `bridge_label`, exactly like [`Self::get_or_create_conversational_channel_session`].
    #[allow(clippy::too_many_arguments)]
    pub async fn provision_marked_conversational_channel_session(
        &self,
        agent: &DbEntry,
        bridge_label: &str,
        transport: &str,
        login_id: &str,
        channel: &str,
        source: &str,
        allow_create: bool,
    ) -> anyhow::Result<(ConversationId, Database)> {
        let Some(agent_db) = self.open_agent_db(&agent.db_id, None).await? else {
            anyhow::bail!(
                "agent DB {} not openable on this peer; refusing to provision a channel session",
                agent.db_id
            );
        };
        let lock = self
            .channel_create_lock(&channel_key(transport, login_id, channel))
            .await;
        let _guard = lock.lock().await;
        let mut marked = self.find_sessions_by_source(source).await?;
        anyhow::ensure!(
            marked.len() <= 1,
            "{} sessions carry provisioning marker {source}; refusing to choose one",
            marked.len()
        );
        let marked = marked.pop();
        let bound = self
            .find_unique_channel_session(agent, bridge_label, transport, login_id, channel)
            .await?;
        let (conv, db) = match (marked, bound) {
            (Some(id), Some(found)) if found.1.root_id().to_string() == id => found,
            (_, Some(found)) => anyhow::bail!(
                "channel is already bound to session {}; refusing to provision another",
                found.1.root_id()
            ),
            (Some(id), None) => self.open_session(&id).await?,
            (None, None) if allow_create => self.create_session(Some(source)).await?,
            (None, None) => anyhow::bail!(
                "session creation outcome is unknown; no marked DB is visible; refusing a replacement"
            ),
        };
        // A recovered DB may predate every post-creation initialization write.
        // Finish those writes before exposing it; never reset an existing turn
        // baseline or transcript.
        super::Session::try_new(conv.clone(), db.clone())
            .await?
            .ensure_turn_schema_baseline()
            .await?;
        crate::db_kind::write_marker(&db, crate::db_kind::KIND_SESSION, source).await?;
        self.upsert_session_catalog(&db.root_id().to_string(), Some(source), None)
            .await?;
        bind_conversational_transport(&db, transport, login_id, channel).await?;
        self.ensure_session_host(&db.root_id().to_string(), agent)
            .await?;
        // Workspace readiness must not swallow a sync-publication failure.
        self.enable_sync_for(db.root_id()).await?;
        agent_db
            .expose_session_on(&db.root_id().to_string(), bridge_label)
            .await?;
        Ok((conv, db))
    }

    /// Make a channel session available to the daemon before notifying it
    /// through the agent DB registry. This also refreshes an existing
    /// session's exposure after bridge restart or state recovery: the registry
    /// write wakes the daemon's watch, while on-commit sync makes the session
    /// tree serveable when it tries to pull it.
    async fn publish_channel_session(
        &self,
        agent_db: &crate::agent_db::AgentDb,
        session_db_id: &eidetica::entry::ID,
        bridge_label: &str,
    ) -> anyhow::Result<()> {
        // Enable sync for the channel session DB. New DBs default to
        // `sync_enabled = false`; recovered sessions can have the same stale
        // setting. Without this the bridge won't serve the daemon's sync
        // requests for the session, so the inbound message never reaches the
        // agent and no reply comes back. Best-effort — a sync-serve failure
        // shouldn't sink the inbound message path.
        if let Err(e) = self.enable_sync_for(session_db_id).await {
            tracing::warn!(
                session_db_id = %session_db_id,
                error = %e,
                "failed to enable sync for channel session DB; daemon may not see inbound"
            );
        }
        agent_db
            .expose_session_on(&session_db_id.to_string(), bridge_label)
            .await
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
        // Dot-containing Matrix domains become nested DocStore paths on sync.
        // Binding enumeration must reconstruct the original JSON key.
        unbind_transport(&db, "matrix", "@bot:s", "!r:s")
            .await
            .unwrap();
        bind_transport(&db, "matrix", "@bot:example.com", "!r:example.com")
            .await
            .unwrap();
        assert_eq!(
            transport_bindings(&db).await.unwrap(),
            vec![(
                "matrix".into(),
                "@bot:example.com".into(),
                "!r:example.com".into()
            )]
        );
        unbind_transport(&db, "matrix", "@bot:example.com", "!r:example.com")
            .await
            .unwrap();
        bind_transport(&db, "matrix", "@bot:s", "!r:s")
            .await
            .unwrap();
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
    async fn matrix_binding_rejects_other_rooms_and_legacy_conflicts() {
        let (_inst, reg) = make_registry().await;
        let (_cv, db) = reg.create_session(Some("local")).await.unwrap();
        bind_transport(&db, "matrix", "@a:s", "!a:s").await.unwrap();
        bind_transport(&db, "matrix", "@a:s", "!a:s").await.unwrap();
        assert_eq!(
            bind_transport(&db, "matrix", "@b:s", "!a:s")
                .await
                .unwrap_err()
                .to_string(),
            "session has a different or conflicting external attachment; detach or migrate explicitly"
        );
        assert_eq!(
            bind_transport(&db, "matrix", "@a:s", "!b:s")
                .await
                .unwrap_err()
                .to_string(),
            "session has a different or conflicting external attachment; detach or migrate explicitly"
        );
        assert_eq!(
            bind_transport(&db, "discord", "bot", "room")
                .await
                .unwrap_err()
                .to_string(),
            "session has a different or conflicting external attachment; detach or migrate explicitly"
        );
        let current = session_attachment(&db).await.unwrap().unwrap();
        assert!(current.conversational_replies);
        let txn = db.new_transaction().await.unwrap();
        txn.get_store::<DocStore>(STORE_TRANSPORT_BINDINGS)
            .await
            .unwrap()
            .set_string(channel_key("discord", "bot", "room"), "discord")
            .await
            .unwrap();
        txn.commit().await.unwrap();
        assert_eq!(
            session_attachment(&db).await.unwrap_err().to_string(),
            "session has multiple external bindings; detach or migrate explicitly"
        );
        let txn = db.new_transaction().await.unwrap();
        txn.get_store::<DocStore>(STORE_TRANSPORT_BINDINGS)
            .await
            .unwrap()
            .set_string(channel_key("matrix", "@legacy:s", "!legacy:s"), "matrix")
            .await
            .unwrap();
        txn.commit().await.unwrap();
        assert_eq!(
            is_bound(&db, "matrix", "@a:s", "!a:s")
                .await
                .unwrap_err()
                .to_string(),
            "conflicting legacy Matrix bindings; explicit migration required"
        );
        assert_eq!(
            bind_transport(&db, "matrix", "@a:s", "!a:s")
                .await
                .unwrap_err()
                .to_string(),
            "session has a different or conflicting external attachment; detach or migrate explicitly"
        );
        assert!(
            unbind_transport(&db, "matrix", "@a:s", "!a:s")
                .await
                .unwrap()
        );
    }

    async fn legacy_bind(db: &Database, login: &str, room: &str) {
        let txn = db.new_transaction().await.unwrap();
        txn.get_store::<DocStore>(STORE_TRANSPORT_BINDINGS)
            .await
            .unwrap()
            .set_string(channel_key("matrix", login, room), "matrix")
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }

    #[tokio::test]
    async fn legacy_multi_room_lookup_refuses_without_creating_or_hiding_bindings() {
        let (_inst, reg) = make_registry().await;
        let agent = make_agent_entry(&reg, "chaz").await;
        let login = "@chaz:example.com";
        let (_, db) = reg
            .get_or_create_channel_session(&agent, login, "matrix", login, "!one:example.com")
            .await
            .unwrap();
        legacy_bind(&db, login, "!two:example.com").await;
        let agent_db = reg
            .open_agent_db(&agent.db_id, None)
            .await
            .unwrap()
            .unwrap();
        let before = agent_db.list_session_refs().await.unwrap();
        assert_eq!(transport_bindings(&db).await.unwrap().len(), 2);
        for room in ["!one:example.com", "!two:example.com"] {
            assert!(
                reg.find_channel_session(&agent, login, "matrix", login, room)
                    .await
                    .is_err(),
                "legacy ambiguity must not be reported as an absent binding"
            );
            assert!(
                reg.get_or_create_conversational_channel_session(
                    &agent, login, "matrix", login, room,
                )
                .await
                .is_err(),
                "ambiguity must not mint a new session"
            );
        }
        assert_eq!(
            agent_db.list_session_refs().await.unwrap().len(),
            before.len()
        );
        assert_eq!(transport_bindings(&db).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn duplicate_room_lookup_refuses_without_registry_order_winner_or_creation() {
        let (_inst, reg) = make_registry().await;
        let agent = make_agent_entry(&reg, "chaz").await;
        let login = "@chaz:example.com";
        let room = "!one:example.com";
        let (_, first) = reg
            .get_or_create_channel_session(&agent, login, "matrix", login, room)
            .await
            .unwrap();
        let (_, second) = reg.create_session(Some("legacy-duplicate")).await.unwrap();
        legacy_bind(&second, login, room).await;
        reg.ensure_session_host(&second.root_id().to_string(), &agent)
            .await
            .unwrap();
        let agent_db = reg
            .open_agent_db(&agent.db_id, None)
            .await
            .unwrap()
            .unwrap();
        agent_db
            .expose_session_on(&second.root_id().to_string(), login)
            .await
            .unwrap();
        let error = reg
            .find_channel_session(&agent, login, "matrix", login, room)
            .await
            .expect_err("must not choose by registry order")
            .to_string();
        assert!(error.contains(&first.root_id().to_string()), "{error}");
        assert!(error.contains(&second.root_id().to_string()), "{error}");
        assert!(
            reg.get_or_create_channel_session(&agent, login, "matrix", login, room)
                .await
                .is_err()
        );
        assert_eq!(agent_db.list_session_refs().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn legacy_unbind_reduces_ambiguity_and_preserves_remaining_history() {
        let (_inst, reg) = make_registry().await;
        let (conv, db) = reg.create_session(Some("legacy")).await.unwrap();
        let mut session = super::super::Session::new(conv.clone(), db.clone()).await;
        session
            .add_entry(super::super::SessionEntry::new_bridge_event(
                "human",
                super::super::BridgeEventRole::Observation,
                "retained history",
                None,
            ))
            .await
            .unwrap();
        legacy_bind(&db, "@bot:example.com", "!one:example.com").await;
        legacy_bind(&db, "@bot:example.com", "!two:example.com").await;
        assert!(
            unbind_transport(&db, "matrix", "@bot:example.com", "!two:example.com")
                .await
                .unwrap()
        );
        assert_eq!(
            session_attachment(&db).await.unwrap().unwrap().channel,
            "!one:example.com"
        );
        let reloaded = super::super::Session::new(conv, db).await;
        assert!(
            reloaded
                .entries()
                .iter()
                .any(|e| e.bridge_body().as_deref() == Some("retained history"))
        );
    }

    #[tokio::test]
    async fn attach_refuses_bound_room_and_repeated_exact_attach_is_safe() {
        let (_inst, reg) = make_registry_with_sync().await;
        let agent = make_agent_entry(&reg, "chaz").await;
        let login = "@bot:example.com";
        let room = "!one:example.com";
        let (_, first) = reg
            .get_or_create_conversational_channel_session(&agent, login, "matrix", login, room)
            .await
            .unwrap();
        let (_, target) = reg.create_session(Some("local")).await.unwrap();
        let error = reg
            .attach_conversational_channel_session(&agent, login, "matrix", login, room, &target)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("detach first"), "{error}");
        assert!(transport_bindings(&target).await.unwrap().is_empty());
        for _ in 0..2 {
            reg.attach_conversational_channel_session(&agent, login, "matrix", login, room, &first)
                .await
                .unwrap();
        }
        assert_eq!(transport_bindings(&first).await.unwrap().len(), 1);
        assert_eq!(
            reg.find_channel_session(&agent, login, "matrix", login, room)
                .await
                .unwrap()
                .unwrap()
                .1
                .root_id(),
            first.root_id()
        );
        // No two concurrent rooms can claim the same previously local session.
        let (one, two) = tokio::join!(
            reg.attach_conversational_channel_session(
                &agent,
                login,
                "matrix",
                login,
                "!two:example.com",
                &target
            ),
            reg.attach_conversational_channel_session(
                &agent,
                login,
                "matrix",
                login,
                "!three:example.com",
                &target
            ),
        );
        assert_ne!(one.is_ok(), two.is_ok());
        assert_eq!(transport_bindings(&target).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn explicit_legacy_detach_preserves_sibling_exposure_and_exact_identity() {
        let (_inst, reg) = make_registry_with_sync().await;
        let agent = make_agent_entry(&reg, "chaz").await;
        let login = "@bot:example.com";
        let one = "!one:example.com";
        let two = "!two:example.com";
        let (_, original) = reg
            .get_or_create_channel_session(&agent, login, "matrix", login, one)
            .await
            .unwrap();
        legacy_bind(&original, login, two).await;
        let sid = original.root_id().to_string();
        let agent_db = reg
            .open_agent_db(&agent.db_id, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reg.detach_channel_session(&agent, login, "matrix", login, two, None)
                .await
                .unwrap(),
            Some(sid.clone())
        );
        assert_eq!(
            agent_db
                .find_session_ref(&sid)
                .await
                .unwrap()
                .unwrap()
                .exposed_on,
            [login]
        );
        assert_eq!(
            reg.get_or_create_channel_session(&agent, login, "matrix", login, one)
                .await
                .unwrap()
                .1
                .root_id(),
            original.root_id()
        );
        // Simulate a separate legacy session mapping to the *same* exact room.
        let (_, duplicate) = reg.create_session(Some("legacy-duplicate")).await.unwrap();
        legacy_bind(&duplicate, login, one).await;
        reg.ensure_session_host(&duplicate.root_id().to_string(), &agent)
            .await
            .unwrap();
        agent_db
            .expose_session_on(&duplicate.root_id().to_string(), login)
            .await
            .unwrap();
        assert!(
            reg.detach_channel_session(&agent, login, "matrix", login, one, None)
                .await
                .is_err()
        );
        assert!(
            reg.detach_channel_session(&agent, login, "matrix", login, one, Some("wrong-id"))
                .await
                .is_err()
        );
        assert_eq!(transport_bindings(&original).await.unwrap().len(), 1);
        let duplicate_sid = duplicate.root_id().to_string();
        assert_eq!(
            reg.detach_channel_session(&agent, login, "matrix", login, one, Some(&duplicate_sid))
                .await
                .unwrap(),
            Some(duplicate_sid.clone())
        );
        assert!(
            agent_db
                .find_session_ref(&duplicate_sid)
                .await
                .unwrap()
                .unwrap()
                .exposed_on
                .is_empty()
        );
        assert_eq!(
            reg.find_channel_session(&agent, login, "matrix", login, one)
                .await
                .unwrap()
                .unwrap()
                .1
                .root_id(),
            original.root_id()
        );
        assert_eq!(agent_db.list_session_refs().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn detach_propagates_write_errors_instead_of_claiming_success() {
        use eidetica::auth::types::{AuthKey, Permission};
        let (_inst, reg) = make_registry().await;
        let (_, db) = reg.create_session(Some("read-only")).await.unwrap();
        legacy_bind(&db, "@bot:example.com", "!one:example.com").await;
        let reader_key = reg
            .user
            .lock()
            .await
            .add_private_key(Some("reader"))
            .await
            .unwrap();
        let txn = db.new_transaction().await.unwrap();
        txn.get_settings()
            .unwrap()
            .set_auth_key(
                &reader_key,
                AuthKey::active(Some("reader"), Permission::Read),
            )
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let private = reg.user.lock().await.get_signing_key(&reader_key).unwrap();
        let (identity, _) = Database::find_sigkeys(reg.instance(), db.root_id(), &reader_key)
            .await
            .unwrap()
            .remove(0);
        let reader = db
            .clone()
            .with_key(eidetica::database::DatabaseKey::with_identity(
                private, identity,
            ));
        assert!(
            unbind_transport(&reader, "matrix", "@bot:example.com", "!one:example.com")
                .await
                .is_err()
        );
        assert_eq!(transport_bindings(&db).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unexposed_unreadable_local_sessions_do_not_block_bridge_routing() {
        let (_inst, reg) = make_registry().await;
        let agent = make_agent_entry(&reg, "chaz").await;
        let (_foreign, _user, local_only) = test_session_db().await;
        let agent_db = reg
            .open_agent_db(&agent.db_id, None)
            .await
            .unwrap()
            .unwrap();
        let local_id = local_only.root_id().to_string();
        agent_db
            .register_session_ref(crate::agent_db::SessionRef {
                session_db_id: local_id.clone(),
                exposed_on: vec![],
            })
            .await
            .unwrap();
        let login = "@bot:example.com";
        let room = "!one:example.com";
        let (_, db) = reg
            .get_or_create_channel_session(&agent, login, "matrix", login, room)
            .await
            .unwrap();
        assert_ne!(db.root_id(), local_only.root_id());
        assert_eq!(
            reg.find_channel_session(&agent, login, "matrix", login, room)
                .await
                .unwrap()
                .unwrap()
                .1
                .root_id(),
            db.root_id()
        );
        // In contrast, an unreadable exposed session must fail closed, not be
        // treated as absence followed by another implicit channel creation.
        agent_db.expose_session_on(&local_id, login).await.unwrap();
        assert!(
            reg.get_or_create_channel_session(&agent, login, "matrix", login, "!two:example.com")
                .await
                .is_err()
        );
        assert_eq!(agent_db.list_session_refs().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn get_or_create_binds_exposes_and_resolves() {
        let (_inst, reg) = make_registry().await;
        let agent = make_agent_entry(&reg, "chaz").await;
        let login = "@chaz:example.com";

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

    #[tokio::test]
    async fn marked_recovery_completes_interrupted_initialization() {
        let (_inst, reg) = make_registry().await;
        let agent = make_agent_entry(&reg, "chaz").await;
        let source = "matrix-workspace:recovery:op-2";
        // Crash immediately after DB creation, before schema, entity marker
        // and local catalog registration. The creation settings survive.
        let orphan = {
            let mut user = reg.user.lock().await;
            let mut settings = eidetica::crdt::Doc::new();
            settings.set("name", format!("session:{source}"));
            let key = user.get_default_key().unwrap();
            user.create_database(settings, &key).await.unwrap()
        };
        let (_, recovered) = reg
            .provision_marked_conversational_channel_session(
                &agent, "login", "matrix", "login", "!new:s", source, false,
            )
            .await
            .unwrap();
        assert_eq!(recovered.root_id(), orphan.root_id());
        assert!(
            reg.list_sessions()
                .await
                .unwrap()
                .iter()
                .any(|s| s.session_db_id == orphan.root_id().to_string()),
            "recovered DB must finish catalog registration"
        );
        assert_eq!(
            crate::db_kind::read_marker(&recovered).await.unwrap().0,
            crate::db_kind::KIND_SESSION
        );
        let txn = recovered.new_transaction().await.unwrap();
        assert_eq!(
            txn.get_store::<DocStore>("meta")
                .await
                .unwrap()
                .get_string("turn_request_schema")
                .await
                .unwrap(),
            "1"
        );
    }

    #[tokio::test]
    async fn workspace_lookup_rejects_two_sessions_bound_to_one_room() {
        let (_inst, reg) = make_registry().await;
        let agent = make_agent_entry(&reg, "chaz").await;
        for source in ["one", "two"] {
            let (_, db) = reg.create_session(Some(source)).await.unwrap();
            bind_conversational_transport(&db, "matrix", "login", "!room:s")
                .await
                .unwrap();
            reg.ensure_session_host(&db.root_id().to_string(), &agent)
                .await
                .unwrap();
            let adb = reg
                .open_agent_db(&agent.db_id, None)
                .await
                .unwrap()
                .unwrap();
            adb.expose_session_on(&db.root_id().to_string(), "login")
                .await
                .unwrap();
        }
        let result = reg
            .find_unique_channel_session(&agent, "login", "matrix", "login", "!room:s")
            .await;
        assert!(result.is_err(), "must not choose the first of two sessions");
        let before = reg.list_sessions().await.unwrap().len();
        let err = reg
            .provision_marked_conversational_channel_session(
                &agent,
                "login",
                "matrix",
                "login",
                "!missing:s",
                "unseen-marker",
                false,
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("multiple sessions") || err.to_string().contains("unknown"),
            "{err}"
        );
        assert_eq!(reg.list_sessions().await.unwrap().len(), before);
        let adb = reg
            .open_agent_db(&agent.db_id, None)
            .await
            .unwrap()
            .unwrap();
        for r in adb.list_session_refs().await.unwrap() {
            adb.deregister_session_ref(&r.session_db_id).await.unwrap();
        }
        let err = reg
            .provision_marked_conversational_channel_session(
                &agent,
                "login",
                "matrix",
                "login",
                "!missing:s",
                "unseen-marker",
                false,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
        adb.register_session_ref(crate::agent_db::SessionRef {
            session_db_id: "unreadable".into(),
            exposed_on: vec!["login".into()],
        })
        .await
        .unwrap();
        assert!(
            reg.find_unique_channel_session(&agent, "login", "matrix", "login", "!missing:s")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn marked_provisioning_recovers_the_same_db_and_refuses_ambiguity() {
        let (_inst, reg) = make_registry().await;
        let agent = make_agent_entry(&reg, "chaz").await;
        let login = "@chaz:example.com";
        let marker = "matrix-workspace:ws1:op-2";

        // Interrupted run: the DB exists with the marker, but its ID was lost
        // before it was bound or recorded anywhere.
        let (_cv, orphan) = reg.create_session(Some(marker)).await.unwrap();
        let orphan_id = orphan.root_id().to_string();
        assert_eq!(
            reg.find_sessions_by_source(marker).await.unwrap(),
            vec![orphan_id.clone()]
        );

        let (_cv, db) = reg
            .provision_marked_conversational_channel_session(
                &agent, login, "matrix", login, "!new:s", marker, true,
            )
            .await
            .unwrap();
        assert_eq!(
            db.root_id().to_string(),
            orphan_id,
            "recovered, not duplicated"
        );
        assert!(is_bound(&db, "matrix", login, "!new:s").await.unwrap());
        let agent_db = reg
            .open_agent_db(&agent.db_id, Some(&agent.pubkey))
            .await
            .unwrap()
            .unwrap();
        let r = agent_db
            .find_session_ref(&orphan_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.exposed_on, vec![login.to_string()]);

        // Repeating is idempotent.
        let (_cv, again) = reg
            .provision_marked_conversational_channel_session(
                &agent, login, "matrix", login, "!new:s", marker, true,
            )
            .await
            .unwrap();
        assert_eq!(again.root_id().to_string(), orphan_id);
        assert_eq!(reg.find_sessions_by_source(marker).await.unwrap().len(), 1);

        // A fresh marker never adopts the room's existing session.
        let err = reg
            .provision_marked_conversational_channel_session(
                &agent,
                login,
                "matrix",
                login,
                "!new:s",
                "matrix-workspace:ws1:op-3",
                true,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already bound"), "{err}");

        // Two carriers of one marker refuse instead of choosing.
        let dup = "matrix-workspace:ws1:op-4";
        reg.create_session(Some(dup)).await.unwrap();
        reg.create_session(Some(dup)).await.unwrap();
        let err = reg
            .provision_marked_conversational_channel_session(
                &agent, login, "matrix", login, "!dup:s", dup, true,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("refusing to choose"), "{err}");
        assert!(
            reg.find_channel_session(&agent, login, "matrix", login, "!dup:s")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn existing_channel_session_refreshes_exposure_and_sync() {
        let (_inst, reg) = make_registry_with_sync().await;
        let agent = make_agent_entry(&reg, "chaz").await;
        let login = "@chaz:example.com";

        let (_cv, db) = reg
            .get_or_create_channel_session(&agent, login, "matrix", login, "!room:s")
            .await
            .unwrap();
        let sid = db.root_id().clone();
        reg.disable_sync_for(&sid).await.unwrap();

        let agent_db = reg
            .open_agent_db(&agent.db_id, Some(&agent.pubkey))
            .await
            .unwrap()
            .unwrap();
        let writes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let writes_for_callback = writes.clone();
        let subscription = agent_db
            .database()
            .on_write(move |_, _| {
                let writes = writes_for_callback.clone();
                Box::pin(async move {
                    writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
            })
            .await
            .unwrap();
        subscription.detach();

        let (_cv, resolved) = reg
            .get_or_create_channel_session(&agent, login, "matrix", login, "!room:s")
            .await
            .unwrap();

        assert_eq!(resolved.root_id(), &sid);
        assert!(reg.is_sync_enabled_for(&sid).await.unwrap());
        assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
