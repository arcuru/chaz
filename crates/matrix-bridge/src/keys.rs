//! Offline bridge maintenance. No Eidetica, agent, message or model entry points.
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use matrix_sdk::encryption::{CrossSigningResetAuthType, verification::VerificationRequestState};
use matrix_sdk::ruma::api::client::{backup::get_latest_backup_info, keys::get_keys, uiaa};
use matrix_sdk::ruma::events::key::verification::VerificationMethod;
use matrix_sdk::ruma::{OwnedDeviceId, OwnedUserId};
use matrix_sdk::{
    Client,
    config::{RequestConfig, SyncSettings},
};
use matrix_sdk_base::crypto::store::CryptoStore;
use matrix_sdk_sqlite::SqliteCryptoStore;
use serde::{Deserialize, Serialize};

use crate::bridge::client::{FullSession, check_device_key, encrypted_builder, write_session};
use crate::bridge::{Login, MatrixClient};
use crate::config::MatrixBridgeConfig;
use crate::store_lock::{StoreLock, no_symlinks, private_dir};

#[derive(clap::Args)]
pub struct KeysArgs {
    /// Login identifier from the config (required when there are several).
    #[arg(long)]
    login: Option<String>,
    #[command(subcommand)]
    command: KeysCommand,
}

#[derive(clap::Subcommand)]
enum KeysCommand {
    /// Inspect public fingerprints, recovery and own-account devices.
    Status,
    /// Create an identity only if the server has none.
    Init {
        #[arg(long)]
        account: OwnedUserId,
    },
    /// Explicitly replace this account's identity in a staged encrypted store.
    Reset {
        #[arg(long)]
        account: OwnedUserId,
    },
    /// Resolve a pending transition; never generates another identity.
    Resume {
        /// Re-authorize the SAME staged identity; interactive confirmation required.
        #[arg(long)]
        authorize: bool,
    },
    /// Discard a positively cancelled/rejected transition, retaining its store.
    Abort,
    /// Enable standard Matrix recovery and backup. Save a private recovery
    /// passphrase before contacting the server, so interruptions cannot lose it.
    RecoverySetup {
        #[arg(long)]
        output: PathBuf,
        /// Explicit permission to replace existing secret storage/backup.
        #[arg(long)]
        replace_existing: bool,
    },
    /// Restore identity secrets and standard backup keys, not trusted history.
    RecoveryRestore {
        #[arg(long)]
        key_file: PathBuf,
    },
    /// SAS-verify one selected device of this account. Never auto-approve.
    Verify {
        #[arg(long)]
        user: OwnedUserId,
        #[arg(long)]
        device: OwnedDeviceId,
        #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=600))]
        timeout: u64,
    },
}

fn sdk_error<E>(message: &'static str) -> impl FnOnce(E) -> anyhow::Error {
    move |_| anyhow::anyhow!(message)
}

/// A byte-at-a-time, bounded terminal/pipe reader. Unlike tokio's blocking
/// stdin thread, this cannot keep the process alive after a SAS timeout.
async fn input() -> anyhow::Result<String> {
    let mut line = Vec::new();
    loop {
        let mut fd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized descriptor; stdin is borrowed, not closed.
        let ready = unsafe { libc::poll(&mut fd, 1, 0) };
        ensure!(ready >= 0, "cannot read operator confirmation");
        if ready == 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }
        let mut byte = [0u8];
        // SAFETY: valid one-byte buffer; no buffering may hide input from poll.
        let count = unsafe { libc::read(0, byte.as_mut_ptr().cast(), 1) };
        ensure!(count >= 0, "cannot read operator confirmation");
        ensure!(count != 0, "operator cancelled (end of input)");
        if byte[0] == b'\n' {
            return String::from_utf8(line).context("invalid confirmation");
        }
        ensure!(line.len() < 512, "confirmation is too long");
        line.push(byte[0]);
    }
}

async fn confirm(expected: &str) -> anyhow::Result<()> {
    println!("Type {expected} to continue; anything else cancels:");
    std::io::stdout().flush()?;
    ensure!(
        input().await? == expected,
        "operator cancelled; no identity upload authorized"
    );
    Ok(())
}

fn random_secret() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("system RNG unavailable"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Reserve with O_EXCL/O_NOFOLLOW. Even dangling symlinks and empty existing
/// files are refused; there is no overwrite flag for recovery material.
fn secret_output(path: &Path, secret: &str) -> anyhow::Result<()> {
    no_symlinks(path)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let meta = std::fs::metadata(parent)?;
    ensure!(
        meta.is_dir() && meta.permissions().mode() & 0o077 == 0,
        "recovery destination directory must already exist with mode 0700"
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .context(
            "recovery file already exists or cannot be created; choose a new private destination",
        )?;
    file.write_all(secret.as_bytes())?;
    file.sync_all()?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn read_secret(path: &Path) -> anyhow::Result<String> {
    no_symlinks(path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.permissions().mode() & 0o077 == 0 && meta.len() <= 4096,
        "recovery input must be a private regular file (mode 0600, at most 4096 bytes)"
    );
    let mut secret = String::new();
    file.read_to_string(&mut secret)?;
    ensure!(!secret.trim().is_empty(), "recovery input is empty");
    Ok(secret)
}

fn load_session(root: &Path) -> anyhow::Result<FullSession> {
    let file = root.join("session");
    no_symlinks(&file)?;
    serde_json::from_slice(&std::fs::read(file)?).context("invalid saved Matrix session")
}

async fn open(full: &FullSession) -> anyhow::Result<Client> {
    let client = encrypted_builder(&full.client_session)
        .request_config(
            RequestConfig::default()
                .timeout(Duration::from_secs(15))
                .retry_limit(0),
        )
        .build()
        .await
        .map_err(|_| anyhow::anyhow!("cannot open encrypted Matrix store"))?;
    client
        .restore_session(full.user_session.clone())
        .await
        .map_err(sdk_error(
            "cannot restore Matrix login; check configured account authentication",
        ))?;
    check_device_key(&client, full.client_session.device_ed25519_key.as_deref()).await.map_err(|_| anyhow::anyhow!("Matrix device/store fingerprint check failed or query inconclusive; preserve the original session and store"))?;
    Ok(client)
}

async fn published(client: &Client) -> anyhow::Result<Option<String>> {
    let user = client
        .user_id()
        .context("no authenticated Matrix account")?;
    let mut request = get_keys::v3::Request::new();
    request.device_keys.insert(user.to_owned(), vec![]);
    let response = client.send(request).await.map_err(sdk_error(
        "published identity query failed; transition remains unresolved",
    ))?;
    ensure!(
        response.failures.is_empty(),
        "published identity query inconclusive; transition remains unresolved"
    );
    let Some(raw) = response.master_keys.get(user) else {
        return Ok(None);
    };
    let key: serde_json::Value = raw.deserialize_as()?;
    ensure!(
        key["user_id"].as_str() == Some(user.as_str()),
        "published identity belongs to a different account"
    );
    let keys = key["keys"]
        .as_object()
        .context("invalid published identity")?;
    ensure!(keys.len() == 1, "invalid published master key count");
    Ok(Some(
        keys.values()
            .next()
            .and_then(|v| v.as_str())
            .context("invalid published master fingerprint")?
            .to_owned(),
    ))
}

async fn local_master(full: &FullSession) -> anyhow::Result<Option<String>> {
    let store = SqliteCryptoStore::open(
        &full.client_session.db_path,
        Some(&full.client_session.passphrase),
    )
    .await?;
    let Some(identity) = store.load_identity().await? else {
        return Ok(None);
    };
    Ok(identity
        .master_public_key()
        .await
        .and_then(|k| k.get_first_key().map(|k| k.to_base64())))
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
enum Phase {
    Prepared,
    Uncertain,
    Cancelled,
    Rejected,
    Committing,
}

#[derive(Serialize, Deserialize)]
struct Pending {
    version: u8,
    account: String,
    old_master: Option<String>,
    old_store: PathBuf,
    staged: FullSession,
    phase: Phase,
}

fn journal(root: &Path, pending: &Pending) -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    file.write_all(&serde_json::to_vec(pending)?)?;
    file.as_file().sync_all()?;
    file.persist(root.join("pending-reset.json"))?;
    File::open(root)?.sync_all()?;
    Ok(())
}

fn load_pending(root: &Path) -> anyhow::Result<Pending> {
    let path = root.join("pending-reset.json");
    no_symlinks(&path)?;
    let pending: Pending = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(
        pending.version == 1,
        "unsupported transition journal; preserve it for repair"
    );
    Ok(pending)
}

/// Copy all encrypted SQLite files (including WAL) before opening any SDK
/// client. The process lease excludes writers. Never delete the old generation.
fn copy_store(from: &Path, to: &Path) -> anyhow::Result<()> {
    private_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        ensure!(
            kind.is_file(),
            "unexpected entry in Matrix store; refusing staged copy"
        );
        if entry.file_name() == "ownership.lock" {
            continue;
        }
        let dest = to.join(entry.file_name());
        std::fs::copy(entry.path(), &dest)?;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600))?;
        File::open(&dest)?.sync_all()?;
    }
    File::open(to)?.sync_all()?;
    Ok(())
}

async fn resolve(root: &Path, pending: &mut Pending, client: &Client) -> anyhow::Result<bool> {
    let remote = published(client).await?;
    let candidate = local_master(&pending.staged).await?;
    if candidate != pending.old_master && candidate.is_some() && remote == candidate {
        ensure!(
            client
                .encryption()
                .cross_signing_status()
                .await
                .is_some_and(|s| s.is_complete()),
            "server committed but staged secrets incomplete; preserve pending state for repair"
        );
        client
            .encryption()
            .request_user_identity(client.user_id().context("no account")?)
            .await
            .map_err(sdk_error(
                "server committed; cannot refresh identity; run keys resume",
            ))?;
        client
            .encryption()
            .get_own_device()
            .await
            .map_err(sdk_error("cannot query own device"))?
            .context("own device missing")?
            .verify()
            .await
            .map_err(|_| {
                anyhow::anyhow!("server committed; device signature upload failed; run keys resume")
            })?;
        pending.phase = Phase::Committing;
        journal(root, pending)?;
        // The only commit point is atomic replacement of this pointer. Both
        // stores survive a crash before/after it; resume is idempotent.
        write_session(&root.join("session"), &pending.staged).await?;
        archive_journal(root)?;
        println!("Identity transition committed; previous encrypted store retained.");
        return Ok(true);
    }
    if remote == pending.old_master {
        match pending.phase {
            Phase::Prepared | Phase::Cancelled | Phase::Rejected => println!(
                "Server identity unchanged; transition rejected/cancelled. Active keys preserved. Run keys abort."
            ),
            _ => println!(
                "Server currently has the previous identity; outcome ambiguous. Active and staged keys retained. Run keys resume later; bridge remains stopped."
            ),
        }
    } else {
        println!(
            "Published identity matches neither transition endpoint; preserve both stores and recover the account explicitly."
        );
    }
    Ok(false)
}

fn archive_journal(root: &Path) -> anyhow::Result<()> {
    let dest = root.join(format!("transition-{}.json", random_secret()?));
    std::fs::rename(root.join("pending-reset.json"), dest)?;
    File::open(root)?.sync_all()?;
    Ok(())
}

async fn transition(
    root: &Path,
    full: FullSession,
    creds: &Login,
    init: bool,
    account: &OwnedUserId,
    mut lease: StoreLock,
) -> anyhow::Result<()> {
    ensure!(
        full.user_session.meta.user_id == *account,
        "confirmation account differs from authenticated account"
    );
    confirm(&format!(
        "{} {account}",
        if init { "INIT" } else { "RESET" }
    ))
    .await?;
    let mut staged = full.clone();
    staged.client_session.db_path = root.join(format!("store-{}", random_secret()?));
    copy_store(&full.client_session.db_path, &staged.client_session.db_path)?;
    lease.lock_store(&staged.client_session.db_path)?;
    let client = open(&staged).await?;
    client.add_event_handler_context(std::sync::Arc::new(lease));
    let old_master = published(&client).await?;
    ensure!(
        !init || old_master.is_none(),
        "published identity already exists; use recovery or explicit reset, never init"
    );
    let mut pending = Pending {
        version: 1,
        account: account.to_string(),
        old_master,
        old_store: full.client_session.db_path,
        staged,
        phase: Phase::Prepared,
    };
    journal(root, &pending)?;
    pending.phase = Phase::Uncertain;
    journal(root, &pending)?;
    // This API mutates local keys before UIAA. Only the staged store is open.
    match client.encryption().reset_cross_signing().await {
        Ok(Some(handle)) => match handle.auth_type() {
            CrossSigningResetAuthType::Uiaa(info)
                if info
                    .flows
                    .iter()
                    .any(|f| f.stages.len() == 1 && f.stages[0].as_str() == "m.login.password") =>
            {
                if confirm(&format!("AUTHORIZE {account}")).await.is_err() {
                    handle.cancel().await;
                    pending.phase = Phase::Cancelled;
                } else if let Some(password) = &creds.password {
                    let mut auth = uiaa::Password::new(
                        uiaa::UserIdentifier::Matrix(uiaa::MatrixUserIdentifier::new(
                            account.to_string(),
                        )),
                        password.clone(),
                    );
                    auth.session = info.session.clone();
                    if let Err(error) = handle.auth(Some(uiaa::AuthData::Password(auth))).await
                        && error
                            .as_uiaa_response()
                            .is_some_and(|i| i.auth_error.is_some())
                    {
                        pending.phase = Phase::Rejected;
                    }
                } else {
                    handle.cancel().await;
                    pending.phase = Phase::Cancelled;
                    println!(
                        "Password UIAA requires a configured password; no credentials accepted in arguments."
                    );
                }
            }
            _ => {
                handle.cancel().await;
                pending.phase = Phase::Cancelled;
                println!(
                    "Unsupported homeserver authentication flow; only password UIAA is supported. No authorization sent."
                );
            }
        },
        Ok(None) => (),
        Err(_) => println!(
            "Identity upload did not complete; resolving published identity before any commit."
        ),
    }
    journal(root, &pending)?;
    ensure!(
        resolve(root, &mut pending, &client).await?,
        "transition not committed; keys retained in pending state"
    );
    Ok(())
}

async fn status(client: &Client, full: &FullSession) -> anyhow::Result<()> {
    println!(
        "Account: {}\nDevice: {}\nDevice fingerprint: {}",
        client.user_id().context("no user")?,
        client.device_id().context("no device")?,
        client
            .encryption()
            .ed25519_key()
            .await
            .context("no device key")?
    );
    println!(
        "Published master: {}\nLocal private master: {}",
        published(client).await?.as_deref().unwrap_or("none"),
        local_master(full).await?.as_deref().unwrap_or("none")
    );
    println!(
        "Identity secrets complete: {}",
        client
            .encryption()
            .cross_signing_status()
            .await
            .is_some_and(|s| s.is_complete())
    );
    println!(
        "Secret storage: {}\nBackup enabled locally: {}",
        client
            .encryption()
            .secret_storage()
            .is_enabled()
            .await
            .map_err(|_| anyhow::anyhow!("cannot query recovery status"))?,
        client.encryption().backups().are_enabled().await
    );
    client
        .encryption()
        .request_user_identity(client.user_id().context("no user")?)
        .await
        .map_err(sdk_error("cannot query own identity"))?;
    let devices = client
        .encryption()
        .get_user_devices(client.user_id().context("no user")?)
        .await
        .map_err(sdk_error("cannot query own devices"))?;
    for device in devices.devices() {
        println!(
            "Device {} cross-signed: {}",
            device.device_id(),
            device.is_cross_signed_by_owner()
        );
    }
    Ok(())
}

async fn recovery_setup(client: &Client, output: &Path, replace: bool) -> anyhow::Result<()> {
    ensure!(
        client
            .encryption()
            .cross_signing_status()
            .await
            .is_some_and(|s| s.is_complete()),
        "identity secrets missing; restore or initialize explicitly before enabling recovery"
    );
    let existing = client
        .encryption()
        .secret_storage()
        .is_enabled()
        .await
        .map_err(|_| anyhow::anyhow!("cannot query secret storage"))?;
    let backup = client
        .encryption()
        .backups()
        .fetch_exists_on_server()
        .await
        .map_err(|_| anyhow::anyhow!("cannot query backup"))?;
    if existing || backup {
        ensure!(
            replace,
            "recovery/backup already exists; restore it, or explicitly use --replace-existing (deletes the previous server backup)"
        );
        confirm(&format!(
            "REPLACE RECOVERY {}",
            client.user_id().context("no user")?
        ))
        .await?;
    }
    // Native standard Matrix passphrase recovery: publish a durable random
    // passphrase BEFORE SDK setup. Even death mid-upload cannot lose the secret.
    let secret = random_secret()?;
    secret_output(output, &secret)?;
    if backup && replace {
        client.encryption().backups().disable_and_delete().await.map_err(|_| anyhow::anyhow!("backup replacement interrupted; recovery file retained; inspect status before retrying"))?;
    }
    client.encryption().recovery().enable().with_passphrase(&secret).await
        .map_err(|_| anyhow::anyhow!("recovery setup interrupted; saved passphrase retained; use recovery-restore with this file before replacing recovery"))?;
    client.encryption().backups().wait_for_steady_state().await
        .map_err(|_| anyhow::anyhow!("identity recovery enabled; room-key upload incomplete; retry backup upload before losing this store"))?;
    println!(
        "Recovery enabled; private recovery passphrase saved. Standard encrypted backup uploaded."
    );
    Ok(())
}

async fn recovery_restore(
    client: &Client,
    full: &FullSession,
    key_file: &Path,
) -> anyhow::Result<()> {
    let before = published(client)
        .await?
        .context("no published identity to recover")?;
    let secret = read_secret(key_file)?;
    client.encryption().recovery().recover(secret.trim()).await
        .map_err(|_| anyhow::anyhow!("recovery failed; incorrect secret, incomplete recovery data or unreachable homeserver; identity was not reset"))?;
    ensure!(
        local_master(full).await?.as_deref() == Some(before.as_str())
            && published(client).await?.as_deref() == Some(before.as_str()),
        "recovered identity does not match published identity; stop and preserve the store"
    );
    ensure!(
        client
            .encryption()
            .cross_signing_status()
            .await
            .is_some_and(|s| s.is_complete()),
        "recovery is missing private identity secrets"
    );
    // Enumerate the standard backup, not only joined rooms (which may have
    // been left). The SDK decrypts and imports the actual encrypted keys.
    let info = client
        .send(get_latest_backup_info::v3::Request::new())
        .await
        .map_err(sdk_error("identity restored; cannot query key backup"))?;
    let response = client
        .send(
            matrix_sdk::ruma::api::client::backup::get_backup_keys::v3::Request::new(info.version),
        )
        .await
        .map_err(sdk_error("identity restored; cannot enumerate key backup"))?;
    for room in response.rooms.keys() {
        client
            .encryption()
            .backups()
            .download_room_keys_for_room(room)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "identity restored; standard backup download failed; repeat recovery-restore"
                )
            })?;
    }
    let crypto = SqliteCryptoStore::open(
        &full.client_session.db_path,
        Some(&full.client_session.passphrase),
    )
    .await?;
    let imported = crypto.get_inbound_group_sessions().await?;
    for (room, keys) in &response.rooms {
        for id in keys.sessions.keys() {
            ensure!(
                imported
                    .iter()
                    .any(|key| key.room_id() == room && key.session_id() == id.as_str()),
                "identity restored, but a backed-up room key is missing/invalid; preserve store and retry recovery"
            );
        }
    }
    println!(
        "Same identity restored; standard encrypted room keys imported. Imported history is NOT authenticated bridge/model input."
    );
    Ok(())
}

async fn verify(
    client: &Client,
    user: &OwnedUserId,
    device: &OwnedDeviceId,
    seconds: u64,
) -> anyhow::Result<()> {
    ensure!(
        client.user_id() == Some(user.as_ref()),
        "other-user verification is forbidden; select a device of the configured account"
    );
    ensure!(
        client.device_id() != Some(device.as_ref()),
        "cannot SAS-verify this device with itself"
    );
    client
        .encryption()
        .request_user_identity(user)
        .await
        .map_err(sdk_error("cannot query own account"))?;
    let target = client
        .encryption()
        .get_device(user, device)
        .await
        .map_err(sdk_error("cannot query selected device"))?
        .context("selected own-account device not found")?;
    let request = target
        .request_verification_with_methods(vec![VerificationMethod::SasV1])
        .await
        .map_err(sdk_error("cannot start SAS request"))?;
    let flow = request.flow_id().to_owned();
    println!("SAS account: {user}\nSAS device: {device}\nSAS transaction: {flow}");
    let operation = async {
        let mut sas = None;
        let mut approved = false;
        loop {
            ensure!(!request.is_cancelled(), "SAS rejected/cancelled by peer");
            if sas.is_none()
                && let VerificationRequestState::Ready {
                    other_device_data, ..
                } = request.state()
            {
                ensure!(
                    other_device_data.user_id() == user && other_device_data.device_id() == device,
                    "SAS response belongs to a different account/device"
                );
                sas = request
                    .start_sas()
                    .await
                    .map_err(sdk_error("cannot start selected SAS transaction"))?;
            }
            if let Some(sas) = &sas {
                ensure!(
                    sas.other_user_id() == user
                        && sas.other_device().device_id() == device
                        && request.flow_id() == flow,
                    "SAS binding changed"
                );
                ensure!(!sas.is_cancelled(), "SAS mismatch/rejection/cancellation");
                if sas.can_be_presented() && !approved {
                    let (a, b, c) = sas.decimals().context("SAS decimals unavailable")?;
                    println!("Compare SAS decimals on BOTH devices: {a} {b} {c}");
                    let expected = format!("MATCH {user} {device} {flow}");
                    println!(
                        "Type {expected} only if they match; type mismatch or cancel otherwise:"
                    );
                    std::io::stdout().flush()?;
                    let answer = input().await?;
                    if answer == "mismatch" {
                        sas.mismatch()
                            .await
                            .map_err(sdk_error("cannot send SAS mismatch"))?;
                        bail!("SAS mismatch; not verified");
                    }
                    if answer != expected {
                        sas.cancel().await.map_err(sdk_error("cannot cancel SAS"))?;
                        bail!("operator cancelled SAS; not verified");
                    }
                    sas.confirm()
                        .await
                        .map_err(sdk_error("SAS confirmation failed"))?;
                    approved = true;
                }
                if sas.is_done() {
                    ensure!(
                        approved,
                        "SAS finished without operator approval; refusing success"
                    );
                    println!("SAS verified selected own-account device after explicit match.");
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let syncing = async {
        loop {
            client
                .sync_once(SyncSettings::default().timeout(Duration::from_secs(1)))
                .await
                .map_err(sdk_error("SAS sync failed; not verified"))?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::time::timeout(Duration::from_secs(seconds), async {
        tokio::select! { result = operation => result, result = syncing => result }
    })
    .await;
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            let _ = request.cancel().await;
            Err(error)
        }
        Err(_) => {
            let _ = request.cancel().await;
            bail!("SAS timed out; no operator match completed")
        }
    }
}

/// Entry point for the CLI; intentionally never opens chaz's database.
pub async fn run(config: &MatrixBridgeConfig, args: &KeysArgs) -> anyhow::Result<()> {
    let entries: Vec<_> = config
        .logins
        .iter()
        .filter(|l| {
            args.login
                .as_deref()
                .is_none_or(|id| l.login.login_id() == id)
        })
        .collect();
    ensure!(
        entries.len() == 1,
        "select exactly one configured login using keys --login"
    );
    let (login_id, creds) = entries[0]
        .to_credentials()
        .map_err(|_| anyhow::anyhow!("cannot resolve configured Matrix credentials"))?;
    let login = Login {
        homeserver_url: creds.homeserver_url,
        username: creds.username,
        password: creds.password,
    };
    if let KeysCommand::Init { account } | KeysCommand::Reset { account } = &args.command {
        ensure!(
            account.as_str() == login.username,
            "confirmation account differs from configured account"
        );
    }
    if let KeysCommand::Verify { user, .. } = &args.command {
        ensure!(
            user.as_str() == login.username,
            "other-user verification is forbidden"
        );
    }
    let base = config
        .state_dir
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| dirs::state_dir().map(|d| d.join("chaz-matrix")))
        .context("no state directory")?;
    let root = base
        .join("matrix")
        .join(crate::config::sanitize_login_id(&login_id));
    if !root.join("session").exists() {
        ensure!(
            !matches!(
                args.command,
                KeysCommand::Resume { .. } | KeysCommand::Abort
            ),
            "no saved session for transition"
        );
        let mc = MatrixClient::login(
            &login,
            Some(root.to_str().context("non-UTF8 state path")?),
            "chaz",
        )
        .await.map_err(|_| anyhow::anyhow!("cannot provision Matrix login/store; stop other writers and check configured authentication; no identity reset performed"))?;
        ensure!(
            mc.client().user_id().map(|u| u.as_str()) == Some(login.username.as_str()),
            "authenticated account differs from configured account"
        );
        mc.client()
            .sync_once(SyncSettings::default().timeout(Duration::from_secs(1)))
            .await
            .map_err(sdk_error(
                "new login saved; initial crypto sync failed; rerun maintenance",
            ))?;
        // Keep SDK background tasks from writing during a staged snapshot.
        // A new login has no transition to stage; init/reset require a second
        // invocation after this process exits.
        println!(
            "New Matrix device saved. Run the maintenance command again against this persisted login."
        );
        return Ok(());
    }
    let mut lease = StoreLock::acquire(&root)?;
    let root = lease.root.clone();
    let full = load_session(&root)?;
    ensure!(
        full.user_session.meta.user_id.as_str() == login.username
            && full.client_session.homeserver == login.homeserver_url,
        "saved account/homeserver differs from configuration; refusing maintenance"
    );
    lease.lock_store(&full.client_session.db_path)?;
    if root.join("pending-reset.json").exists() {
        let mut pending = load_pending(&root)?;
        ensure!(
            pending.account == login.username
                && pending.staged.user_session.meta == full.user_session.meta
                && (full.client_session.db_path == pending.old_store
                    || full.client_session.db_path == pending.staged.client_session.db_path),
            "pending transition account/device/store differs; preserve journal for repair"
        );
        lease.lock_store(&pending.staged.client_session.db_path)?;
        let client = open(&pending.staged).await?;
        client.add_event_handler_context(std::sync::Arc::new(lease));
        if matches!(args.command, KeysCommand::Abort) {
            ensure!(
                matches!(
                    pending.phase,
                    Phase::Prepared | Phase::Cancelled | Phase::Rejected
                ) && published(&client).await? == pending.old_master,
                "cannot abort an ambiguous/server-committed transition; use keys resume"
            );
            archive_journal(&root)?;
            println!(
                "Cancelled/rejected transition archived; active identity unchanged; staged keys retained."
            );
            return Ok(());
        }
        ensure!(
            matches!(
                args.command,
                KeysCommand::Resume { .. } | KeysCommand::Status
            ),
            "pending transition; only status, resume or safe abort allowed"
        );
        if matches!(args.command, KeysCommand::Status) {
            println!(
                "Pending transition; active store retained. Published master: {}\nStaged master: {}",
                published(&client).await?.as_deref().unwrap_or("none"),
                local_master(&pending.staged)
                    .await?
                    .as_deref()
                    .unwrap_or("none")
            );
            return Ok(());
        }
        if matches!(args.command, KeysCommand::Resume { authorize: true })
            && published(&client).await? == pending.old_master
        {
            ensure!(
                local_master(&pending.staged)
                    .await?
                    .is_some_and(|k| Some(k) != pending.old_master),
                "no replacement identity was generated; preserve journal and inspect state"
            );
            confirm(&format!("AUTHORIZE {}", pending.account)).await?;
            pending.phase = Phase::Uncertain;
            journal(&root, &pending)?;
            if let Err(error) = client.encryption().bootstrap_cross_signing(None).await
                && let Some(info) = error.as_uiaa_response()
            {
                ensure!(
                    info.flows
                        .iter()
                        .any(|f| f.stages.len() == 1 && f.stages[0].as_str() == "m.login.password"),
                    "unsupported authentication flow; pending store retained"
                );
                let mut auth = uiaa::Password::new(
                    uiaa::UserIdentifier::Matrix(uiaa::MatrixUserIdentifier::new(
                        pending.account.clone(),
                    )),
                    login
                        .password
                        .clone()
                        .context("configured password required")?,
                );
                auth.session = info.session.clone();
                if let Err(error) = client
                    .encryption()
                    .bootstrap_cross_signing(Some(uiaa::AuthData::Password(auth)))
                    .await
                    && error
                        .as_uiaa_response()
                        .is_some_and(|i| i.auth_error.is_some())
                {
                    pending.phase = Phase::Rejected;
                }
            }
            journal(&root, &pending)?;
        }
        ensure!(
            resolve(&root, &mut pending, &client).await?,
            "transition remains unresolved; active and staged keys retained"
        );
        return Ok(());
    }
    match &args.command {
        KeysCommand::Init { account } | KeysCommand::Reset { account } => {
            transition(
                &root,
                full,
                &login,
                matches!(args.command, KeysCommand::Init { .. }),
                account,
                lease,
            )
            .await
        }
        KeysCommand::Resume { .. } | KeysCommand::Abort => bail!("no pending identity transition"),
        command => {
            let client = open(&full).await?;
            client.add_event_handler_context(std::sync::Arc::new(lease));
            match command {
                KeysCommand::Status => status(&client, &full).await,
                KeysCommand::RecoverySetup {
                    output,
                    replace_existing,
                } => recovery_setup(&client, output, *replace_existing).await,
                KeysCommand::RecoveryRestore { key_file } => {
                    recovery_restore(&client, &full, key_file).await
                }
                KeysCommand::Verify {
                    user,
                    device,
                    timeout,
                } => verify(&client, user, device, *timeout).await,
                _ => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_recovery_files_never_overwrite_or_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let key = dir.path().join("key");
        secret_output(&key, "private-test-secret").unwrap();
        assert_eq!(
            std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(secret_output(&key, "replacement").is_err());
        assert_eq!(read_secret(&key).unwrap(), "private-test-secret");
        std::os::unix::fs::symlink(&key, dir.path().join("link")).unwrap();
        assert!(secret_output(&dir.path().join("link"), "x").is_err());
        assert!(read_secret(&dir.path().join("link")).is_err());
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_secret(&key).is_err());
    }
}
