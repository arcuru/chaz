//! Agent participation on sessions (Living Agents Stages 3-4): attach/detach,
//! agent-history log, and turn-taking resolution (`resolve_agent_for_entry`).
//!
//! The session's AuthSettings is authoritative: an agent is on the session
//! iff its pubkey has `Permission::Write(_)`. `SessionMeta.agents` mirrors
//! this as a readable cache, and the agent DB's history store records each
//! attachment.

use crate::agent::Agent;
use crate::agent_db::{AgentDb, SessionHistoryEntry};
use crate::hosted_index::DbEntry;

use chrono::Utc;
use eidetica::Database;
use eidetica::auth::types::{AuthKey, Permission};
use eidetica::store::Table;
use tracing::{info, warn};

use super::{AgentRef, SessionRegistry, parse_mentions, read_meta_from_db, update_meta_on_db};

impl SessionRegistry {
    /// Attach an agent to a session. Grants the agent's pubkey Write
    /// permission on the session DB, mirrors into SessionMeta.agents, and
    /// appends to the agent DB's session-history log.
    ///
    /// Idempotent at the auth layer (set_auth_key upserts) and at the meta
    /// layer (dedup by db_id). The history log appends on every call —
    /// re-attaching a previously detached agent is a meaningful event.
    pub async fn attach_agent_to_session(
        &self,
        session_db_id: &str,
        agent: &DbEntry,
    ) -> anyhow::Result<()> {
        // 1. Session DB: grant Write permission to the agent's pubkey.
        let (_conv, session_db) = self.open_session(session_db_id).await?;
        let agent_key_name = format!("agent:{}", agent.display_name);
        {
            let txn = session_db.new_transaction().await?;
            let settings = txn.get_settings()?;
            settings
                .set_auth_key(
                    &agent.pubkey,
                    AuthKey::active(Some(&agent_key_name), Permission::Write(10)),
                )
                .await?;
            txn.commit().await?;
        }

        // 2. SessionMeta: upsert the AgentRef (dedup by db_id).
        let agent_ref = AgentRef {
            db_id: agent.db_id.to_string(),
            display_name: agent.display_name.clone(),
        };
        update_meta_on_db(&session_db, |m| {
            if let Some(existing) = m.agents.iter_mut().find(|a| a.db_id == agent_ref.db_id) {
                existing.display_name = agent_ref.display_name.clone();
            } else {
                m.agents.push(agent_ref.clone());
            }
        })
        .await?;

        // 3. Agent DB: append history entry. Best-effort — a failure here
        //    doesn't unwind the attach (the session-side change has already
        //    committed and sync'd).
        if let Err(e) = self.append_agent_history(&agent.db_id, session_db_id).await {
            warn!(
                agent = %agent.display_name,
                agent_db_id = %agent.db_id,
                session_db_id,
                "Failed to append agent history on attach: {e}"
            );
        }

        // The system prompt is set on the agent definition and passed
        // directly to the LLM — no session-level snapshot needed.

        info!(
            agent = %agent.display_name,
            agent_db_id = %agent.db_id,
            session_db_id,
            "Attached agent to session"
        );
        Ok(())
    }

    /// Detach an agent from a session. Revokes the agent's pubkey on the
    /// session DB and removes the matching AgentRef from SessionMeta.agents.
    /// The agent's history store is append-only — detach does not rewrite it.
    pub async fn detach_agent_from_session(
        &self,
        session_db_id: &str,
        agent: &DbEntry,
    ) -> anyhow::Result<()> {
        let (_conv, session_db) = self.open_session(session_db_id).await?;

        {
            let txn = session_db.new_transaction().await?;
            let settings = txn.get_settings()?;
            // `revoke_auth_key` is idempotent-ish: errors if the key isn't
            // present, so tolerate that case.
            if let Err(e) = settings.revoke_auth_key(&agent.pubkey).await {
                warn!(
                    agent = %agent.display_name,
                    "revoke_auth_key returned {e} — continuing with meta update"
                );
            }
            txn.commit().await?;
        }

        let mut cleared_host = false;
        update_meta_on_db(&session_db, |m| {
            m.agents.retain(|a| a.db_id != agent.db_id.to_string());
            // Detaching the designated host would leave a dangling
            // `host_agent_db_id` that silently falls back to the first
            // authorized agent. Clear it at the source instead.
            if m.host_agent_db_id.as_deref() == Some(agent.db_id.to_string().as_str()) {
                m.host_agent_db_id = None;
                cleared_host = true;
            }
        })
        .await?;
        if cleared_host {
            warn!(
                agent = %agent.display_name,
                session_db_id,
                "Detached agent was the session host — cleared host_agent_db_id"
            );
        }

        // No routine sweep needed — schedules are now agent-owned
        // (Schedule store in agent DB). Fire-time membership check in
        // `Server::fire_agent_schedule` handles detached agent self-skip.

        info!(
            agent = %agent.display_name,
            agent_db_id = %agent.db_id,
            session_db_id,
            "Detached agent from session"
        );
        Ok(())
    }

    /// Open the agent's DB via this user and append a SessionHistoryEntry.
    async fn append_agent_history(
        &self,
        agent_db_id: &eidetica::entry::ID,
        session_db_id: &str,
    ) -> anyhow::Result<()> {
        let user = self.user.lock().await;
        let agent_db = user.open_database(agent_db_id).await?;
        let agent_handle = AgentDb::from_database(agent_db);
        agent_handle.ensure_stores().await?;

        let txn = agent_handle.database().new_transaction().await?;
        let store = txn
            .get_store::<Table<SessionHistoryEntry>>(crate::agent_db::HISTORY_STORE)
            .await?;
        store
            .insert(SessionHistoryEntry {
                session_db_id: session_db_id.to_string(),
                joined_at: Utc::now(),
            })
            .await?;
        txn.commit().await?;
        Ok(())
    }

    /// Resolve which agent should handle a session.
    ///
    /// Priority:
    /// 1. Explicit name override (used by `!chaz run` / scheduled one-shots).
    /// 2. Key-possession routing (Stage 3c): walk the session's AuthSettings;
    ///    the first Active+Write pubkey we find in `agent_index` wins and we
    ///    resolve its display_name against the in-memory `AgentRegistry`.
    /// 3. Legacy `SessionMeta.agent_name` fallback — preserved so existing
    ///    sessions keep working until migrated.
    /// 4. Default agent.
    ///
    /// Turn-taking in multi-agent sessions (mention-based + host fallback)
    /// is Stage 4; v1 takes the first matching authorized agent.
    pub async fn resolve_agent(
        &self,
        session_db_id: &str,
        override_name: Option<&str>,
        agent_index: &crate::hosted_index::HostedIndex,
    ) -> Agent {
        if let Some(name) = override_name
            && let Some(agent) = self.agents.get(name)
        {
            return agent.clone();
        }

        let Ok((_conv_id, db)) = self.open_session(session_db_id).await else {
            return self.agents.default_agent().clone();
        };

        if let Some(agent) = self.resolve_from_auth(&db, agent_index).await {
            return agent;
        }

        let meta = read_meta_from_db(&db).await;
        if let Some(agent_name) = meta.agent_name.as_deref()
            && let Some(agent) = self.agents.get(agent_name)
        {
            return agent.clone();
        }

        self.agents.default_agent().clone()
    }

    /// Look up the first agent authorized on this session via key-possession.
    async fn resolve_from_auth(
        &self,
        session_db: &Database,
        agent_index: &crate::hosted_index::HostedIndex,
    ) -> Option<Agent> {
        let authorized = self.authorized_agents(session_db, agent_index).await;
        authorized
            .into_iter()
            .find_map(|e| self.agents.get(&e.display_name))
    }

    /// Return every agent that (a) has an Active Write key on this session
    /// and (b) exists in the peer's agent_index. Used by the mention-aware
    /// turn-taking router as the candidate set.
    async fn authorized_agents(
        &self,
        session_db: &Database,
        agent_index: &crate::hosted_index::HostedIndex,
    ) -> Vec<crate::hosted_index::DbEntry> {
        use eidetica::auth::crypto::PublicKey;
        use eidetica::auth::types::KeyStatus;

        let Ok(settings) = session_db.get_settings().await else {
            return Vec::new();
        };
        let Ok(auth) = settings.auth_snapshot().await else {
            return Vec::new();
        };
        let Ok(keys) = auth.get_all_keys() else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for (pubkey_str, key_info) in keys {
            if !matches!(key_info.status(), KeyStatus::Active) {
                continue;
            }
            if !matches!(key_info.permissions(), Permission::Write(_)) {
                continue;
            }
            let Ok(pubkey) = PublicKey::from_prefixed_string(&pubkey_str) else {
                continue;
            };
            if let Some(entry) = agent_index.find_by_pubkey(&pubkey) {
                out.push(entry);
            }
        }
        out
    }

    /// Mention-aware routing (Stage 4a). Turn precedence:
    /// 1. Explicit name override (scheduler / `/run`).
    /// 2. First `@<display_name>` token in `trigger_text` that matches an
    ///    agent authorized on the session.
    /// 3. `SessionMeta.host_agent_db_id` if it points at an authorized agent.
    /// 4. First authorized agent on the session (Stage 3c behavior).
    /// 5. Legacy `SessionMeta.agent_name`.
    /// 6. Default agent.
    pub async fn resolve_agent_for_entry(
        &self,
        session_db_id: &str,
        override_name: Option<&str>,
        agent_index: &crate::hosted_index::HostedIndex,
        trigger_text: Option<&str>,
    ) -> Agent {
        if let Some(name) = override_name
            && let Some(agent) = self.agents.get(name)
        {
            return agent.clone();
        }

        let Ok((_conv_id, db)) = self.open_session(session_db_id).await else {
            return self.agents.default_agent().clone();
        };

        let authorized = self.authorized_agents(&db, agent_index).await;

        // (2) @mention.
        if let Some(text) = trigger_text {
            let mentions = parse_mentions(text);
            for mention in &mentions {
                if let Some(entry) = authorized
                    .iter()
                    .find(|e| e.display_name.eq_ignore_ascii_case(mention))
                    && let Some(agent) = self.agents.get(&entry.display_name)
                {
                    return agent.clone();
                }
            }
            // A human addressed `@someone` but no attached agent matched.
            // The turn still routes (host / first-authorized below) — but
            // silently sending it to a different agent than the one named
            // is the Gap-2 footgun, so make the misroute observable.
            if !mentions.is_empty() {
                let roster: Vec<&str> =
                    authorized.iter().map(|e| e.display_name.as_str()).collect();
                warn!(
                    session_db_id,
                    mentioned = ?mentions,
                    attached = ?roster,
                    "No @mentioned agent is attached to this session — falling back to host/first-authorized"
                );
            }
        }

        let meta = read_meta_from_db(&db).await;

        // (3) designated host agent.
        if let Some(host_id) = meta.host_agent_db_id.as_deref()
            && let Some(entry) = authorized.iter().find(|e| e.db_id.to_string() == host_id)
            && let Some(agent) = self.agents.get(&entry.display_name)
        {
            return agent.clone();
        }

        // (4) first authorized agent.
        if let Some(entry) = authorized.first()
            && let Some(agent) = self.agents.get(&entry.display_name)
        {
            return agent.clone();
        }

        // (5) legacy agent_name.
        if let Some(name) = meta.agent_name.as_deref()
            && let Some(agent) = self.agents.get(name)
        {
            return agent.clone();
        }

        self.agents.default_agent().clone()
    }

    /// Chat-room (agent→agent) resolution. Returns the agent an `@mention`
    /// in `trigger_text` explicitly addresses, excluding `exclude_sender`
    /// so an agent cannot wake itself with a self-mention.
    ///
    /// Unlike [`Self::resolve_agent_for_entry`] there is **no** host /
    /// first-authorized / default fallback: `None` means "no attached
    /// agent was explicitly addressed, so no agent speaks". This is what
    /// keeps v1 participation strictly mention-gated — an agent's message
    /// only wakes another agent when it names one.
    pub async fn resolve_mentioned_agent(
        &self,
        session_db_id: &str,
        trigger_text: &str,
        exclude_sender: &str,
        agent_index: &crate::hosted_index::HostedIndex,
    ) -> Option<Agent> {
        let (_conv_id, db) = self.open_session(session_db_id).await.ok()?;
        let authorized = self.authorized_agents(&db, agent_index).await;
        for mention in parse_mentions(trigger_text) {
            if mention.eq_ignore_ascii_case(exclude_sender) {
                continue;
            }
            if let Some(entry) = authorized
                .iter()
                .find(|e| e.display_name.eq_ignore_ascii_case(&mention))
                && let Some(agent) = self.agents.get(&entry.display_name)
            {
                return Some(agent);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;

    #[tokio::test]
    async fn attach_agent_updates_auth_meta_and_history() {
        let (_instance, registry) = make_registry().await;
        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session_id = session_db.root_id().to_string();

        let agent = make_agent_entry(&registry, "alpha").await;
        registry
            .attach_agent_to_session(&session_id, &agent)
            .await
            .unwrap();

        // 1. Session AuthSettings now includes the agent's pubkey.
        let settings = session_db.get_settings().await.unwrap();
        let auth = settings.get_auth_key(&agent.pubkey).await.unwrap();
        assert!(matches!(auth.permissions(), Permission::Write(_)));

        // 2. SessionMeta.agents includes the AgentRef.
        let meta = read_meta_from_db(&session_db).await;
        assert_eq!(meta.agents.len(), 1);
        assert_eq!(meta.agents[0].display_name, "alpha");
        assert_eq!(meta.agents[0].db_id, agent.db_id.to_string());

        // 3. Agent's history store has one entry for this session.
        let user = registry.user.lock().await;
        let agent_db = user.open_database(&agent.db_id).await.unwrap();
        let txn = agent_db.new_transaction().await.unwrap();
        let history = txn
            .get_store::<Table<SessionHistoryEntry>>(crate::agent_db::HISTORY_STORE)
            .await
            .unwrap();
        let rows = history.search(|_| true).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.session_db_id, session_id);
    }

    #[tokio::test]
    async fn attach_is_idempotent_in_meta() {
        let (_instance, registry) = make_registry().await;
        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session_id = session_db.root_id().to_string();
        let agent = make_agent_entry(&registry, "alpha").await;

        registry
            .attach_agent_to_session(&session_id, &agent)
            .await
            .unwrap();
        registry
            .attach_agent_to_session(&session_id, &agent)
            .await
            .unwrap();

        let meta = read_meta_from_db(&session_db).await;
        assert_eq!(meta.agents.len(), 1);
    }

    #[tokio::test]
    async fn resolve_agent_via_session_auth_key() {
        let (_instance, registry, index) = make_registry_with_alpha_agent().await;
        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session_id = session_db.root_id().to_string();

        let agent_entry = make_agent_entry(&registry, "alpha").await;
        index.register(agent_entry.clone());
        registry
            .attach_agent_to_session(&session_id, &agent_entry)
            .await
            .unwrap();

        // Deliberately set a WRONG agent_name to prove the auth-based path wins.
        update_meta_on_db(&session_db, |m| {
            m.agent_name = Some("not-real".to_string());
        })
        .await
        .unwrap();

        let resolved = registry.resolve_agent(&session_id, None, &index).await;
        assert_eq!(resolved.name, "alpha");
    }

    #[tokio::test]
    async fn resolve_agent_falls_back_to_agent_name_when_no_auth_match() {
        let (_instance, registry, index) = make_registry_with_alpha_agent().await;
        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session_id = session_db.root_id().to_string();

        // No agent attached via auth. Legacy agent_name points at alpha.
        update_meta_on_db(&session_db, |m| {
            m.agent_name = Some("alpha".to_string());
        })
        .await
        .unwrap();

        let resolved = registry.resolve_agent(&session_id, None, &index).await;
        assert_eq!(resolved.name, "alpha");
    }

    #[tokio::test]
    async fn detach_removes_from_meta() {
        let (_instance, registry) = make_registry().await;
        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session_id = session_db.root_id().to_string();
        let agent = make_agent_entry(&registry, "alpha").await;

        registry
            .attach_agent_to_session(&session_id, &agent)
            .await
            .unwrap();
        registry
            .detach_agent_from_session(&session_id, &agent)
            .await
            .unwrap();

        let meta = read_meta_from_db(&session_db).await;
        assert!(meta.agents.is_empty());
    }

    #[tokio::test]
    async fn detaching_host_agent_clears_dangling_host_id() {
        let (_instance, registry) = make_registry().await;
        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session_id = session_db.root_id().to_string();
        let agent = make_agent_entry(&registry, "alpha").await;

        registry
            .attach_agent_to_session(&session_id, &agent)
            .await
            .unwrap();
        // Designate alpha as host, then detach it.
        let alpha_id = agent.db_id.to_string();
        update_meta_on_db(&session_db, |m| {
            m.host_agent_db_id = Some(alpha_id.clone());
        })
        .await
        .unwrap();

        registry
            .detach_agent_from_session(&session_id, &agent)
            .await
            .unwrap();

        let meta = read_meta_from_db(&session_db).await;
        assert!(meta.agents.is_empty());
        assert!(
            meta.host_agent_db_id.is_none(),
            "detaching the host must clear the dangling host id"
        );
    }

    #[tokio::test]
    async fn mention_routes_to_named_agent() {
        let (_instance, registry, index) = make_registry_with_two_agents().await;
        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session_id = session_db.root_id().to_string();

        let alpha = make_agent_entry(&registry, "alpha").await;
        let beta = make_agent_entry(&registry, "beta").await;
        index.register(alpha.clone());
        index.register(beta.clone());
        registry
            .attach_agent_to_session(&session_id, &alpha)
            .await
            .unwrap();
        registry
            .attach_agent_to_session(&session_id, &beta)
            .await
            .unwrap();

        // Mentioning @beta should pick beta, even though alpha was attached first.
        let resolved = registry
            .resolve_agent_for_entry(&session_id, None, &index, Some("yo @beta what's up"))
            .await;
        assert_eq!(resolved.name, "beta");

        // Mentioning @alpha picks alpha.
        let resolved = registry
            .resolve_agent_for_entry(&session_id, None, &index, Some("hey @alpha"))
            .await;
        assert_eq!(resolved.name, "alpha");
    }

    #[tokio::test]
    async fn no_mention_falls_back_to_host_agent() {
        let (_instance, registry, index) = make_registry_with_two_agents().await;
        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session_id = session_db.root_id().to_string();

        let alpha = make_agent_entry(&registry, "alpha").await;
        let beta = make_agent_entry(&registry, "beta").await;
        index.register(alpha.clone());
        index.register(beta.clone());
        registry
            .attach_agent_to_session(&session_id, &alpha)
            .await
            .unwrap();
        registry
            .attach_agent_to_session(&session_id, &beta)
            .await
            .unwrap();

        // Designate beta as host.
        let beta_db_id = beta.db_id.to_string();
        update_meta_on_db(&session_db, |m| {
            m.host_agent_db_id = Some(beta_db_id.clone());
        })
        .await
        .unwrap();

        // Plain message (no @mention) should go to the host.
        let resolved = registry
            .resolve_agent_for_entry(&session_id, None, &index, Some("hello everyone"))
            .await;
        assert_eq!(resolved.name, "beta");
    }

    #[tokio::test]
    async fn override_beats_mention() {
        let (_instance, registry, index) = make_registry_with_two_agents().await;
        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session_id = session_db.root_id().to_string();

        let alpha = make_agent_entry(&registry, "alpha").await;
        let beta = make_agent_entry(&registry, "beta").await;
        index.register(alpha.clone());
        index.register(beta.clone());
        registry
            .attach_agent_to_session(&session_id, &alpha)
            .await
            .unwrap();
        registry
            .attach_agent_to_session(&session_id, &beta)
            .await
            .unwrap();

        // Even with @beta in the text, an explicit override should win.
        let resolved = registry
            .resolve_agent_for_entry(&session_id, Some("alpha"), &index, Some("@beta help"))
            .await;
        assert_eq!(resolved.name, "alpha");
    }

    #[tokio::test]
    async fn unknown_mention_falls_through_to_first_authorized() {
        let (_instance, registry, index) = make_registry_with_two_agents().await;
        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session_id = session_db.root_id().to_string();

        let alpha = make_agent_entry(&registry, "alpha").await;
        index.register(alpha.clone());
        registry
            .attach_agent_to_session(&session_id, &alpha)
            .await
            .unwrap();

        // @gamma isn't attached; router should fall back to first authorized (alpha).
        let resolved = registry
            .resolve_agent_for_entry(&session_id, None, &index, Some("@gamma huh?"))
            .await;
        assert_eq!(resolved.name, "alpha");
    }

    #[tokio::test]
    async fn resolve_mentioned_agent_picks_named_excluding_sender() {
        let (_instance, registry, index) = make_registry_with_two_agents().await;
        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session_id = session_db.root_id().to_string();

        let alpha = make_agent_entry(&registry, "alpha").await;
        let beta = make_agent_entry(&registry, "beta").await;
        index.register(alpha.clone());
        index.register(beta.clone());
        registry
            .attach_agent_to_session(&session_id, &alpha)
            .await
            .unwrap();
        registry
            .attach_agent_to_session(&session_id, &beta)
            .await
            .unwrap();

        // alpha addresses beta — beta is woken.
        let r = registry
            .resolve_mentioned_agent(&session_id, "what do you think @beta?", "alpha", &index)
            .await;
        assert_eq!(r.map(|a| a.name), Some("beta".to_string()));

        // alpha mentions itself first, then beta — self-mention is skipped,
        // beta still wins.
        let r = registry
            .resolve_mentioned_agent(&session_id, "@alpha rambling, @beta help", "alpha", &index)
            .await;
        assert_eq!(r.map(|a| a.name), Some("beta".to_string()));
    }

    #[tokio::test]
    async fn resolve_mentioned_agent_returns_none_when_unaddressed() {
        let (_instance, registry, index) = make_registry_with_two_agents().await;
        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session_id = session_db.root_id().to_string();

        let alpha = make_agent_entry(&registry, "alpha").await;
        let beta = make_agent_entry(&registry, "beta").await;
        index.register(alpha.clone());
        index.register(beta.clone());
        registry
            .attach_agent_to_session(&session_id, &alpha)
            .await
            .unwrap();
        registry
            .attach_agent_to_session(&session_id, &beta)
            .await
            .unwrap();

        // No mention at all — nobody speaks.
        assert!(
            registry
                .resolve_mentioned_agent(&session_id, "just thinking out loud", "alpha", &index)
                .await
                .is_none()
        );

        // Only a self-mention — excluded, so nobody speaks.
        assert!(
            registry
                .resolve_mentioned_agent(&session_id, "@alpha note to self", "alpha", &index)
                .await
                .is_none()
        );

        // Stray mention of an unattached name — no fallback, nobody speaks.
        assert!(
            registry
                .resolve_mentioned_agent(&session_id, "@gamma you there?", "alpha", &index)
                .await
                .is_none()
        );
    }
}
