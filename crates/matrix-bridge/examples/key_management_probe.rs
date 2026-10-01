//! Disposable fixture/peer for dev/matrix-e2e/key-management.py. No models.
//! Untrusted decryption below is a TEST diagnostic only, never a bridge policy.
use anyhow::{Context, ensure};
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::ruma::api::client::room::create_room;
use matrix_sdk::ruma::events::key::verification::VerificationMethod;
use matrix_sdk::ruma::events::room::{
    encryption::RoomEncryptionEventContent, message::RoomMessageEventContent,
};
use matrix_sdk::ruma::events::{EmptyStateKey, InitialStateEvent};
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId, OwnedUserId};
use matrix_sdk::{Client, config::SyncSettings};
use matrix_sdk_base::crypto::{DecryptionSettings, TrustRequirement, store::CryptoStore};
use matrix_sdk_sqlite::SqliteCryptoStore;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(90), run()).await?
}

async fn run() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(args.get(1).context("login state dir")?);
    let op = args.get(2).context("fixture | inspect | diagnose | sas")?;
    let saved: Value = serde_json::from_slice(&std::fs::read(dir.join("session"))?)?;
    let cs = &saved["client_session"];
    let hs = cs["homeserver"].as_str().context("homeserver")?;
    ensure!(
        hs.starts_with("http://127.0.0.1:"),
        "disposable loopback only"
    );
    let db = cs["db_path"].as_str().context("store")?;
    let passphrase = cs["passphrase"].as_str().context("store passphrase")?;
    let crypto = SqliteCryptoStore::open(db, Some(passphrase)).await?;
    let master = if let Some(identity) = crypto.load_identity().await? {
        identity
            .master_public_key()
            .await
            .and_then(|k| k.get_first_key().map(|k| k.to_base64()))
    } else {
        None
    };
    let identity_keys = if let Some(identity) = crypto.load_identity().await? {
        vec![
            identity
                .master_public_key()
                .await
                .and_then(|k| k.get_first_key().map(|k| k.to_base64())),
            identity
                .self_signing_public_key()
                .await
                .and_then(|k| k.get_first_key().map(|k| k.to_base64())),
            identity
                .user_signing_public_key()
                .await
                .and_then(|k| k.get_first_key().map(|k| k.to_base64())),
        ]
    } else {
        vec![None, None, None]
    };
    let sessions = crypto.get_inbound_group_sessions().await?;
    if op == "inspect" {
        println!(
            "{}",
            json!({"master":master, "identity_keys":identity_keys, "keys":sessions.len(), "imported":sessions.iter().filter(|s| s.has_been_imported()).count()})
        );
        return Ok(());
    }
    drop(crypto);
    let client = Client::builder()
        .homeserver_url(hs)
        .sqlite_store(db, Some(passphrase))
        .with_decryption_settings(DecryptionSettings {
            sender_device_trust_requirement: if op == "diagnose" {
                TrustRequirement::Untrusted
            } else {
                TrustRequirement::CrossSigned
            },
        })
        .build()
        .await?;
    let session: MatrixSession = serde_json::from_value(saved["user_session"].clone())?;
    client.restore_session(session).await?;
    if op == "fixture" {
        client
            .sync_once(SyncSettings::default().timeout(Duration::from_secs(1)))
            .await?;
        let mut request = create_room::v3::Request::new();
        request.preset = Some(create_room::v3::RoomPreset::PrivateChat);
        request.initial_state = vec![
            Raw::new(&InitialStateEvent::new(
                EmptyStateKey,
                RoomEncryptionEventContent::with_recommended_defaults(),
            ))?
            .cast_unchecked(),
        ];
        let room = client.create_room(request).await?;
        client
            .sync_once(SyncSettings::default().timeout(Duration::from_secs(1)))
            .await?;
        let event = room
            .send(RoomMessageEventContent::text_plain(
                "post-cutover-backup-fixture",
            ))
            .await?
            .response
            .event_id;
        let plain: Value = room.event(&event, None).await?.raw().deserialize_as()?;
        ensure!(
            plain["content"]["body"] == "post-cutover-backup-fixture",
            "original strict control failed"
        );
        println!(
            "{}",
            json!({"room":room.room_id(), "event":event, "master":master})
        );
    } else if op == "diagnose" {
        let control: Value =
            serde_json::from_slice(&std::fs::read(args.get(3).context("fixture file")?)?)?;
        let room: OwnedRoomId = control["room"].as_str().context("room")?.parse()?;
        let event: OwnedEventId = control["event"].as_str().context("event")?.parse()?;
        ensure!(
            master == control["master"].as_str().map(str::to_owned),
            "wrong private identity"
        );
        ensure!(
            sessions
                .iter()
                .any(|s| s.has_been_imported() && s.room_id() == room),
            "real backup key not imported"
        );
        client
            .sync_once(SyncSettings::default().timeout(Duration::from_secs(1)))
            .await?;
        let plain: Value = client
            .get_room(&room)
            .context("no fixture room")?
            .event(&event, None)
            .await?
            .raw()
            .deserialize_as()?;
        ensure!(
            plain["content"]["body"] == "post-cutover-backup-fixture",
            "restored key material cannot decrypt fixture"
        );
        println!(
            "PASS test-only Untrusted diagnostic: same private identity and actual imported backup key decrypt exact post-cutover fixture; not production trust evidence"
        );
    } else {
        ensure!(op == "sas", "unknown op");
        let mode = args.get(3).context("SAS mode")?;
        let user: OwnedUserId = client.user_id().context("no user")?.to_owned();
        let until = Instant::now() + Duration::from_secs(60);
        let incoming = std::sync::Arc::new(std::sync::Mutex::new(None));
        let capture = incoming.clone();
        client.add_event_handler(move |event: matrix_sdk::ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEvent| {
            let capture = capture.clone();
            async move { *capture.lock().unwrap() = Some((event.sender, event.content.transaction_id)); }
        });
        client
            .sync_once(SyncSettings::default().timeout(Duration::from_secs(1)))
            .await?;
        println!("PEER READY");
        let mut selected = None;
        let mut sas = None;
        let mut confirmed = false;
        loop {
            ensure!(Instant::now() < until, "peer SAS timed out");
            if selected.is_none() {
                let received = incoming.lock().unwrap().take();
                if let Some((sender, flow)) = received
                    && sender == user
                {
                    let request = client
                        .encryption()
                        .get_verification_request(&sender, &flow)
                        .await
                        .context("no request for incoming event")?;
                    if !request.we_started() && !request.is_cancelled() {
                        if mode == "reject" {
                            request.cancel().await?;
                            println!("PASS peer rejected request");
                            return Ok(());
                        }
                        request
                            .accept_with_methods(vec![VerificationMethod::SasV1])
                            .await?;
                        selected = Some(request);
                    }
                }
            }
            if let Some(request) = &selected {
                if request.is_cancelled() {
                    println!("PASS peer observed cancellation");
                    return Ok(());
                }
                if sas.is_none()
                    && let Some(verification) = client
                        .encryption()
                        .get_verification(&user, request.flow_id())
                        .await
                    && let Some(flow) = verification.sas()
                {
                    flow.accept().await?;
                    sas = Some(flow);
                }
            }
            if let Some(sas) = &sas {
                if sas.is_cancelled() {
                    println!("PASS peer observed SAS cancellation");
                    return Ok(());
                }
                if sas.can_be_presented() && !confirmed {
                    let (a, b, c) = sas.decimals().context("no decimals")?;
                    println!("PEER SAS {a} {b} {c}");
                    if mode == "cancel" {
                        sas.cancel().await?;
                        println!("PASS peer cancelled SAS");
                        return Ok(());
                    }
                    sas.confirm().await?;
                    confirmed = true;
                }
                if sas.is_done() {
                    println!("PASS peer completed real SAS");
                    return Ok(());
                }
            }
            client
                .sync_once(SyncSettings::default().timeout(Duration::from_secs(1)))
                .await?;
        }
    }
    Ok(())
}
