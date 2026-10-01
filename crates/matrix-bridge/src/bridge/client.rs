//! Owned Matrix client layer — the slice of headjack chaz actually used.
//!
//! chaz used headjack for four things: password login + session restore, invite
//! auto-join, the sync loop, and command dispatch. The first three are a thin
//! wrapper over matrix-sdk 0.16 and live here; command dispatch is gone —
//! inbound messages now route through [`chaz_core::commands::parse`] in
//! `mod.rs`. The on-disk `{state_dir}/session` JSON is kept byte-compatible with
//! headjack's `FullSession` so an existing login restores without re-auth.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::ruma::api::client::filter::FilterDefinition;
use matrix_sdk::ruma::api::client::{keys::get_keys, uiaa};
use matrix_sdk::ruma::events::room::member::StrippedRoomMemberEvent;
use matrix_sdk::{
    Client, ClientBuilder, Error, LoopCtrl, Room, RoomMemberships, config::SyncSettings,
};
use matrix_sdk_base::crypto::{CollectStrategy, DecryptionSettings, TrustRequirement};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::io::Write;
use tokio::fs;
use tokio::time::sleep;
use tracing::{error, info, warn};

/// Matrix login credentials.
pub struct Login {
    pub homeserver_url: String,
    pub username: String,
    pub password: Option<String>,
}

/// Data needed to rebuild a client — persisted alongside the user session.
/// Field set and names match headjack's on-disk format exactly so existing
/// `session` files deserialize unchanged.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ClientSession {
    pub(crate) homeserver: String,
    pub(crate) db_path: PathBuf,
    pub(crate) passphrase: String,
    /// Public device fingerprint, used to detect an empty/replaced crypto DB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) device_ed25519_key: Option<String>,
}

/// The full session persisted to `{state_dir}/session` as JSON. Layout is
/// byte-compatible with headjack's `FullSession`.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct FullSession {
    pub(crate) client_session: ClientSession,
    pub(crate) user_session: MatrixSession,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sync_token: Option<String>,
}

/// A connected Matrix client plus the bookkeeping the sync loop needs.
pub struct MatrixClient {
    client: Client,
    sync_token: Option<String>,
    session_file: PathBuf,
}

impl MatrixClient {
    /// The underlying matrix-sdk client.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Log in (or restore an existing session) for `login`.
    ///
    /// `state_dir` resolves like headjack: an explicit path (tilde-expanded) or,
    /// when `None`, `$XDG_STATE_HOME/{name}`. The `session` file under it holds
    /// the persisted credentials + sync token. On a fresh login `name` is also
    /// the client's initial device display name, so callers can tell their
    /// device apart in the account's session list: the bridge signs in as
    /// `chaz`, while the `rooms` maintenance command's throwaway device shows
    /// as `chaz-matrix-rooms`.
    pub async fn login(login: &Login, state_dir: Option<&str>, name: &str) -> anyhow::Result<Self> {
        let state_dir = match state_dir {
            Some(s) => PathBuf::from(expand_tilde(s)),
            None => dirs::state_dir()
                .expect("no state_dir directory found")
                .join(name),
        };
        let mut lease = crate::store_lock::StoreLock::acquire(&state_dir)?;
        anyhow::ensure!(
            !lease.root.join("pending-reset.json").exists(),
            "Matrix identity transition pending; run keys resume with the bridge stopped"
        );
        let state_dir = lease.root.clone();
        let session_file = state_dir.join("session");
        let store = if session_file.exists() {
            crate::store_lock::no_symlinks(&session_file)?;
            let full: FullSession = serde_json::from_slice(&std::fs::read(&session_file)?)?;
            anyhow::ensure!(
                full.user_session.meta.user_id.as_str() == login.username
                    && full.client_session.homeserver == login.homeserver_url,
                "saved Matrix account/homeserver differs from configuration; refusing store access"
            );
            if full.client_session.db_path.as_os_str().is_empty() {
                state_dir.join("store")
            } else {
                full.client_session.db_path
            }
        } else {
            state_dir.join("store")
        };
        if store.exists() {
            lease.lock_store(&store)?;
        }

        let (client, sync_token) = if session_file.exists() {
            restore_session(&session_file, &state_dir).await?
        } else {
            (
                do_login(&session_file, &state_dir, login, name).await?,
                None,
            )
        };
        if !store.join("ownership.lock").exists() {
            lease.lock_store(&store)?;
        }
        // Context belongs to the SDK client's inner Arc, so raw Client clones
        // and handler tasks keep the lease until their final store access.
        client.add_event_handler_context(std::sync::Arc::new(lease));
        Ok(Self {
            client,
            sync_token,
            session_file,
        })
    }

    /// Sign this device with the account's cross-signing identity, creating
    /// the identity if the account has none. Only the long-lived bridge device
    /// calls this: a throwaway device must never own the identity.
    ///
    /// Done explicitly, and awaited, rather than through the SDK's
    /// `auto_enable_cross_signing`, whose background task only logs failures
    /// and could run before the session file is saved.
    pub async fn ensure_cross_signed(&self, login: &Login) -> anyhow::Result<()> {
        initialize_cross_signing(&self.client, login).await
    }

    /// Install the invite auto-join handler: allow-list filtered, exponential
    /// backoff capped at 3600s, and a post-join room-size check that leaves
    /// rooms exceeding the limit. Mirrors headjack's `join_rooms`.
    pub fn install_autojoin(&self, allow_list: Option<String>, room_size_limit: Option<usize>) {
        let username = self.full_name();
        self.client.add_event_handler(
            move |room_member: StrippedRoomMemberEvent, client: Client, room: Room| async move {
                if room_member.state_key != client.user_id().unwrap() {
                    return;
                }
                if !is_allowed(
                    allow_list.as_deref(),
                    room_member.sender.as_str(),
                    &username,
                ) {
                    return;
                }
                info!("Received stripped room member event: {room_member:?}");
                tokio::spawn(async move {
                    info!("Autojoining room {}", room.room_id());
                    let mut delay = 2;
                    while let Err(err) = room.join().await {
                        warn!(
                            "Failed to join room {} ({err:?}), retrying in {delay}s",
                            room.room_id()
                        );
                        sleep(Duration::from_secs(delay)).await;
                        delay *= 2;
                        if delay > 3600 {
                            error!("Can't join room {} ({err:?})", room.room_id());
                            break;
                        }
                    }
                    if is_room_too_large(&room, room_size_limit).await {
                        warn!(
                            "Room {} has too many members, refusing to join",
                            room.room_id()
                        );
                        if let Err(e) = room.leave().await {
                            error!("Error leaving room: {e:?}");
                        }
                        return;
                    }
                    info!("Successfully joined room {}", room.room_id());
                });
            },
        );
    }

    /// Block until the first sync against the homeserver succeeds, retrying on
    /// transient errors. Primes the sync token so the subsequent run loop only
    /// sees *new* events (history is not replayed through handlers).
    pub async fn initial_sync(&mut self) {
        loop {
            match self.sync_once().await {
                Ok(()) => break,
                Err(e) => {
                    error!("An error occurred during initial sync: {e}");
                    error!("Trying again…");
                }
            }
        }
    }

    /// One sync pass, persisting the new next-batch token to the session file.
    async fn sync_once(&mut self) -> anyhow::Result<()> {
        let filter = FilterDefinition::with_lazy_loading();
        let mut settings = SyncSettings::default().filter(filter.into());
        if let Some(token) = &self.sync_token {
            settings = settings.token(token);
        }
        let response = self.client.sync_once(settings).await?;
        self.sync_token = Some(response.next_batch.clone());
        persist_sync_token(&self.session_file, response.next_batch).await?;
        Ok(())
    }

    /// Run the continuous sync loop, persisting the sync token after each
    /// response. Returns on the first sync error (the caller retries).
    pub async fn run_sync_loop(&self) -> anyhow::Result<()> {
        let filter = FilterDefinition::with_lazy_loading();
        let mut settings = SyncSettings::default().filter(filter.into());
        if let Some(token) = &self.sync_token {
            settings = settings.token(token);
        }
        let session_file = self.session_file.clone();
        self.client
            .sync_with_result_callback(settings, |sync_result| {
                let session_file = session_file.clone();
                async move {
                    let response = sync_result?;
                    persist_sync_token(&session_file, response.next_batch)
                        .await
                        .map_err(|e| Error::UnknownError(e.into()))?;
                    Ok(LoopCtrl::Continue)
                }
            })
            .await?;
        Ok(())
    }

    fn full_name(&self) -> String {
        self.client.user_id().unwrap().to_string()
    }
}

/// Whether `sender` is permitted: never the bot itself, otherwise must match
/// the allow-list regex. With no allow-list, nobody is allowed (matches
/// headjack — chaz always configures one).
pub(crate) fn is_allowed(allow_list: Option<&str>, sender: &str, username: &str) -> bool {
    if sender == username {
        false
    } else if let Some(allow_list) = allow_list {
        Regex::new(allow_list)
            .expect("Invalid allow_list regular expression")
            .is_match(sender)
    } else {
        false
    }
}

/// Expand a leading `~/` against the home directory.
fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.display().to_string() + &path[1..];
    }
    path.to_string()
}

/// Reuse both halves of the store metadata, or migrate a genuinely legacy
/// session. A half-written pair is not permission to generate replacement keys.
fn resolve_store(session: &mut ClientSession, state_dir: &Path) -> anyhow::Result<bool> {
    let no_path = session.db_path.as_os_str().is_empty();
    let no_passphrase = session.passphrase.is_empty();
    anyhow::ensure!(
        no_path == no_passphrase,
        "incomplete Matrix crypto store metadata; restore the session and store together from backup"
    );
    if no_path {
        session.db_path = state_dir.join("store");
        anyhow::ensure!(
            !session.db_path.exists(),
            "unrecorded Matrix store exists; refusing to replace crypto keys; restore the session and store together from backup"
        );
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("system RNG unavailable"))?;
        session.passphrase = bytes.iter().fold(String::new(), |mut value, byte| {
            use std::fmt::Write as _;
            let _ = write!(value, "{byte:02x}");
            value
        });
    } else {
        // No recorded device key means the store was never opened (a crash
        // right after recording the passphrase), so there are no keys to lose.
        anyhow::ensure!(
            session.db_path.join("matrix-sdk-crypto.sqlite3").is_file()
                || session.device_ed25519_key.is_none(),
            "Matrix crypto store is missing; refusing to replace device keys; restore the session and store together from backup"
        );
    }
    Ok(no_path)
}

/// Build a client on the login's persistent, passphrase-encrypted store.
///
/// Recipient policy follows MSC4153: room keys go only to devices their owner
/// has cross-signed. The bridge also requires cross-signed incoming senders. No interactive verification with the bot is needed.
/// `auto_enable_cross_signing` stays off; see [`MatrixClient::ensure_cross_signed`].
pub(crate) fn encrypted_builder(session: &ClientSession) -> ClientBuilder {
    Client::builder()
        .homeserver_url(&session.homeserver)
        .sqlite_store(&session.db_path, Some(&session.passphrase))
        .with_room_key_recipient_strategy(CollectStrategy::IdentityBasedStrategy)
        .with_decryption_settings(DecryptionSettings {
            sender_device_trust_requirement: TrustRequirement::CrossSigned,
        })
}

/// Query the server before any key upload. In particular, a legacy session
/// must not overwrite encryption keys published by some other client/store.
pub(crate) async fn check_device_key(
    client: &Client,
    expected: Option<&str>,
) -> anyhow::Result<String> {
    let local = client
        .encryption()
        .ed25519_key()
        .await
        .context("Matrix device key missing")?;
    anyhow::ensure!(
        expected.is_none_or(|key| key == local),
        "Matrix crypto account changed; refusing to upload replacement device keys; restore the session and store together from backup"
    );
    let user = client.user_id().context("Matrix user missing")?;
    let device = client.device_id().context("Matrix device missing")?;
    let mut request = get_keys::v3::Request::new();
    request
        .device_keys
        .insert(user.to_owned(), vec![device.to_owned()]);
    let response = client.send(request).await?;
    anyhow::ensure!(
        response.failures.is_empty(),
        "Matrix device key query failed; refusing to upload replacement device keys"
    );
    let remote = match response
        .device_keys
        .get(user)
        .and_then(|devices| devices.get(device))
    {
        Some(raw) => {
            let published = raw.deserialize()?;
            anyhow::ensure!(
                published.user_id == user && published.device_id == device,
                "published Matrix device account differs; refusing to overwrite it"
            );
            let key_id: matrix_sdk::ruma::OwnedDeviceKeyId = format!("ed25519:{device}").parse()?;
            Some(
                published
                    .keys
                    .get(&key_id)
                    .context("published Matrix device key missing; refusing to overwrite it")?
                    .clone(),
            )
        }
        None => None,
    };
    anyhow::ensure!(
        remote.as_deref().is_none_or(|key| key == local),
        "published Matrix device key differs from the local store; refusing to overwrite it; \
         restore the original crypto store"
    );
    Ok(local)
}

/// Restore a client + sync token from a persisted session file.
///
/// A pre-encryption session (empty `db_path`/`passphrase`, as headjack wrote
/// it) is upgraded in place: a fresh store and passphrase are recorded in the
/// session file before the client can upload any key material.
async fn restore_session(
    session_file: &Path,
    state_dir: &Path,
) -> anyhow::Result<(Client, Option<String>)> {
    info!(
        "Previous session found in '{}'",
        session_file.to_string_lossy()
    );
    let serialized = fs::read_to_string(session_file).await?;
    let mut full: FullSession = serde_json::from_str(&serialized)?;
    let migrated = resolve_store(&mut full.client_session, state_dir)?;
    if migrated {
        info!("Upgrading pre-encryption Matrix session with a persistent crypto store");
        // Record the passphrase before the store it protects exists.
        write_session(session_file, &full).await?;
    }

    let client = encrypted_builder(&full.client_session).build().await?;
    info!("Restoring session for {}…", &full.user_session.meta.user_id);
    client.restore_session(full.user_session.clone()).await?;
    let key = check_device_key(&client, full.client_session.device_ed25519_key.as_deref()).await?;
    if full.client_session.device_ed25519_key.as_deref() != Some(key.as_str()) {
        full.client_session.device_ed25519_key = Some(key);
        write_session(session_file, &full).await?;
    }
    Ok((client, full.sync_token))
}

/// Password-login a fresh device and persist the session.
///
/// `device_name` is the Matrix initial device display name: it identifies this
/// device in the account's session list, and is the caller's `name` from
/// [`MatrixClient::login`].
async fn do_login(
    session_file: &Path,
    state_dir: &Path,
    login: &Login,
    device_name: &str,
) -> anyhow::Result<Client> {
    info!("No previous session found, logging in…");
    let password = match &login.password {
        Some(p) => p.clone(),
        None => anyhow::bail!("password is required (interactive entry is not supported)"),
    };

    // A store without a session file belongs to a device whose access token
    // was never saved; it can never be restored. Keep it rather than delete key
    // material, but move it aside so the new device starts from an empty store.
    let store = state_dir.join("store");
    if store.exists() {
        let aside = state_dir.join(format!(
            "store.orphaned-{}",
            chrono::Utc::now().format("%Y%m%dT%H%M%S")
        ));
        warn!(
            "Crypto store without a session file; moving it to {}",
            aside.display()
        );
        fs::rename(&store, &aside).await?;
    }
    let mut client_session = ClientSession {
        homeserver: login.homeserver_url.clone(),
        db_path: PathBuf::new(),
        passphrase: String::new(),
        device_ed25519_key: None,
    };
    resolve_store(&mut client_session, state_dir)?;

    let client = encrypted_builder(&client_session).build().await?;
    let matrix_auth = client.matrix_auth();
    matrix_auth
        .login_username(&login.username, &password)
        .initial_device_display_name(device_name)
        .await?;
    info!("Logged in as {}", login.username);

    let user_session = matrix_auth
        .session()
        .expect("a logged-in client should have a session");
    client_session.device_ed25519_key = client.encryption().ed25519_key().await;
    let full = FullSession {
        client_session,
        user_session,
        sync_token: None,
    };
    write_session(session_file, &full).await?;
    info!("Session persisted in {}", session_file.to_string_lossy());
    Ok(client)
}

/// Make sure this device is signed by the account's cross-signing identity.
///
/// Outbound room keys go only to cross-signed devices (MSC4153's
/// `IdentityBasedStrategy`), and that strategy also refuses to send from a
/// device that is not itself cross-signed. So a device we cannot sign can still
/// read encrypted rooms but cannot reply in them; that is logged, not fatal, so
/// unencrypted rooms keep working.
async fn initialize_cross_signing(client: &Client, login: &Login) -> anyhow::Result<()> {
    let encryption = client.encryption();
    let user = client.user_id().context("Matrix user missing")?.to_owned();
    // Local keys can be complete before their upload succeeds. The SDK also
    // retains a cached identity when the server returns none, so query the raw
    // published identity before deciding whether bootstrap is safe or needed.
    let mut request = get_keys::v3::Request::new();
    request.device_keys.insert(user.clone(), vec![]);
    let published = client.send(request).await?;
    anyhow::ensure!(
        published.failures.is_empty(),
        "Matrix identity query failed; refusing cross-signing bootstrap"
    );
    if published.master_keys.contains_key(&user) {
        encryption.request_user_identity(&user).await?;
        let device = encryption
            .get_own_device()
            .await?
            .context("Matrix device missing")?;
        if encryption
            .cross_signing_status()
            .await
            .is_some_and(|status| status.is_complete())
        {
            // The SDK may also cache a signature whose upload failed. Re-send
            // our device signature with the existing keys, never reset identity.
            device
                .verify()
                .await
                .context("Matrix device cross-signing failed")?;
        } else if !device.is_cross_signed_by_owner() {
            warn!(
                "{user} already has a cross-signing identity this device does not hold; \
                 replies in encrypted rooms stay disabled until another session of the \
                 account verifies this device"
            );
        }
        return Ok(());
    }

    info!("Creating a cross-signing identity for {user}");
    match encryption.bootstrap_cross_signing(None).await {
        Ok(()) => Ok(()),
        Err(error) => {
            let Some(response) = error.as_uiaa_response() else {
                return Err(error).context("Matrix cross-signing bootstrap failed");
            };
            let Some(password) = &login.password else {
                anyhow::bail!(
                    "creating a cross-signing identity for {user} needs the account password"
                );
            };
            let mut auth = uiaa::Password::new(
                uiaa::UserIdentifier::Matrix(uiaa::MatrixUserIdentifier::new(user.to_string())),
                password.clone(),
            );
            auth.session = response.session.clone();
            encryption
                .bootstrap_cross_signing(Some(uiaa::AuthData::Password(auth)))
                .await
                .context("Matrix cross-signing bootstrap failed")
        }
    }
}

/// Atomically write the session file with owner-only permissions: it holds
/// the access token and the passphrase to the device's key store, and a torn
/// write would lose the latter.
pub(crate) async fn write_session(session_file: &Path, full: &FullSession) -> anyhow::Result<()> {
    let serialized = serde_json::to_vec(full)?;
    let path = session_file.to_owned();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let dir = path.parent().context("session file has no parent")?;
        std::fs::create_dir_all(dir)?;
        crate::store_lock::no_symlinks(&path)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        let file = tmp.as_file_mut();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(&serialized)?;
        file.sync_all()?;
        tmp.persist(&path)?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    })
    .await?
}

/// Rewrite the session file with the latest sync token.
async fn persist_sync_token(session_file: &Path, sync_token: String) -> anyhow::Result<()> {
    let serialized = fs::read_to_string(session_file).await?;
    let mut full: FullSession = serde_json::from_str(&serialized)?;
    full.sync_token = Some(sync_token);
    write_session(session_file, &full).await
}

/// Whether the room exceeds the configured member cap.
async fn is_room_too_large(room: &Room, room_size_limit: Option<usize>) -> bool {
    match room_size_limit {
        Some(limit) => room
            .members(RoomMemberships::ACTIVE)
            .await
            .map(|m| m.len() > limit)
            .unwrap_or(false),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy(homeserver: &str) -> ClientSession {
        ClientSession {
            homeserver: homeserver.to_owned(),
            db_path: PathBuf::new(),
            passphrase: String::new(),
            device_ed25519_key: None,
        }
    }

    #[tokio::test]
    async fn raw_sdk_client_clones_retain_store_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let lease = crate::store_lock::StoreLock::acquire(dir.path()).unwrap();
        let server = wiremock::MockServer::start().await;
        let client = Client::builder()
            .homeserver_url(server.uri())
            .build()
            .await
            .unwrap();
        client.add_event_handler_context(std::sync::Arc::new(lease));
        let clone = client.clone();
        drop(client);
        assert!(crate::store_lock::StoreLock::acquire(dir.path()).is_err());
        drop(clone);
        assert!(crate::store_lock::StoreLock::acquire(dir.path()).is_ok());
    }

    #[test]
    fn a_pre_encryption_session_gets_a_fresh_store_and_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = legacy("http://hs");
        assert!(resolve_store(&mut session, dir.path()).unwrap());
        assert_eq!(session.db_path, dir.path().join("store"));
        assert_eq!(session.passphrase.len(), 64);
        assert!(session.passphrase.chars().all(|c| c.is_ascii_hexdigit()));

        let mut other = legacy("http://hs");
        let other_dir = tempfile::tempdir().unwrap();
        resolve_store(&mut other, other_dir.path()).unwrap();
        assert_ne!(session.passphrase, other.passphrase);
    }

    #[test]
    fn an_unrecorded_store_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("store")).unwrap();
        let error = resolve_store(&mut legacy("http://hs"), dir.path()).unwrap_err();
        assert!(
            error.to_string().contains("unrecorded Matrix store"),
            "{error}"
        );
    }

    #[test]
    fn half_recorded_store_metadata_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = legacy("http://hs");
        session.passphrase = "secret".into();
        let error = resolve_store(&mut session, dir.path()).unwrap_err();
        assert!(error.to_string().contains("incomplete"), "{error}");
    }

    #[test]
    fn a_recorded_store_must_still_exist() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = legacy("http://hs");
        session.db_path = dir.path().join("store");
        session.passphrase = "secret".into();
        // Recorded, but never opened: nothing to lose, so it may be created.
        assert!(!resolve_store(&mut session, dir.path()).unwrap());
        session.device_ed25519_key = Some("key".into());
        let error = resolve_store(&mut session, dir.path()).unwrap_err();
        assert!(error.to_string().contains("store is missing"), "{error}");

        std::fs::create_dir(&session.db_path).unwrap();
        std::fs::write(session.db_path.join("matrix-sdk-crypto.sqlite3"), b"").unwrap();
        assert!(!resolve_store(&mut session, dir.path()).unwrap());
        assert_eq!(session.passphrase, "secret");
    }

    #[test]
    fn a_headjack_session_file_still_parses() {
        let json = r#"{"client_session":{"homeserver":"http://hs","db_path":"","passphrase":""},
            "user_session":{"user_id":"@a:hs","device_id":"DEV","access_token":"tok"},
            "sync_token":"s1"}"#;
        let full: FullSession = serde_json::from_str(json).unwrap();
        assert!(full.client_session.device_ed25519_key.is_none());
        assert_eq!(full.sync_token.as_deref(), Some("s1"));
    }

    #[tokio::test]
    async fn interrupted_cross_signing_upload_is_retried_without_replacing_keys() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for failed_path in [
            "/_matrix/client/v3/keys/device_signing/upload",
            "/_matrix/client/v3/keys/signatures/upload",
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/_matrix/client/versions"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "versions": ["v1.11"]
                })))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/keys/query"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "device_keys": {}, "master_keys": {}, "self_signing_keys": {},
                    "user_signing_keys": {}, "failures": {}
                })))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/keys/upload"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "one_time_key_counts": {}
                })))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/keys/signatures/upload"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "failures": {}
                })))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/keys/device_signing/upload"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path(failed_path))
                .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                    "errcode": "M_FORBIDDEN", "error": "injected upload failure"
                })))
                .with_priority(1)
                .up_to_n_times(1)
                .expect(1)
                .mount(&server)
                .await;

            let dir = tempfile::tempdir().unwrap();
            let mut session = legacy(&server.uri());
            resolve_store(&mut session, dir.path()).unwrap();
            let client = encrypted_builder(&session).build().await.unwrap();
            let user_session: MatrixSession = serde_json::from_value(serde_json::json!({
                "user_id": "@a:hs", "device_id": "DEV", "access_token": "disposable"
            }))
            .unwrap();
            client.restore_session(user_session.clone()).await.unwrap();
            assert!(
                client
                    .encryption()
                    .bootstrap_cross_signing(None)
                    .await
                    .is_err()
            );
            assert!(
                client
                    .encryption()
                    .cross_signing_status()
                    .await
                    .unwrap()
                    .is_complete()
            );
            drop(client);

            if failed_path.ends_with("signatures/upload") {
                let requests = server.received_requests().await.unwrap();
                let identity: serde_json::Value = serde_json::from_slice(
                    &requests
                        .iter()
                        .find(|request| request.url.path().ends_with("device_signing/upload"))
                        .unwrap()
                        .body,
                )
                .unwrap();
                let device: serde_json::Value = serde_json::from_slice(
                    &requests
                        .iter()
                        .find(|request| request.url.path() == "/_matrix/client/v3/keys/upload")
                        .unwrap()
                        .body,
                )
                .unwrap();
                Mock::given(method("POST"))
                    .and(path("/_matrix/client/v3/keys/query"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "device_keys": {"@a:hs": {"DEV": device["device_keys"]}},
                        "master_keys": {"@a:hs": identity["master_key"]},
                        "self_signing_keys": {"@a:hs": identity["self_signing_key"]},
                        "user_signing_keys": {"@a:hs": identity["user_signing_key"]},
                        "failures": {}
                    })))
                    .with_priority(1)
                    .mount(&server)
                    .await;
            }
            let client = encrypted_builder(&session).build().await.unwrap();
            client.restore_session(user_session).await.unwrap();
            initialize_cross_signing(
                &client,
                &Login {
                    homeserver_url: server.uri(),
                    username: "a".into(),
                    password: None,
                },
            )
            .await
            .unwrap();
            let requests = server.received_requests().await.unwrap();
            let uploads: Vec<_> = requests
                .iter()
                .filter(|request| {
                    request.url.path() == "/_matrix/client/v3/keys/device_signing/upload"
                })
                .collect();
            if failed_path.ends_with("device_signing/upload") {
                assert_eq!(
                    uploads.len(),
                    2,
                    "restart must retry the unpublished identity"
                );
                assert_eq!(
                    uploads[0].body, uploads[1].body,
                    "retry must retain cross-signing keys"
                );
            } else {
                assert_eq!(
                    uploads.len(),
                    1,
                    "a published identity must not be rewritten"
                );
                assert_eq!(
                    requests
                        .iter()
                        .filter(|request| request.url.path().ends_with("signatures/upload"))
                        .count(),
                    2,
                    "restart must retry the device signature"
                );
            }
        }
    }

    #[tokio::test]
    async fn crypto_account_and_published_key_checks_fail_closed() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"versions": ["v1.11"]})),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let mut session = legacy(&server.uri());
        resolve_store(&mut session, dir.path()).unwrap();
        let user_session: MatrixSession = serde_json::from_value(serde_json::json!({
            "user_id": "@a:hs", "device_id": "DEV", "access_token": "disposable"
        }))
        .unwrap();
        let client = encrypted_builder(&session).build().await.unwrap();
        client.restore_session(user_session.clone()).await.unwrap();
        let key = client.encryption().ed25519_key().await.unwrap();
        assert!(
            check_device_key(&client, Some("different recorded key"))
                .await
                .is_err()
        );
        for (remote_key, failures, expected_ok) in [
            (Some(key.as_str()), serde_json::json!({}), true),
            (
                Some("different published key"),
                serde_json::json!({}),
                false,
            ),
            (None, serde_json::json!({}), false),
            (
                None,
                serde_json::json!({"hs": {"errcode": "M_UNAVAILABLE"}}),
                false,
            ),
        ] {
            let device_keys = if failures.as_object().unwrap().is_empty() {
                let keys = remote_key
                    .map(|key| serde_json::json!({"ed25519:DEV": key}))
                    .unwrap_or(serde_json::json!({}));
                serde_json::json!({"@a:hs": {"DEV": {"user_id": "@a:hs", "device_id": "DEV", "algorithms": [], "keys": keys, "signatures": {}}}})
            } else {
                serde_json::json!({})
            };
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/keys/query"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "device_keys": device_keys, "failures": failures
                })))
                .up_to_n_times(1)
                .mount(&server)
                .await;
            assert_eq!(
                check_device_key(&client, Some(&key)).await.is_ok(),
                expected_ok,
                "missing or mismatched published key must not authorize a replacement upload"
            );
        }
        // Even an identity the SDK cannot parse must not be treated as absent
        // and overwritten; a failed query is not proof of absence either.
        for (body, expected_ok) in [
            (
                serde_json::json!({"device_keys": {}, "master_keys": {"@a:hs": {
                    "user_id": "@a:hs", "usage": ["master"], "keys": {}
                }}}),
                true,
            ),
            (
                serde_json::json!({"device_keys": {}, "failures": {"hs": {"errcode": "M_UNAVAILABLE"}}}),
                false,
            ),
        ] {
            Mock::given(method("POST"))
                .and(path("/_matrix/client/v3/keys/query"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .up_to_n_times(if expected_ok { 2 } else { 1 })
                .mount(&server)
                .await;
            assert_eq!(
                initialize_cross_signing(
                    &client,
                    &Login {
                        homeserver_url: server.uri(),
                        username: "a".into(),
                        password: None,
                    }
                )
                .await
                .is_ok(),
                expected_ok
            );
        }
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|request| { !request.url.path().ends_with("/upload") }),
            "account/key/identity checks must not upload anything"
        );
        drop(client);
        for (user, device) in [("@b:hs", "DEV"), ("@a:hs", "OTHER")] {
            let client = encrypted_builder(&session).build().await.unwrap();
            let other: MatrixSession = serde_json::from_value(serde_json::json!({
                "user_id": user, "device_id": device, "access_token": "disposable"
            }))
            .unwrap();
            assert!(
                client.restore_session(other).await.is_err(),
                "mismatched account must not restore"
            );
        }
    }

    #[tokio::test]
    async fn the_session_file_is_private_and_replaced_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session");
        std::fs::write(&path, b"old").unwrap();
        // A prior interrupted write may have left a temporary file with a
        // permissive mode; creation options do not change an existing inode.
        std::fs::write(path.with_extension("tmp"), b"interrupted").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                path.with_extension("tmp"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        let full: FullSession = serde_json::from_str(
            r#"{"client_session":{"homeserver":"http://hs","db_path":"/s","passphrase":"p"},
                "user_session":{"user_id":"@a:hs","device_id":"DEV","access_token":"tok"}}"#,
        )
        .unwrap();
        write_session(&path, &full).await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        persist_sync_token(&path, "s2".into()).await.unwrap();

        let saved: FullSession =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.client_session.passphrase, "p");
        assert_eq!(saved.sync_token.as_deref(), Some("s2"));
        // The old predictable temporary name is no longer opened or removed.
        assert_eq!(
            std::fs::read(path.with_extension("tmp")).unwrap(),
            b"interrupted"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
