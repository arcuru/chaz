//! Skill Bank DB primitive — parallel to [`MemoryBankDb`].
//!
//! A `SkillBankDb` is a standalone eidetica `Database` owned by a
//! per-bank `PrivateKey`, holding a single `skills` Table store. Agents
//! gain access to a bank by having their pubkey added to the bank's
//! `AuthSettings` with `Read` or `Write`; access shows up as a
//! [`SkillBankRef`] in the agent's own DB's `skill_banks` subtree.
//!
//! Shape is deliberately minimal vs. [`AgentDb`]:
//! - `skills` (Table<Skill>) — the bank's skills. Same subtree name and
//!   schema as an agent's own skills, so read/write code paths are
//!   uniform.
//! - `meta`   (DocStore) — display name + description.
//!
//! Because an Agent DB *also* carries a `skills` subtree with the same
//! shape, any Agent DB is usable as a skill bank by anyone with Read on
//! it — no separate "bank" type is required for introspection. This is
//! the same uniformity invariant `MemoryBankDb` upholds.

#![allow(dead_code)]

use crate::agent_db::{Skill, read_blob, write_blob};
use eidetica::Database;
use eidetica::auth::crypto::PublicKey;
use eidetica::crdt::Doc;
use eidetica::entry::ID;
use eidetica::store::{DocStore, Table};
use eidetica::user::User;
use serde::{Deserialize, Serialize};
use tracing::info;

pub const SKILLS_STORE: &str = "skills";
pub const META_STORE: &str = "meta";

/// Display metadata for a skill bank. Read by `/skills list` and by the
/// dynamic descriptor that tells the LLM which banks are available.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SkillBankMeta {
    pub display_name: Option<String>,
    pub description: Option<String>,
}

/// Handle over the eidetica `Database` that holds a skill bank.
#[derive(Clone)]
pub struct SkillBankDb {
    database: Database,
}

impl SkillBankDb {
    pub fn from_database(database: Database) -> Self {
        Self { database }
    }

    pub fn id(&self) -> ID {
        self.database.root_id().clone()
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    pub async fn read_meta(&self) -> anyhow::Result<SkillBankMeta> {
        read_blob(&self.database, META_STORE).await
    }

    pub async fn write_meta(&self, meta: &SkillBankMeta) -> anyhow::Result<()> {
        write_blob(&self.database, META_STORE, meta).await
    }

    /// Touch every well-known store so it exists in the DB. Safe to call
    /// repeatedly; commits in one transaction.
    pub async fn ensure_stores(&self) -> anyhow::Result<()> {
        let txn = self.database.new_transaction().await?;
        txn.get_store::<Table<Skill>>(SKILLS_STORE).await?;
        txn.get_store::<DocStore>(META_STORE).await?;
        txn.commit().await?;
        Ok(())
    }
}

/// DB name used in eidetica settings — idempotency key for
/// `find_skill_bank` (parallel to `memory:<display_name>`).
pub fn skill_bank_db_name(display_name: &str) -> String {
    format!("skill:{display_name}")
}

/// Create a new Skill Bank DB. Generates a fresh key on `user`, creates
/// the DB signed by that key with `name: skill:<display_name>` in
/// settings, populates `meta`, and initializes the `skills` store.
pub async fn create_skill_bank(
    user: &mut User,
    display_name: &str,
    meta: &SkillBankMeta,
) -> anyhow::Result<(SkillBankDb, PublicKey)> {
    let key = user
        .add_private_key(Some(&format!("skill:{display_name}")))
        .await?;
    let mut settings = Doc::new();
    settings.set("name", skill_bank_db_name(display_name).as_str());
    let database = user.create_database(settings, &key).await?;
    info!(
        bank = display_name,
        db_id = %database.root_id(),
        key = %key,
        "Created Skill Bank DB"
    );

    let bank = SkillBankDb::from_database(database);
    bank.ensure_stores().await?;
    bank.write_meta(meta).await?;
    crate::db_kind::write_marker(
        bank.database(),
        crate::db_kind::KIND_SKILL_BANK,
        display_name,
    )
    .await?;
    Ok((bank, key))
}

/// Look up an existing Skill Bank DB by display name on this peer's
/// `User`. Returns `(SkillBankDb, pubkey)` where pubkey is the key this
/// user holds for the DB. `None` if no bank with that name is tracked.
pub async fn find_skill_bank(user: &User, display_name: &str) -> Option<(SkillBankDb, PublicKey)> {
    let name = skill_bank_db_name(display_name);
    let database = user.find_database(&name).await.ok()?.into_iter().next()?;
    let pubkey = user.find_key(database.root_id()).ok().flatten()?;
    Some((SkillBankDb::from_database(database), pubkey))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use eidetica::backend::database::InMemory;
    use eidetica::{Instance, NewUser};

    async fn test_user() -> User {
        let backend = InMemory::new();
        let (_instance, user) =
            Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
                .await
                .unwrap();
        user
    }

    #[tokio::test]
    async fn create_and_read_meta() {
        let mut user = test_user().await;
        let meta = SkillBankMeta {
            display_name: Some("devops".to_string()),
            description: Some("devops + deploy automation skills".to_string()),
        };
        let (bank, pubkey) = create_skill_bank(&mut user, "devops", &meta).await.unwrap();
        assert_eq!(bank.read_meta().await.unwrap(), meta);
        assert_eq!(user.find_key(&bank.id()).unwrap(), Some(pubkey));
    }

    #[tokio::test]
    async fn find_by_display_name() {
        let mut user = test_user().await;
        let (created, _) = create_skill_bank(
            &mut user,
            "writing",
            &SkillBankMeta {
                display_name: Some("writing".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let id = created.id();
        let (found, _) = find_skill_bank(&user, "writing").await.expect("found");
        assert_eq!(found.id(), id);
        assert!(find_skill_bank(&user, "nope").await.is_none());
    }

    #[tokio::test]
    async fn skills_store_schema_matches_agent_db() {
        // Bank's `skills` subtree must be `Table<Skill>` with the same
        // store name as an AgentDb so the extension can read/write
        // either one uniformly.
        let mut user = test_user().await;
        let (bank, _) = create_skill_bank(
            &mut user,
            "shared",
            &SkillBankMeta {
                display_name: Some("shared".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let txn = bank.database().new_transaction().await.unwrap();
        let store = txn.get_store::<Table<Skill>>(SKILLS_STORE).await.unwrap();
        store
            .insert(Skill {
                name: "greet".into(),
                description: "say hi".into(),
                body: "When greeting, be warm but concise.".into(),
                timestamp: Utc::now(),
                tags: vec![],
            })
            .await
            .unwrap();
        txn.commit().await.unwrap();

        let txn = bank.database().new_transaction().await.unwrap();
        let store = txn.get_store::<Table<Skill>>(SKILLS_STORE).await.unwrap();
        let all = store.search(|_: &Skill| true).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1.name, "greet");
    }

    #[tokio::test]
    async fn store_name_matches_agent_db() {
        // Invariant: SkillBankDb::SKILLS_STORE == agent_db::SKILLS_STORE
        // — both write into "skills".
        assert_eq!(SKILLS_STORE, crate::agent_db::SKILLS_STORE);
    }
}
