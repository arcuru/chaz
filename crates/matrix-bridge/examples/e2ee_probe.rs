//! A scripted Matrix end-to-end-encryption client for `dev/matrix-e2e/run.sh`.
//!
//! The harness drives the rest of the test with `curl`, but encryption needs a
//! real Olm/Megolm client with its own device keys and cross-signing identity,
//! so this stands in for the human's Matrix client in the encrypted-room cases.
//! Each identity keeps its device store and sync position under `--dir`, so
//! successive invocations are the same device and only see new events.
//!
//! Not a chaz feature: nothing in the bridge uses it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, bail, ensure};
use clap::{Parser, Subcommand};
use matrix_sdk::Client;
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::deserialized_responses::{TimelineEventKind, UnableToDecryptReason, WithheldCode};
use matrix_sdk::ruma::api::Direction;
use matrix_sdk::ruma::api::client::message::get_message_events;
use matrix_sdk::ruma::api::client::room::create_room;
use matrix_sdk::ruma::api::client::uiaa;
use matrix_sdk::ruma::events::room::encryption::RoomEncryptionEventContent;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::events::{EmptyStateKey, InitialStateEvent};
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::{OwnedRoomId, OwnedUserId, UInt};
use serde::{Deserialize, Serialize};

#[derive(Parser)]
struct Args {
    /// Directory holding this device's store, session and sync position.
    #[arg(long)]
    dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Log a new device in. Unless `--unsigned`, sign it with the account's
    /// cross-signing identity (creating one if the account has none).
    Login {
        #[arg(long)]
        homeserver: String,
        #[arg(long)]
        user: String,
        #[arg(long)]
        password: String,
        #[arg(long)]
        unsigned: bool,
    },
    /// Create an encrypted direct room with `invite`; prints the room id.
    CreateDm {
        #[arg(long)]
        invite: OwnedUserId,
    },
    /// Wait until `user` has joined `room`.
    WaitJoined {
        #[arg(long)]
        room: OwnedRoomId,
        #[arg(long)]
        user: OwnedUserId,
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    /// Send an encrypted text message; prints its event id.
    Send {
        #[arg(long)]
        room: OwnedRoomId,
        #[arg(long)]
        body: String,
    },
    /// Wait for a new message from `sender` in `room`. By default it must be
    /// encrypted on the wire, decrypt, and contain `contains`. With
    /// `--withheld`, it must instead be undecryptable because its sender
    /// withheld the key from this device as unverified. Prints the event id.
    Expect {
        #[arg(long)]
        room: OwnedRoomId,
        #[arg(long)]
        sender: OwnedUserId,
        #[arg(long, default_value = "")]
        contains: String,
        #[arg(long)]
        withheld: bool,
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// Read `room`'s history from the server, undecrypted, and fail if
    /// `sender` ever sent a plaintext `m.room.message` there. Prints the
    /// number of encrypted events from `sender`.
    Wire {
        #[arg(long)]
        room: OwnedRoomId,
        #[arg(long)]
        sender: OwnedUserId,
    },
}

#[derive(Serialize, Deserialize)]
struct Saved {
    homeserver: String,
    session: MatrixSession,
    sync_token: Option<String>,
}

fn saved_path(dir: &Path) -> PathBuf {
    dir.join("probe-session.json")
}

async fn build(dir: &Path, homeserver: &str) -> anyhow::Result<Client> {
    Ok(Client::builder()
        .homeserver_url(homeserver)
        .sqlite_store(dir.join("store"), None)
        .build()
        .await?)
}

async fn restore(dir: &Path) -> anyhow::Result<(Client, Saved)> {
    let saved: Saved = serde_json::from_str(&std::fs::read_to_string(saved_path(dir))?)?;
    let client = build(dir, &saved.homeserver).await?;
    client.restore_session(saved.session.clone()).await?;
    Ok((client, saved))
}

/// One sync from the saved position; returns the response and saves the new
/// position so the next invocation starts after it.
async fn sync(
    client: &Client,
    saved: &mut Saved,
    dir: &Path,
) -> anyhow::Result<matrix_sdk::sync::SyncResponse> {
    let mut settings = SyncSettings::default().timeout(Duration::from_secs(2));
    if let Some(token) = &saved.sync_token {
        settings = settings.token(token.clone());
    }
    let response = client.sync_once(settings).await?;
    saved.sync_token = Some(response.next_batch.clone());
    std::fs::write(saved_path(dir), serde_json::to_string(saved)?)?;
    Ok(response)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    std::fs::create_dir_all(&args.dir)?;
    match args.command {
        Command::Login {
            homeserver,
            user,
            password,
            unsigned,
        } => {
            let client = build(&args.dir, &homeserver).await?;
            client
                .matrix_auth()
                .login_username(&user, &password)
                .initial_device_display_name(if unsigned { "probe-unsigned" } else { "probe" })
                .await?;
            let session = client.matrix_auth().session().context("no session")?;
            let mut saved = Saved {
                homeserver,
                session,
                sync_token: None,
            };
            let encryption = client.encryption();
            if !unsigned {
                let me = client.user_id().context("no user")?.to_owned();
                if encryption.request_user_identity(&me).await?.is_some() {
                    bail!("{me} already has a cross-signing identity; the probe only creates one");
                }
                if let Err(error) = encryption.bootstrap_cross_signing(None).await {
                    let response = error.as_uiaa_response().context("bootstrap failed")?;
                    let mut auth = uiaa::Password::new(
                        uiaa::UserIdentifier::Matrix(uiaa::MatrixUserIdentifier::new(user)),
                        password,
                    );
                    auth.session = response.session.clone();
                    encryption
                        .bootstrap_cross_signing(Some(uiaa::AuthData::Password(auth)))
                        .await?;
                }
            }
            sync(&client, &mut saved, &args.dir).await?;
            let own = encryption
                .get_own_device()
                .await?
                .context("no own device")?;
            ensure!(
                own.is_cross_signed_by_owner() != unsigned,
                "device cross-signing state is not what was asked for"
            );
            println!("{}", client.device_id().context("no device")?);
        }
        Command::CreateDm { invite } => {
            let (client, mut saved) = restore(&args.dir).await?;
            let encryption = InitialStateEvent::<RoomEncryptionEventContent>::new(
                EmptyStateKey,
                RoomEncryptionEventContent::with_recommended_defaults(),
            );
            let mut request = create_room::v3::Request::new();
            request.invite = vec![invite];
            request.is_direct = true;
            request.preset = Some(create_room::v3::RoomPreset::TrustedPrivateChat);
            request.initial_state = vec![Raw::new(&encryption)?.cast_unchecked()];
            let room = client.create_room(request).await?;
            sync(&client, &mut saved, &args.dir).await?;
            ensure!(
                room.latest_encryption_state().await?.is_encrypted(),
                "room was not created encrypted"
            );
            println!("{}", room.room_id());
        }
        Command::WaitJoined {
            room,
            user,
            timeout,
        } => {
            let (client, mut saved) = restore(&args.dir).await?;
            let deadline = Instant::now() + Duration::from_secs(timeout);
            loop {
                sync(&client, &mut saved, &args.dir).await?;
                let room = client.get_room(&room).context("unknown room")?;
                if let Some(member) = room.get_member_no_sync(&user).await?
                    && *member.membership()
                        == matrix_sdk::ruma::events::room::member::MembershipState::Join
                {
                    break;
                }
                ensure!(
                    Instant::now() < deadline,
                    "{user} did not join within {timeout}s"
                );
            }
        }
        Command::Send { room, body } => {
            let (client, mut saved) = restore(&args.dir).await?;
            sync(&client, &mut saved, &args.dir).await?;
            let room = client.get_room(&room).context("unknown room")?;
            ensure!(
                room.latest_encryption_state().await?.is_encrypted(),
                "room is not encrypted"
            );
            let response = room.send(RoomMessageEventContent::text_plain(body)).await?;
            println!("{}", response.response.event_id);
        }
        Command::Expect {
            room,
            sender,
            contains,
            withheld,
            timeout,
        } => {
            let (client, mut saved) = restore(&args.dir).await?;
            let deadline = Instant::now() + Duration::from_secs(timeout);
            loop {
                let response = sync(&client, &mut saved, &args.dir).await?;
                let events = response
                    .rooms
                    .joined
                    .get(&room)
                    .map(|update| update.timeline.events.clone())
                    .unwrap_or_default();
                for event in events {
                    let raw: serde_json::Value = event.raw().deserialize_as()?;
                    if raw["sender"].as_str() != Some(sender.as_str()) {
                        continue;
                    }
                    let id = raw["event_id"].as_str().unwrap_or_default().to_owned();
                    match &event.kind {
                        TimelineEventKind::PlainText { .. } => {
                            if raw["type"] == "m.room.message" {
                                bail!("{id} from {sender} arrived unencrypted");
                            }
                        }
                        TimelineEventKind::Decrypted(_) => {
                            ensure!(!withheld, "{id} decrypted, but its key should be withheld");
                            let body = raw["content"]["body"].as_str().unwrap_or_default();
                            if body.contains(&contains) {
                                println!("{id}");
                                return Ok(());
                            }
                        }
                        TimelineEventKind::UnableToDecrypt { utd_info, .. } => {
                            let reason = &utd_info.reason;
                            if withheld {
                                ensure!(
                                    matches!(
                                        reason,
                                        UnableToDecryptReason::MissingMegolmSession {
                                            withheld_code: Some(WithheldCode::Unverified)
                                        }
                                    ),
                                    "{id} is undecryptable, but not withheld as unverified: {reason:?}"
                                );
                                println!("{id}");
                                return Ok(());
                            }
                            eprintln!("{id} not decryptable yet: {reason:?}");
                        }
                    }
                }
                if Instant::now() >= deadline {
                    bail!("no matching message from {sender} within {timeout}s");
                }
            }
        }
        Command::Wire { room, sender } => {
            let (client, _) = restore(&args.dir).await?;
            let mut request = get_message_events::v3::Request::new(room, Direction::Backward);
            request.limit = UInt::from(500u32);
            let response = client.send(request).await?;
            let mut encrypted = 0;
            for event in response.chunk {
                let raw: serde_json::Value = event.deserialize_as()?;
                if raw["sender"].as_str() != Some(sender.as_str()) {
                    continue;
                }
                match raw["type"].as_str() {
                    Some("m.room.message") => bail!(
                        "{} from {sender} is a plaintext m.room.message on the server",
                        raw["event_id"]
                    ),
                    Some("m.room.encrypted") => encrypted += 1,
                    _ => {}
                }
            }
            println!("{encrypted}");
        }
    }
    Ok(())
}
