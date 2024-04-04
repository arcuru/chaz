use lazy_static::lazy_static;
use matrix_sdk::ruma::events::room::member::StrippedRoomMemberEvent;
use matrix_sdk::ruma::events::room::message::MessageType;
use matrix_sdk::ruma::events::room::message::OriginalSyncRoomMessageEvent;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::RoomState;
use matrix_sdk::{
    config::SyncSettings, matrix_auth::MatrixSession, ruma::api::client::filter::FilterDefinition,
    Client, Error, LoopCtrl, Room,
};
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs;
use tokio::sync::Mutex;
use tokio::time::sleep;

// The structure of the matrix rust sdk requires that any state that you need access to in the callbacks
// is 'static.
// This is a bit of a pain, so we need to use a global state to store the actual bot state for ease of use.

lazy_static! {
    ///  Stores the global state for all bots.
    /// The key is the user ID of the bot
    static ref GLOBAL_STATE: Mutex<HashMap<String, Mutex<State>>> = Mutex::new(HashMap::new());
}

/// The data needed to re-build a client.
#[derive(Debug, Serialize, Deserialize)]
struct ClientSession {
    /// The URL of the homeserver of the user.
    homeserver: String,

    /// The path of the database.
    db_path: PathBuf,

    /// The passphrase of the database.
    passphrase: String,
}

struct HelpText {
    /// The command string that triggers this command
    command: String,
    /// Single line of help text
    short: Option<String>,
}

struct State {
    /// Descriptions of the commands
    help: Vec<HelpText>,
}

/// The full session to persist.
/// It contains the data to re-build the client and the Matrix user session.
/// This will be synced to disk so that we can restore the session later.
#[derive(Debug, Serialize, Deserialize)]
struct FullSession {
    /// The data to re-build the client.
    client_session: ClientSession,

    /// The Matrix user session.
    user_session: MatrixSession,

    /// The latest sync token.
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Login {
    /// The homeserver URL to connect to
    pub homeserver_url: String,
    /// The username to login with
    pub username: String,
    /// Optionally specify the password, if not set it will be asked for on cmd line
    pub password: Option<String>,
}

/// The bot struct, holds all configuration needed for the bot
#[derive(Debug, Clone)]
pub struct BotConfig {
    /// Login info for matrix
    pub login: Login,
    /// Name to use for the bot
    /// Defaults to login.username
    pub name: Option<String>,
    /// Allow list of which accounts we will respond to
    pub allow_list: Option<String>,
    /// Set the state directory to use
    /// Defaults to $XDG_STATE_HOME/username
    pub state_dir: Option<String>,
}

/// A Matrix Bot
pub struct Bot {
    config: BotConfig,

    /// The current sync token
    sync_token: Option<String>,

    /// The matrix client
    client: Option<Client>,
}

impl Bot {
    pub async fn new(config: BotConfig) -> Self {
        let bot = Bot {
            config,
            sync_token: None,
            client: None,
        };
        // Initialize the global state for the bot if it doesn't exist
        let mut global_state = GLOBAL_STATE.lock().await;
        global_state
            .entry(bot.name())
            .or_insert_with(|| Mutex::new(State { help: Vec::new() }));
        bot
    }

    /// Get the path to the session file
    fn session_file(&self) -> PathBuf {
        self.state_dir().join("session")
    }

    /// Login to the matrix server
    /// Performs everything needed to login or relogin
    pub async fn login(&mut self) -> anyhow::Result<()> {
        let state_dir = self.state_dir();
        let session_file = self.session_file();

        let (client, sync_token) = if session_file.exists() {
            restore_session(&session_file).await?
        } else {
            (
                login(
                    &state_dir,
                    &session_file,
                    &self.config.login.homeserver_url,
                    &self.config.login.username,
                    &self.config.login.password,
                )
                .await?,
                None,
            )
        };

        self.sync_token = sync_token;
        self.client = Some(client);

        Ok(())
    }

    /// Sync to the current state of the homeserver
    pub async fn sync(&mut self) -> anyhow::Result<()> {
        let client = self.client.as_ref().expect("client not initialized");

        // Enable room members lazy-loading, it will speed up the initial sync a lot
        // with accounts in lots of rooms.
        // See <https://spec.matrix.org/v1.6/client-server-api/#lazy-loading-room-members>.
        let filter = FilterDefinition::with_lazy_loading();
        let mut sync_settings = SyncSettings::default().filter(filter.into());

        // If we've already synced through a certain point, we'll sync the latest.
        if let Some(sync_token) = &self.sync_token {
            sync_settings = sync_settings.token(sync_token);
        }

        loop {
            match client.sync_once(sync_settings.clone()).await {
                Ok(response) => {
                    self.sync_token = Some(response.next_batch.clone());
                    persist_sync_token(&self.session_file(), response.next_batch.clone()).await?;
                    break;
                }
                Err(error) => {
                    eprintln!("An error occurred during initial sync: {error}");
                    eprintln!("Trying again…");
                }
            }
        }
        Ok(())
    }

    /// Create the help command
    /// This adds a command that prints the help
    async fn register_help_command(&self) {
        let name = self.name();
        self.register_text_command(
            "help",
            "Show this message".to_string(),
            |_, room| async move {
                let global_state = GLOBAL_STATE.lock().await;
                let state = global_state.get(&name).unwrap();
                let state = state.lock().await;
                let help = &state.help;
                let mut response = String::from(".help\n\nAvailable commands:\n");

                for h in help {
                    if let Some(short) = &h.short {
                        response.push_str(&format!("- .{} - {}\n", h.command, short));
                    }
                }
                room.send(RoomMessageEventContent::text_plain(response))
                    .await
                    .map_err(|_| ())?;
                Ok(())
            },
        )
        .await;
    }

    /// Adds a callback to join rooms we've been invited to
    /// Ignores invites from anyone who is not on the allow_list
    pub fn join_rooms(&self) {
        let client = self.client.as_ref().expect("client not initialized");
        let allow_list = self.config.allow_list.clone();
        client.add_event_handler(
            |room_member: StrippedRoomMemberEvent, client: Client, room: Room| async move {
                if room_member.state_key != client.user_id().unwrap() {
                    // the invite we've seen isn't for us, but for someone else. ignore
                    return;
                }
                if !is_allowed(allow_list, room_member.sender.as_str()) {
                    // Sender is not on the allowlist
                    return;
                }
                eprintln!("Received stripped room member event: {:?}", room_member);

                // The event handlers are called before the next sync begins, but
                // methods that change the state of a room (joining, leaving a room)
                // wait for the sync to return the new room state so we need to spawn
                // a new task for them.
                tokio::spawn(async move {
                    eprintln!("Autojoining room {}", room.room_id());
                    let mut delay = 2;

                    while let Err(err) = room.join().await {
                        // retry autojoin due to synapse sending invites, before the
                        // invited user can join for more information see
                        // https://github.com/matrix-org/synapse/issues/4345
                        eprintln!(
                            "Failed to join room {} ({err:?}), retrying in {delay}s",
                            room.room_id()
                        );

                        sleep(Duration::from_secs(delay)).await;
                        delay *= 2;

                        if delay > 3600 {
                            eprintln!("Can't join room {} ({err:?})", room.room_id());
                            break;
                        }
                    }
                    eprintln!("Successfully joined room {}", room.room_id());
                });
            },
        );
    }

    /// Register a command that will be called for every non-command message
    /// Useful for bots that want to act more like chatbots, having some response to every message
    pub fn register_text_handler<F, Fut>(&self, callback: F)
    where
        F: FnOnce(String, Room) -> Fut + Send + 'static + Clone + Sync,
        Fut: std::future::Future<Output = Result<(), ()>> + Send + 'static,
    {
        let client = self.client.as_ref().expect("client not initialized");
        let allow_list = self.config.allow_list.clone();
        client.add_event_handler(
            move |event: OriginalSyncRoomMessageEvent, room: Room| async move {
                // Ignore messages from rooms we're not in
                if room.state() != RoomState::Joined {
                    return;
                }
                let MessageType::Text(text_content) = &event.content.msgtype else {
                    return;
                };
                if !is_allowed(allow_list, event.sender.as_str()) {
                    // Sender is not on the allowlist
                    return;
                }
                let body = text_content.body.trim_start();
                if is_command(body) {
                    return;
                }
                if let Err(e) = callback(body.to_string(), room).await {
                    eprintln!("Error responding to: {}\nError: {:?}", body, e);
                }
            },
        );
    }

    /// Register a text command
    /// This will call the callback when the command is received
    /// Sending no help text will make the command not show up in the help
    /// TODO: This adds a separate handler for every command, this can be made more efficient
    pub async fn register_text_command<F, Fut, OptString>(
        &self,
        command: &str,
        short_help: OptString,
        callback: F,
    ) where
        F: FnOnce(String, Room) -> Fut + Send + 'static + Clone + Sync,
        Fut: std::future::Future<Output = Result<(), ()>> + Send + 'static,
        OptString: Into<Option<String>>,
    {
        {
            // Add the command to the help list
            let mut global_state = GLOBAL_STATE.lock().await;
            let state = global_state.get_mut(&self.name()).unwrap();
            let mut state = state.lock().await;
            state.help.push(HelpText {
                command: command.to_string(),
                short: short_help.into(),
            });
        }
        let client = self.client.as_ref().expect("client not initialized");
        let allow_list = self.config.allow_list.clone();
        let command = command.to_owned();
        client.add_event_handler(
            move |event: OriginalSyncRoomMessageEvent, room: Room| async move {
                // Ignore messages from rooms we're not in
                if room.state() != RoomState::Joined {
                    return;
                }
                let MessageType::Text(text_content) = &event.content.msgtype else {
                    return;
                };
                if !is_allowed(allow_list, event.sender.as_str()) {
                    // Sender is not on the allowlist
                    return;
                }

                let body = text_content.body.trim_start();
                if !is_command(body) {
                    return;
                }
                let input_command = body.split_whitespace().next();
                if let Some(input_command) = input_command {
                    if input_command[1..] == command {
                        // Call the callback
                        if let Err(e) = callback(body.to_string(), room).await {
                            eprintln!("Error running command: {} - {:?}", command, e);
                        }
                    }
                }
            },
        );
    }

    /// Run the bot continuously
    /// This function takes ownership of the bot, we'll be moving data out of it for use in the function closures
    pub async fn run(&self) -> anyhow::Result<()> {
        self.register_help_command().await;
        let client = self.client.as_ref().expect("client not initialized");

        let filter = FilterDefinition::with_lazy_loading();
        let mut sync_settings = SyncSettings::default().filter(filter.into());

        // If we've already synced through a certain point, we'll sync the latest.
        if let Some(sync_token) = &self.sync_token {
            sync_settings = sync_settings.token(sync_token);
        }
        // This loops until we kill the program or an error happens.
        client
            .sync_with_result_callback(sync_settings, |sync_result| async move {
                let response = sync_result?;

                // We persist the token each time to be able to restore our session
                self.persist_sync_token(response.next_batch)
                    .await
                    .map_err(|err| Error::UnknownError(err.into()))?;

                Ok(LoopCtrl::Continue)
            })
            .await?;

        Ok(())
    }

    async fn persist_sync_token(&self, sync_token: String) -> anyhow::Result<()> {
        let serialized_session = fs::read_to_string(self.session_file().clone()).await?;
        let mut full_session: FullSession = serde_json::from_str(&serialized_session)?;

        full_session.sync_token = Some(sync_token);
        let serialized_session = serde_json::to_string(&full_session)?;
        fs::write(self.session_file().clone(), serialized_session).await?;

        Ok(())
    }

    /// Get the state directory for the bot
    pub fn state_dir(&self) -> PathBuf {
        if let Some(state_dir) = &self.config.state_dir {
            PathBuf::from(expand_tilde(state_dir))
        } else {
            dirs::state_dir()
                .expect("no state_dir directory found")
                .join(self.name())
        }
    }

    /// Get the name of the bot
    pub fn name(&self) -> String {
        self.config
            .name
            .clone()
            .unwrap_or_else(|| self.config.login.username.clone())
    }

    /// Get the client used by the bot
    pub fn client(&self) -> &Client {
        self.client.as_ref().expect("client not initialized")
    }
}

/// Verify if the sender is on the allow_list
fn is_allowed(allow_list: Option<String>, sender: &str) -> bool {
    // FIXME: Check to see if it's from ourselves, in which case we should do nothing
    if let Some(allow_list) = allow_list {
        let regex = Regex::new(&allow_list).expect("Invalid regular expression");
        return regex.is_match(sender);
    }
    false
}

/// Check if the message is a command
pub fn is_command(text: &str) -> bool {
    text.starts_with('.') && !text.starts_with("..")
}

/// Fixup the path if they've provided a ~
fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") {
        if let Some(home_dir) = dirs::home_dir() {
            let without_tilde = &path[1..]; // Remove the '~' and keep the rest of the path
            return home_dir.display().to_string() + without_tilde;
        }
    }
    path.to_string()
}

/// Restore a previous session.
async fn restore_session(session_file: &Path) -> anyhow::Result<(Client, Option<String>)> {
    eprintln!(
        "Previous session found in '{}'",
        session_file.to_string_lossy()
    );

    // The session was serialized as JSON in a file.
    let serialized_session = fs::read_to_string(session_file).await?;
    let FullSession {
        client_session,
        user_session,
        sync_token,
    } = serde_json::from_str(&serialized_session)?;

    // Build the client with the previous settings from the session.
    let client = Client::builder()
        .homeserver_url(client_session.homeserver)
        .sqlite_store(client_session.db_path, Some(&client_session.passphrase))
        .build()
        .await?;

    eprintln!("Restoring session for {}…", &user_session.meta.user_id);

    // Restore the Matrix user session.
    client.restore_session(user_session).await?;

    eprintln!("Done!");

    Ok((client, sync_token))
}

/// Login with a new device.
async fn login(
    state_dir: &Path,
    session_file: &Path,
    homeserver_url: &str,
    username: &str,
    password: &Option<String>,
) -> anyhow::Result<Client> {
    eprintln!("No previous session found, logging in…");

    let (client, client_session) = build_client(state_dir, homeserver_url.to_owned()).await?;
    let matrix_auth = client.matrix_auth();

    // If there's no password, ask for it
    let password = match password {
        Some(password) => password.clone(),
        None => {
            print!("Password: ");
            io::stdout().flush().expect("Unable to write to stdout");
            let mut password = String::new();
            io::stdin()
                .read_line(&mut password)
                .expect("Unable to read user input");
            password.trim().to_owned()
        }
    };

    match matrix_auth
        .login_username(username, &password)
        .initial_device_display_name("headjack client")
        .await
    {
        Ok(_) => {
            eprintln!("Logged in as {username}");
        }
        Err(error) => {
            eprintln!("Error logging in: {error}");
            return Err(error.into());
        }
    }

    // Persist the session to reuse it later.
    let user_session = matrix_auth
        .session()
        .expect("A logged-in client should have a session");
    let serialized_session = serde_json::to_string(&FullSession {
        client_session,
        user_session,
        sync_token: None,
    })?;
    fs::write(session_file, serialized_session).await?;

    eprintln!("Session persisted in {}", session_file.to_string_lossy());

    Ok(client)
}

/// Build a new client.
async fn build_client(
    state_dir: &Path,
    homeserver: String,
) -> anyhow::Result<(Client, ClientSession)> {
    let mut rng = thread_rng();

    // Place the db into a subfolder, just in case multiple clients are running
    let db_subfolder: String = (&mut rng)
        .sample_iter(Alphanumeric)
        .take(7)
        .map(char::from)
        .collect();
    let db_path = state_dir.join(db_subfolder);

    // Generate a random passphrase.
    // It will be saved in the session file and used to encrypt the database.
    let passphrase: String = (&mut rng)
        .sample_iter(Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();

    match Client::builder()
        .homeserver_url(&homeserver)
        // We use the SQLite store, which is enabled by default. This is the crucial part to
        // persist the encryption setup.
        // Note that other store backends are available and you can even implement your own.
        .sqlite_store(&db_path, Some(&passphrase))
        .build()
        .await
    {
        Ok(client) => Ok((
            client,
            ClientSession {
                homeserver,
                db_path,
                passphrase,
            },
        )),
        Err(error) => Err(error.into()),
    }
}

/// Write the sync_token to the session file
async fn persist_sync_token(session_file: &Path, sync_token: String) -> anyhow::Result<()> {
    let serialized_session = fs::read_to_string(session_file).await?;
    let mut full_session: FullSession = serde_json::from_str(&serialized_session)?;

    full_session.sync_token = Some(sync_token);
    let serialized_session = serde_json::to_string(&full_session)?;
    fs::write(session_file, serialized_session).await?;

    Ok(())
}
