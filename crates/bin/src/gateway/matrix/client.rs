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

use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::ruma::api::client::filter::FilterDefinition;
use matrix_sdk::ruma::events::room::member::StrippedRoomMemberEvent;
use matrix_sdk::{Client, Error, LoopCtrl, Room, RoomMemberships, config::SyncSettings};
use regex::Regex;
use serde::{Deserialize, Serialize};
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
#[derive(Debug, Serialize, Deserialize)]
struct ClientSession {
    homeserver: String,
    db_path: PathBuf,
    passphrase: String,
}

/// The full session persisted to `{state_dir}/session` as JSON. Layout is
/// byte-compatible with headjack's `FullSession`.
#[derive(Debug, Serialize, Deserialize)]
struct FullSession {
    client_session: ClientSession,
    user_session: MatrixSession,
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_token: Option<String>,
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
    /// the persisted credentials + sync token.
    pub async fn login(login: &Login, state_dir: Option<&str>, name: &str) -> anyhow::Result<Self> {
        let state_dir = match state_dir {
            Some(s) => PathBuf::from(expand_tilde(s)),
            None => dirs::state_dir()
                .expect("no state_dir directory found")
                .join(name),
        };
        let session_file = state_dir.join("session");

        let (client, sync_token) = if session_file.exists() {
            restore_session(&session_file).await?
        } else {
            (do_login(&session_file, login).await?, None)
        };

        Ok(Self {
            client,
            sync_token,
            session_file,
        })
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

/// Restore a client + sync token from a persisted session file.
async fn restore_session(session_file: &Path) -> anyhow::Result<(Client, Option<String>)> {
    info!(
        "Previous session found in '{}'",
        session_file.to_string_lossy()
    );
    let serialized = fs::read_to_string(session_file).await?;
    let FullSession {
        client_session,
        user_session,
        sync_token,
    } = serde_json::from_str(&serialized)?;

    let client = Client::builder()
        .homeserver_url(client_session.homeserver)
        .build()
        .await?;
    info!("Restoring session for {}…", &user_session.meta.user_id);
    client.restore_session(user_session).await?;
    Ok((client, sync_token))
}

/// Password-login a fresh device and persist the session.
async fn do_login(session_file: &Path, login: &Login) -> anyhow::Result<Client> {
    info!("No previous session found, logging in…");
    let client = Client::builder()
        .homeserver_url(&login.homeserver_url)
        .build()
        .await?;
    let matrix_auth = client.matrix_auth();

    let password = match &login.password {
        Some(p) => p.clone(),
        None => anyhow::bail!("password is required (interactive entry is not supported)"),
    };
    matrix_auth
        .login_username(&login.username, &password)
        .initial_device_display_name("chaz")
        .await?;
    info!("Logged in as {}", login.username);

    let user_session = matrix_auth
        .session()
        .expect("a logged-in client should have a session");
    let full = FullSession {
        client_session: ClientSession {
            homeserver: login.homeserver_url.clone(),
            db_path: PathBuf::new(),
            passphrase: String::new(),
        },
        user_session,
        sync_token: None,
    };
    if let Some(parent) = session_file.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(session_file, serde_json::to_string(&full)?).await?;
    info!("Session persisted in {}", session_file.to_string_lossy());
    Ok(client)
}

/// Rewrite the session file with the latest sync token.
async fn persist_sync_token(session_file: &Path, sync_token: String) -> anyhow::Result<()> {
    let serialized = fs::read_to_string(session_file).await?;
    let mut full: FullSession = serde_json::from_str(&serialized)?;
    full.sync_token = Some(sync_token);
    fs::write(session_file, serde_json::to_string(&full)?).await?;
    Ok(())
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
