//! Bridge settings DB primitive.
//!
//! A `BridgeDb` is a standalone eidetica `Database` owned by a bridge's own
//! key, holding transport-login *credentials* encrypted in a
//! `PasswordStore<DocStore>`. It is the secret-bearing counterpart to the
//! agent DB's unencrypted `logins` registry ([`crate::agent_db::LoginRef`]):
//! the registry records that a login exists and which bridge DB manages it;
//! this DB holds the homeserver/username/password|token/allow_list the bridge
//! needs to actually authenticate. Created and fully owned by the bridge
//! process — its key is `Admin` here.
//!
//! Encrypted at rest: the credentials store syncs only as opaque ciphertext.
//! The unlock password lives in the bridge's own config, never in any synced
//! DB. A wrong password makes the store refuse to open, so reads error rather
//! than leaking plaintext. Losing the password makes the stored credentials
//! unrecoverable — re-seed from the bridge config.

// Read-side helpers are exercised by the bridge binary / tests, not the chaz
// daemon; keep the API surface stable without per-item churn.
#![allow(dead_code)]

use eidetica::Database;
use eidetica::auth::crypto::PublicKey;
use eidetica::crdt::Doc;
use eidetica::entry::ID;
use eidetica::store::{DocStore, PasswordStore};
use eidetica::user::User;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::info;

/// Encrypted store of a bridge's own details: a `PasswordStore<DocStore>`
/// keyed by `login_id`, each value a JSON blob whose **shape is the bridge's
/// own business**. chaz-core imposes no credential schema — a Matrix bridge
/// stores homeserver/user/password, a Discord bridge stores token/allowed
/// users; this layer only encrypts, keys, and round-trips opaque serializable
/// values. Created lazily on the first seed (it can't exist before the unlock
/// password initializes its encryption) and unlocked with a password that
/// never syncs.
pub const CREDENTIALS_STORE: &str = "credentials";

/// Handle over the eidetica `Database` a bridge fully owns and manages. Holds
/// the bridge's encrypted, transport-specific details; the only thing that
/// crosses into the shared world is the [`LoginRef`](crate::agent_db::LoginRef)
/// pointer the bridge drops into its agent's DB (`kind` / `identifier` / this
/// DB's id).
#[derive(Clone, Debug)]
pub struct BridgeDb {
    database: Database,
}

impl BridgeDb {
    /// Wrap an existing database as a `BridgeDb`. Use when the caller already
    /// opened the DB (e.g. via `User::open_database`).
    pub fn from_database(database: Database) -> Self {
        Self { database }
    }

    /// Eidetica root ID of this bridge DB — the value an agent DB's
    /// [`LoginRef::bridge_db_id`](crate::agent_db::LoginRef::bridge_db_id)
    /// points at.
    pub fn id(&self) -> ID {
        self.database.root_id().clone()
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    /// Write (or overwrite) the `creds` value for `login_id`, encrypting it
    /// under `unlock_password`. `T` is whatever the bridge wants to persist —
    /// chaz-core never inspects it. Initializes the store on first use; opens
    /// it with the password thereafter (a wrong password errors). Idempotent —
    /// re-seeding a `login_id` replaces the prior value, so the bridge's
    /// first-boot seeding can run on every start.
    pub async fn seed_credentials<T: Serialize>(
        &self,
        login_id: &str,
        creds: &T,
        unlock_password: &str,
    ) -> anyhow::Result<()> {
        let json = serde_json::to_string(creds)?;
        let txn = self.database.new_transaction().await?;
        let mut store = txn
            .get_store::<PasswordStore<DocStore>>(CREDENTIALS_STORE)
            .await?;
        if store.is_initialized() {
            store.open(unlock_password)?;
        } else {
            store.initialize(unlock_password, Doc::new()).await?;
        }
        store.inner().await?.set_string(login_id, &json).await?;
        txn.commit().await?;
        Ok(())
    }

    /// Read and decrypt the value for `login_id`, deserializing it as `T` (the
    /// same type the bridge seeded). `Ok(None)` when the store has never been
    /// initialized or the login is absent; errors when `unlock_password` is
    /// wrong (the store refuses to open) or the stored blob isn't a `T`.
    pub async fn read_credentials<T: DeserializeOwned>(
        &self,
        login_id: &str,
        unlock_password: &str,
    ) -> anyhow::Result<Option<T>> {
        let txn = self.database.new_transaction().await?;
        let mut store = txn
            .get_store::<PasswordStore<DocStore>>(CREDENTIALS_STORE)
            .await?;
        if !store.is_initialized() {
            return Ok(None);
        }
        store.open(unlock_password)?;
        match store.inner().await?.get_string(login_id).await {
            Ok(json) => Ok(Some(serde_json::from_str(&json)?)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// DB name used in eidetica settings — the idempotency key for
/// [`find_bridge_db`] (parallel to `agent:<name>` / `memory:<name>`).
pub fn bridge_db_name(label: &str) -> String {
    format!("bridge:{label}")
}

/// Create a new bridge settings DB, owned by a fresh key on `user` (the
/// bridge's own key → `Admin`). Writes the db-kind marker; the credentials
/// store is created lazily on the first [`BridgeDb::seed_credentials`] (it
/// can't exist until the unlock password initializes its encryption). Returns
/// the DB handle alongside the owning pubkey.
pub async fn create_bridge_db(
    user: &mut User,
    label: &str,
) -> anyhow::Result<(BridgeDb, PublicKey)> {
    let key = user
        .add_private_key(Some(&format!("bridge:{label}")))
        .await?;
    let mut settings = Doc::new();
    settings.set("name", bridge_db_name(label).as_str());
    let database = user.create_database(settings, &key).await?;
    info!(
        bridge = label,
        db_id = %database.root_id(),
        key = %key,
        "Created bridge settings DB"
    );

    let bridge = BridgeDb::from_database(database);
    crate::db_kind::write_marker(bridge.database(), crate::db_kind::KIND_BRIDGE, label).await?;
    Ok((bridge, key))
}

/// Look up an existing bridge settings DB by label on this peer's `User`.
/// Returns `(BridgeDb, pubkey)` where pubkey is the key this user holds for
/// the DB. `None` if no bridge DB with that label is tracked.
pub async fn find_bridge_db(user: &User, label: &str) -> Option<(BridgeDb, PublicKey)> {
    let name = bridge_db_name(label);
    let database = user.find_database(&name).await.ok()?.into_iter().next()?;
    let pubkey = user.find_key(database.root_id()).ok().flatten()?;
    Some((BridgeDb::from_database(database), pubkey))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    use eidetica::backend::database::InMemory;
    use eidetica::{Instance, NewUser};

    /// Stand-in for whatever shape a real bridge stores — chaz-core is
    /// schema-agnostic, so the test brings its own type.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TestCreds {
        identifier: String,
        secret: String,
    }

    async fn test_user() -> User {
        let backend = InMemory::new();
        let (_instance, user) =
            Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
                .await
                .unwrap();
        user
    }

    fn matrix_creds(identifier: &str, secret: &str) -> TestCreds {
        TestCreds {
            identifier: identifier.to_string(),
            secret: secret.to_string(),
        }
    }

    /// Build a fresh peer + create one bridge DB. The `user` must outlive the
    /// `db` (eidetica's Instance drops with it).
    async fn peer_with_bridge_db() -> (User, BridgeDb) {
        let mut user = test_user().await;
        let (db, _) = create_bridge_db(&mut user, "matrix").await.unwrap();
        (user, db)
    }

    #[tokio::test]
    async fn create_and_find_by_label() {
        let mut user = test_user().await;
        let (db, pubkey) = create_bridge_db(&mut user, "matrix").await.unwrap();
        let id = db.id();

        // Marker identifies it as a bridge DB.
        assert_eq!(
            crate::db_kind::read_marker(db.database()).await,
            Some((
                crate::db_kind::KIND_BRIDGE.to_string(),
                "matrix".to_string()
            ))
        );
        // Returned pubkey is really the one the user holds for this DB.
        assert_eq!(user.find_key(&id).unwrap(), Some(pubkey));

        let (found, _) = find_bridge_db(&user, "matrix").await.expect("found");
        assert_eq!(found.id(), id);
        assert!(find_bridge_db(&user, "nope").await.is_none());
    }

    #[tokio::test]
    async fn credentials_absent_before_any_seed() {
        let (_user, db) = peer_with_bridge_db().await;
        let got = db
            .read_credentials::<TestCreds>("@chaz:example", "unlock-pw")
            .await
            .unwrap();
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn seed_and_read_round_trips_encrypted() {
        let (_user, db) = peer_with_bridge_db().await;
        let creds = matrix_creds("@chaz:example", "hunter2");
        db.seed_credentials("@chaz:example", &creds, "unlock-pw")
            .await
            .unwrap();

        let got = db
            .read_credentials::<TestCreds>("@chaz:example", "unlock-pw")
            .await
            .unwrap();
        assert_eq!(got, Some(creds));
    }

    #[tokio::test]
    async fn wrong_password_fails() {
        let (_user, db) = peer_with_bridge_db().await;
        db.seed_credentials(
            "@chaz:example",
            &matrix_creds("@chaz:example", "hunter2"),
            "correct-pw",
        )
        .await
        .unwrap();
        assert!(
            db.read_credentials::<TestCreds>("@chaz:example", "wrong-pw")
                .await
                .is_err(),
            "wrong unlock password must not decrypt the credentials"
        );
    }

    #[tokio::test]
    async fn reseed_overwrites_value() {
        let (_user, db) = peer_with_bridge_db().await;
        db.seed_credentials("@chaz:example", &matrix_creds("@chaz:example", "old"), "pw")
            .await
            .unwrap();
        db.seed_credentials("@chaz:example", &matrix_creds("@chaz:example", "new"), "pw")
            .await
            .unwrap();

        let got: TestCreds = db
            .read_credentials("@chaz:example", "pw")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.secret, "new");
    }

    #[tokio::test]
    async fn distinct_logins_coexist_in_one_bridge_db() {
        // A bridge may hold several logins; each `login_id` is independent.
        let (_user, db) = peer_with_bridge_db().await;
        db.seed_credentials("@chaz:example", &matrix_creds("@chaz:example", "a"), "pw")
            .await
            .unwrap();
        db.seed_credentials("@other:example", &matrix_creds("@other:example", "b"), "pw")
            .await
            .unwrap();

        assert_eq!(
            db.read_credentials::<TestCreds>("@chaz:example", "pw")
                .await
                .unwrap()
                .map(|c| c.secret),
            Some("a".to_string())
        );
        assert_eq!(
            db.read_credentials::<TestCreds>("@other:example", "pw")
                .await
                .unwrap()
                .map(|c| c.secret),
            Some("b".to_string())
        );
    }
}
