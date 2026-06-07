//! Content-addressed store of resolved agent system prompts, kept **off** an
//! agent's primary DB so the agent's append-only config never carries tens of
//! KB of prompt text. The resolved prompt — an agent's `system_prompt_files`
//! read and concatenated with its inline `system_prompt` — lives in a
//! `DocStore` (`agent_prompts`) on the `chaz_peer` DB; the agent's
//! [`crate::agent_db::AgentDbConfig`] stores only a pointer
//! (`system_prompt_ref`).
//!
//! The pointer is an eidetica [`Snapshot`] — the tip of the commit that wrote
//! the prompt. eidetica entries are content-addressed (CIDs), so a snapshot is
//! a durable, reproducible address: reading the store *at* that snapshot
//! ([`Database::new_transaction_at`]) always reconstructs exactly that version,
//! regardless of later writes. We therefore don't compute our own hash — the
//! tip *is* the content address. Because the store is append-only and a
//! snapshot pins an immutable historical view, a prompt that was in use at some
//! point can always be reproduced from the pointer recorded at the time.
//!
//! See `docs/src/design/model_info_store.md` for the sibling chaz_peer store.

use eidetica::store::DocStore;
use eidetica::{Database, Snapshot};

const STORE: &str = "agent_prompts";
/// Single key; the *snapshot* returned from [`PromptStore::put`] is what
/// disambiguates versions, so one key suffices — reading at a given tip
/// resolves `KEY` to the text written by that commit.
const KEY: &str = "text";

#[derive(Clone)]
pub struct PromptStore {
    db: Database,
}

impl PromptStore {
    /// Wrap the `chaz_peer` database. Reads/writes go to the `agent_prompts`
    /// DocStore on that DB — created lazily on first write.
    pub fn new(chaz_peer: Database) -> Self {
        Self { db: chaz_peer }
    }

    /// Append `text` as a new immutable version and return a [`Snapshot`]
    /// pinning the commit that wrote it. Reading at the returned snapshot always
    /// yields `text`. No hash is computed — the commit's tip (a CID) is the
    /// address.
    ///
    /// Callers (reconcile) avoid churn by *not* calling this when the prompt is
    /// unchanged — they reuse the existing `system_prompt_ref` instead — so a
    /// new entry lands only when the resolved prompt actually changes.
    pub async fn put(&self, text: &str) -> anyhow::Result<Snapshot> {
        let txn = self.db.new_transaction().await?;
        let store = txn.get_store::<DocStore>(STORE).await?;
        store.set_string(KEY, text).await?;
        let id = txn.commit().await?;
        Ok(Snapshot::new(vec![id]))
    }

    /// Read the prompt version pinned by `at` (a snapshot from [`put`]). `None`
    /// when the snapshot's tips are unknown to this peer — e.g. a pointer that
    /// outlived a wiped store.
    pub async fn get(&self, at: &Snapshot) -> Option<String> {
        let txn = self.db.new_transaction_at(at).await.ok()?;
        let store = txn.get_store::<DocStore>(STORE).await.ok()?;
        store.get_string(KEY).await.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eidetica::backend::database::InMemory;
    use eidetica::{Instance, NewUser};

    // Returns the `Instance`/`User` alongside the DB so callers keep them alive
    // — the `Database` borrows the instance backend, which is dropped (and the
    // DB invalidated: "Instance has been dropped") if they fall. Mirrors
    // `model_info_store`'s fixture.
    async fn fresh_db() -> (Instance, eidetica::user::User, Database) {
        let (instance, mut user) =
            Instance::create_backend(Box::new(InMemory::new()), NewUser::passwordless("t"))
                .await
                .unwrap();
        let key = user.get_default_key().unwrap();
        let mut settings = eidetica::crdt::Doc::new();
        settings.set("name", "chaz_peer");
        let db = user.create_database(settings, &key).await.unwrap();
        (instance, user, db)
    }

    #[tokio::test]
    async fn put_then_read_back_at_snapshot() {
        let (_inst, _user, db) = fresh_db().await;
        let store = PromptStore::new(db);
        let text = "You are Ava.\n\nVoice: terse.";
        let snap = store.put(text).await.unwrap();
        assert_eq!(store.get(&snap).await.as_deref(), Some(text));
    }

    #[tokio::test]
    async fn distinct_versions_have_distinct_snapshots() {
        let (_inst, _user, db) = fresh_db().await;
        let store = PromptStore::new(db);
        let a = store.put("prompt A").await.unwrap();
        let b = store.put("prompt B").await.unwrap();
        assert_ne!(a, b);
        assert_eq!(store.get(&a).await.as_deref(), Some("prompt A"));
        assert_eq!(store.get(&b).await.as_deref(), Some("prompt B"));
    }

    #[tokio::test]
    async fn old_snapshot_still_reproduces_after_later_write() {
        // The core reproducibility property: a pointer captured earlier keeps
        // resolving to its original text even after the key is overwritten.
        let (_inst, _user, db) = fresh_db().await;
        let store = PromptStore::new(db);
        let v1 = store.put("version one").await.unwrap();
        let _v2 = store.put("version two").await.unwrap();
        assert_eq!(store.get(&v1).await.as_deref(), Some("version one"));
    }
}
