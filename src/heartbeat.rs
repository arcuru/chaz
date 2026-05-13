//! Heartbeat rules — Living Agents Stage 4b.
//!
//! A heartbeat rule is a cron-driven trigger stored *inside* a session's DB.
//! Each peer periodically scans every session it knows about, looks at the
//! rules in that session, and fires any that are both (a) due under their
//! cron and (b) targeted at an agent this peer hosts (i.e. whose pubkey
//! appears in the local `agent_index`).
//!
//! Firing a rule writes a Directive entry to the session — the same
//! mechanism `spawn_agent` and the config scheduler already use. The
//! server callback path then hands the directive to the target agent via
//! the Stage 4a mention-aware router (rules can say `@agent ...` in the
//! `task` text to pin the turn).
//!
//! `last_fired` is peer-local (`chaz_peer.heartbeat_last_fired`) rather than
//! a rule-DB field because multiple peers may host the same rule's target
//! agent, and each peer fires independently. Keeping `last_fired` out of the
//! synced DB avoids cross-peer fire-coordination churn.

#![allow(dead_code)]

use crate::hosted_index::HostedIndex;
use crate::server::Server;
use crate::session::{EntryType, Session, SessionEntry};
use crate::types::ConversationId;
use chrono::{DateTime, Utc};
use cron::Schedule;
use eidetica::Database;
use eidetica::store::{DocStore, Table};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

pub const RULES_STORE: &str = "rules";
const LAST_FIRED_STORE: &str = "heartbeat_last_fired";

/// A cron rule living inside a session's `rules` table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HeartbeatRule {
    pub id: String,
    pub name: String,
    pub cron: String,
    pub task: String,
    pub target_agent_db_id: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// Upsert a rule by `id` into the session's rules table.
pub async fn upsert_rule(session_db: &Database, rule: HeartbeatRule) -> anyhow::Result<()> {
    let txn = session_db.new_transaction().await?;
    let store = txn.get_store::<Table<HeartbeatRule>>(RULES_STORE).await?;
    // Upsert keyed by rule.id — search for existing, then set or insert.
    let existing = store.search(|r| r.id == rule.id).await?;
    if let Some((key, _)) = existing.into_iter().next() {
        store.set(&key, rule).await?;
    } else {
        store.insert(rule).await?;
    }
    txn.commit().await?;
    Ok(())
}

/// Remove a rule by id. Returns true if one was removed.
pub async fn remove_rule(session_db: &Database, rule_id: &str) -> anyhow::Result<bool> {
    let txn = session_db.new_transaction().await?;
    let store = txn.get_store::<Table<HeartbeatRule>>(RULES_STORE).await?;
    let existing = store.search(|r| r.id == rule_id).await?;
    let mut removed = false;
    for (key, _) in existing {
        if store.delete(&key).await? {
            removed = true;
        }
    }
    txn.commit().await?;
    Ok(removed)
}

/// List all rules on a session.
pub async fn list_rules(session_db: &Database) -> anyhow::Result<Vec<HeartbeatRule>> {
    let txn = session_db.new_transaction().await?;
    let store = txn.get_store::<Table<HeartbeatRule>>(RULES_STORE).await?;
    let rows = store.search(|_| true).await?;
    Ok(rows.into_iter().map(|(_, r)| r).collect())
}

/// Peer-local timestamp of the last successful fire for a given rule.
async fn load_last_fired(chaz_peer: &Database, rule_id: &str) -> Option<DateTime<Utc>> {
    let txn = chaz_peer.new_transaction().await.ok()?;
    let store = txn.get_store::<DocStore>(LAST_FIRED_STORE).await.ok()?;
    let iso = store.get_string(rule_id).await.ok()?;
    DateTime::parse_from_rfc3339(&iso)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

async fn save_last_fired(
    chaz_peer: &Database,
    rule_id: &str,
    when: DateTime<Utc>,
) -> anyhow::Result<()> {
    let txn = chaz_peer.new_transaction().await?;
    let store = txn.get_store::<DocStore>(LAST_FIRED_STORE).await?;
    store.set_string(rule_id, when.to_rfc3339()).await?;
    txn.commit().await?;
    Ok(())
}

/// Background runner: polls every `poll_interval`, fires due rules targeting
/// agents this peer hosts.
pub struct HeartbeatRunner {
    server: Arc<Server>,
    chaz_peer: Database,
    poll_interval: Duration,
    stopped: Arc<Mutex<bool>>,
}

impl HeartbeatRunner {
    pub fn new(server: Arc<Server>, chaz_peer: Database) -> Arc<Self> {
        Arc::new(Self {
            server,
            chaz_peer,
            poll_interval: Duration::from_secs(30),
            stopped: Arc::new(Mutex::new(false)),
        })
    }

    pub fn start(self: &Arc<Self>) {
        let runner = self.clone();
        tokio::spawn(async move {
            runner.run_loop().await;
        });
    }

    async fn run_loop(&self) {
        info!(
            poll_interval_secs = self.poll_interval.as_secs(),
            "Heartbeat runner started"
        );
        loop {
            if *self.stopped.lock().await {
                return;
            }
            if let Err(e) = self.tick().await {
                warn!("Heartbeat tick failed: {e}");
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// Single pass: enumerate sessions, check their rules, fire due ones.
    pub async fn tick(&self) -> anyhow::Result<()> {
        let sessions = self.server.registry().list_sessions().await?;
        for index in sessions {
            let session_db_id = index.session_db_id.clone();
            let Ok((_conv, session_db)) = self.server.registry().open_session(&session_db_id).await
            else {
                continue;
            };
            let rules = match list_rules(&session_db).await {
                Ok(r) => r,
                Err(e) => {
                    debug!(session = %session_db_id, "No rules or read failed: {e}");
                    continue;
                }
            };
            for rule in rules {
                if let Err(e) = self
                    .maybe_fire(
                        &session_db_id,
                        &session_db,
                        &rule,
                        self.server.agent_index(),
                    )
                    .await
                {
                    warn!(rule = %rule.id, "Fire check failed: {e}");
                }
            }
        }
        Ok(())
    }

    async fn maybe_fire(
        &self,
        session_db_id: &str,
        session_db: &Database,
        rule: &HeartbeatRule,
        agent_index: &HostedIndex,
    ) -> anyhow::Result<()> {
        if !rule.enabled {
            return Ok(());
        }

        // Do we host the target agent on this peer?
        let Ok(id) = eidetica::entry::ID::parse(&rule.target_agent_db_id) else {
            debug!(rule = %rule.id, "Rule target_agent_db_id unparseable");
            return Ok(());
        };
        let Some(_entry) = agent_index.find_by_id(&id) else {
            return Ok(()); // Not our agent; silently skip.
        };

        let schedule = match Schedule::from_str(&rule.cron) {
            Ok(s) => s,
            Err(e) => {
                warn!(rule = %rule.id, cron = %rule.cron, "Invalid cron: {e}");
                return Ok(());
            }
        };

        let now = Utc::now();
        // First observation of this rule: bootstrap last_fired = now and skip
        // this tick. `schedule.upcoming(Utc).next()` (and `after(now).next()`)
        // is always strictly in the future, so comparing `next > now` would
        // never fire on the first pass. Subsequent ticks take the `Some(lr)`
        // branch below, which correctly returns the next occurrence after
        // the last fire; once real time crosses it, we fire.
        let last = match load_last_fired(&self.chaz_peer, &rule.id).await {
            Some(t) => t,
            None => {
                save_last_fired(&self.chaz_peer, &rule.id, now).await?;
                debug!(rule = %rule.id, "Bootstrapped last_fired for new heartbeat rule");
                return Ok(());
            }
        };
        let Some(next) = schedule.after(&last).next() else {
            return Ok(());
        };
        if next > now {
            return Ok(());
        }

        info!(
            rule = %rule.id,
            session = %session_db_id,
            target_agent = %rule.target_agent_db_id,
            "Firing heartbeat rule"
        );

        let mut session = Session::new(
            ConversationId(session_db_id.to_string()),
            session_db.clone(),
        )
        .await;
        let content = format!(
            "Heartbeat '{}' at {}.\n\n{}",
            rule.name,
            now.format("%Y-%m-%d %H:%M:%S UTC"),
            rule.task
        );
        session
            .add_entry(SessionEntry {
                sender: "heartbeat".to_string(),
                content,
                timestamp: now,
                entry_type: EntryType::Directive,
                metadata: None,
            })
            .await;

        if let Err(e) = save_last_fired(&self.chaz_peer, &rule.id, now).await {
            error!(rule = %rule.id, "Failed to persist last_fired: {e}");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eidetica::Instance;
    use eidetica::backend::database::InMemory;
    use eidetica::crdt::Doc;
    use eidetica::user::User;

    async fn test_session_db() -> (Instance, User, Database) {
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let mut user = instance.login_user("test", None).await.unwrap();
        let key = user.get_default_key().unwrap();
        let mut s = Doc::new();
        s.set("name", "session");
        let db = user.create_database(s, &key).await.unwrap();
        (instance, user, db)
    }

    fn rule(id: &str, cron: &str, target: &str) -> HeartbeatRule {
        HeartbeatRule {
            id: id.to_string(),
            name: format!("rule-{id}"),
            cron: cron.to_string(),
            task: "do a thing".to_string(),
            target_agent_db_id: target.to_string(),
            enabled: true,
        }
    }

    #[tokio::test]
    async fn rule_upsert_list_remove() {
        let (_i, _u, db) = test_session_db().await;
        upsert_rule(&db, rule("r1", "0 0 * * * *", "sha256:aaa"))
            .await
            .unwrap();
        upsert_rule(&db, rule("r2", "0 0 * * * *", "sha256:bbb"))
            .await
            .unwrap();
        assert_eq!(list_rules(&db).await.unwrap().len(), 2);

        // Upsert r1 (modify task).
        let mut updated = rule("r1", "0 0 * * * *", "sha256:aaa");
        updated.task = "updated".into();
        upsert_rule(&db, updated.clone()).await.unwrap();
        let all = list_rules(&db).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all.iter().find(|r| r.id == "r1").unwrap().task, "updated");

        assert!(remove_rule(&db, "r1").await.unwrap());
        assert_eq!(list_rules(&db).await.unwrap().len(), 1);
        // Removing again is a no-op.
        assert!(!remove_rule(&db, "r1").await.unwrap());
    }

    #[tokio::test]
    async fn last_fired_round_trip() {
        let (_i, _u, db) = test_session_db().await;
        assert!(load_last_fired(&db, "r1").await.is_none());
        let now = Utc::now();
        save_last_fired(&db, "r1", now).await.unwrap();
        let read = load_last_fired(&db, "r1").await.unwrap();
        // Truncated to RFC3339 (ns precision), compare to within a second.
        assert!((now - read).num_milliseconds().abs() < 1000);
    }
}
