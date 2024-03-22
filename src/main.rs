mod aichat;
use aichat::AiChat;

use clap::Parser;
use lazy_static::lazy_static;
use matrix_sdk::{
    config::SyncSettings,
    matrix_auth::MatrixSession,
    room::MessagesOptions,
    ruma::{
        api::client::filter::FilterDefinition,
        events::room::{
            member::StrippedRoomMemberEvent,
            message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
        },
    },
    Client, Error, LoopCtrl, Room, RoomState,
};
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};
use tokio::{
    fs,
    time::{sleep, Duration},
};

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

/// The full session to persist.
#[derive(Debug, Serialize, Deserialize)]
struct FullSession {
    /// The data to re-build the client.
    client_session: ClientSession,

    /// The Matrix user session.
    user_session: MatrixSession,

    /// The latest sync token.
    ///
    /// It is only needed to persist it when using `Client::sync_once()` and we
    /// want to make our syncs faster by not receiving all the initial sync
    /// again.
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_token: Option<String>,
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct HeadJackArgs {
    /// path to config file
    #[arg(short, long)]
    config: PathBuf,
}

#[derive(Debug, Deserialize, Clone)]
struct Config {
    homeserver_url: String,
    username: String,
    /// Optionally specify the password, if not set it will be asked for on cmd line
    password: Option<String>,
    /// Allow list of which accounts we will respond to
    allow_list: Option<String>,
}

lazy_static! {
    static ref GLOBAL_CONFIG: Mutex<Option<Config>> = Mutex::new(None);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // set up some simple stderr logging. You can configure it by changing the env
    // var `RUST_LOG`
    tracing_subscriber::fmt::init();

    // Read in the config file
    let args = HeadJackArgs::parse();
    let mut file = File::open(args.config)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;

    let config: Config = serde_yaml::from_str(&contents)?;
    *GLOBAL_CONFIG.lock().unwrap() = Some(config.clone());

    // The folder containing the persisted data.
    let data_dir = dirs::data_dir()
        .expect("no data_dir directory found")
        .join("headjack");
    // The file where the session is persisted.
    let session_file = data_dir.join("session");

    let (client, sync_token) = if session_file.exists() {
        restore_session(&session_file).await?
    } else {
        (
            login(
                &data_dir,
                &session_file,
                config.homeserver_url,
                &config.username,
                &config.password,
            )
            .await?,
            None,
        )
    };

    sync(client, sync_token, &session_file)
        .await
        .expect("Error syncing with the server");
    Ok(())
}

/// Verify if the sender is on the allow_list
fn is_allowed(sender: &str) -> bool {
    let config = GLOBAL_CONFIG.lock().unwrap().clone().unwrap();
    // FIXME: Check to see if it's from ourselves, in which case we should do nothing
    if let Some(allow_list) = config.allow_list {
        let regex = Regex::new(&allow_list).expect("Invalid regular expression");
        return regex.is_match(sender);
    }
    false
}

/// Login with a new device.
async fn login(
    data_dir: &Path,
    session_file: &Path,
    homeserver_url: String,
    username: &str,
    password: &Option<String>,
) -> anyhow::Result<Client> {
    eprintln!("No previous session found, logging in…");

    let (client, client_session) = build_client(data_dir, homeserver_url).await?;
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
    data_dir: &Path,
    homeserver: String,
) -> anyhow::Result<(Client, ClientSession)> {
    let mut rng = thread_rng();

    // Place the db into a subfolder, just in case multiple clients are running
    let db_subfolder: String = (&mut rng)
        .sample_iter(Alphanumeric)
        .take(7)
        .map(char::from)
        .collect();
    let db_path = data_dir.join(db_subfolder);

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

/// Setup the client to listen to new messages.
async fn sync(
    client: Client,
    _initial_sync_token: Option<String>,
    session_file: &Path,
) -> anyhow::Result<()> {
    // Enable room members lazy-loading, it will speed up the initial sync a lot
    // with accounts in lots of rooms.
    // See <https://spec.matrix.org/v1.6/client-server-api/#lazy-loading-room-members>.
    let filter = FilterDefinition::with_lazy_loading();

    let mut sync_settings = SyncSettings::default().filter(filter.into());

    // This setting syncs it _to_ the provided token.
    // We would use this to respond to events that happened while we were offline.
    // if let Some(sync_token) = initial_sync_token {
    //     sync_settings = sync_settings.token(sync_token);
    // }

    // React to invites.
    // We set this up before the initial sync_once so that we join rooms
    // even if they were invited before the bot was started.
    client.add_event_handler(on_stripped_state_member);

    loop {
        match client.sync_once(sync_settings.clone()).await {
            Ok(response) => {
                // This is the last time we need to provide this token, the sync method after
                // will handle it on its own.
                sync_settings = sync_settings.token(response.next_batch.clone());
                persist_sync_token(session_file, response.next_batch).await?;
                break;
            }
            Err(error) => {
                eprintln!("An error occurred during initial sync: {error}");
                eprintln!("Trying again…");
            }
        }
    }

    eprintln!("The client is ready! Listening to new messages…");

    // Now that we've synced, let's attach a handler for incoming room messages.
    client.add_event_handler(on_room_message);

    // This loops until we kill the program or an error happens.
    client
        .sync_with_result_callback(sync_settings, |sync_result| async move {
            let response = sync_result?;

            // We persist the token each time to be able to restore our session
            persist_sync_token(session_file, response.next_batch)
                .await
                .map_err(|err| Error::UnknownError(err.into()))?;

            Ok(LoopCtrl::Continue)
        })
        .await?;

    Ok(())
}

/// Persist the sync token for a future session.
/// Note that this is needed only when using `sync_once`. Other sync methods get
/// the sync token from the store.
/// Note that the sync token is currently never actually used. It allows you to sync
/// to where we left off and respond to missed messages, but we don't want to do that yet.
async fn persist_sync_token(session_file: &Path, sync_token: String) -> anyhow::Result<()> {
    let serialized_session = fs::read_to_string(session_file).await?;
    let mut full_session: FullSession = serde_json::from_str(&serialized_session)?;

    full_session.sync_token = Some(sync_token);
    let serialized_session = serde_json::to_string(&full_session)?;
    fs::write(session_file, serialized_session).await?;

    Ok(())
}

// Whenever we see a new stripped room member event, we've asked our client to
// call this function. So what exactly are we doing then?
async fn on_stripped_state_member(
    room_member: StrippedRoomMemberEvent,
    client: Client,
    room: Room,
) {
    if room_member.state_key != client.user_id().unwrap() {
        // the invite we've seen isn't for us, but for someone else. ignore
        return;
    }
    if !is_allowed(room_member.sender.as_str()) {
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
}

// This fn is called whenever we see a new room message event. You notice that
// the difference between this and the other function that we've given to the
// handler lies only in their input parameters. However, that is enough for the
// rust-sdk to figure out which one to call and only do so, when the parameters
// are available.
async fn on_room_message(event: OriginalSyncRoomMessageEvent, room: Room) {
    // First, we need to unpack the message: We only want messages from rooms we are
    // still in and that are regular text messages - ignoring everything else.
    if room.state() != RoomState::Joined {
        return;
    }
    let MessageType::Text(text_content) = event.content.msgtype else {
        return;
    };
    if !is_allowed(event.sender.as_str()) {
        // Sender is not on the allowlist
        return;
    }

    // If we start with a single '.', interpret as a command
    let text = text_content.body.trim_start();
    eprintln!("Received message: {}", text);
    if is_command(text) {
        let command = text.split_whitespace().next();
        if let Some(command) = command {
            // Write a match statement to match the first word in the body
            match &command[1..] {
                "party" => {
                    let content =
                        RoomMessageEventContent::text_plain("🎉🎊🥳 let's PARTY!! 🥳🎊🎉");
                    // send our message to the room we found the "!party" command in
                    room.send(content).await.unwrap();
                }
                "send" => {
                    // Send just this message with no context
                    let input = text_content.body.trim_start_matches(".send").trim();

                    // But we need to read the context to figure out the model to use
                    let (_, model) = get_context(&room).await.unwrap();

                    if let Ok(result) = AiChat::default().execute(model, input.to_string()) {
                        // Add the prefix ".response:\n" to the result
                        // That way we can identify our own responses and ignore them for context
                        let result = format!(".response:\n{}", result);
                        let content = RoomMessageEventContent::text_plain(result);

                        room.send(content).await.unwrap();
                    }
                }
                "help" => {
                    let content = RoomMessageEventContent::text_plain(
                        [
                            ".help",
                            "",
                            "Available commands:",
                            "- .party - Start a party!",
                            "- .send <message> - Send this message without context",
                            "- .print - Print the full context of the conversation",
                            "- .help - Print this message",
                            "- .list - List available models",
                            "- .model <model> - Select a model to use",
                        ]
                        .join("\n"),
                    );
                    room.send(content).await.unwrap();
                }
                "print" => {
                    // Prints the full context back to the room
                    let (mut context, _) = get_context(&room).await.unwrap();
                    context.insert_str(0, ".context\n");
                    let content = RoomMessageEventContent::text_plain(context);
                    room.send(content).await.unwrap();
                }
                "model" => {
                    // Verify the command is fine
                    // Get the second word in the command
                    let model = text.split_whitespace().nth(1);
                    if let Some(model) = model {
                        // Verify this model is available
                        let models = AiChat::new("aichat".to_string()).list_models();
                        if models.contains(&model.to_string()) {
                            // Set the model
                            let response = format!(".model set to {}", model);
                            room.send(RoomMessageEventContent::text_plain(response))
                                .await
                                .unwrap();
                        } else {
                            let response = format!(
                                ".model {} not found. Available models:\n\n{}",
                                model,
                                models.join("\n")
                            );
                            room.send(RoomMessageEventContent::text_plain(response))
                                .await
                                .unwrap();
                        }
                    } else {
                        room.send(RoomMessageEventContent::text_plain(
                            ".error - must choose a model",
                        ))
                        .await
                        .unwrap();
                    }
                }
                "list" => {
                    let response = format!(
                        ".models available:\n\n{}",
                        AiChat::new("aichat".to_string()).list_models().join("\n")
                    );
                    room.send(RoomMessageEventContent::text_plain(response))
                        .await
                        .unwrap();
                }
                _ => {
                    eprintln!(".error - Unknown command");
                }
            }
        }
    } else {
        eprintln!("Received message: {}", text_content.body);
        // If it's not a command, we should send the full context without commands to the ollama server
        if let Ok((mut context, model)) = get_context(&room).await {
            let prefix = format!("Here is the full text of our ongoing conversation. Your name is {}, and your messages are prefixed by {}:. My name is {}, and my messages are prefixed by {}:. Send the next response in this conversation. Do not prefix your response with your name or any other text. Do not greet me again if you've already done so. Send only the text of your response.\n",
                        room.client().user_id().unwrap(), room.client().user_id().unwrap(), event.sender, event.sender);
            context.insert_str(0, &prefix);
            if let Ok(result) = AiChat::default().execute(model, context) {
                let content = RoomMessageEventContent::text_plain(result);
                room.send(content).await.unwrap();
            }
        }
    }
}

fn is_command(text: &str) -> bool {
    text.starts_with('.') && !text.starts_with("..")
}

/// Gets the context of the current conversation
/// Returns a model if it was ever entered
async fn get_context(room: &Room) -> Result<(String, Option<String>), ()> {
    // Read all the messages in the room, place them into a single string, and print them out
    let mut messages = Vec::new();

    let mut options = MessagesOptions::backward();
    let mut model_response = None;

    while let Ok(batch) = room.messages(options).await {
        for message in batch.chunk {
            if let Ok(content) = message
                .event
                .get_field::<RoomMessageEventContent>("content")
            {
                let Ok(sender) = message.event.get_field::<String>("sender") else {
                    continue;
                };
                if let Some(content) = content {
                    let MessageType::Text(text_content) = content.msgtype else {
                        continue;
                    };
                    if is_command(&text_content.body) {
                        // if the message is a valid model command, set the model
                        if text_content.body.starts_with(".model") {
                            let model = text_content.body.split_whitespace().nth(1);
                            if let Some(model) = model {
                                let models = AiChat::new("aichat".to_string()).list_models();
                                if models.contains(&model.to_string()) {
                                    model_response = Some(model.to_string());
                                }
                            }
                        }
                        continue;
                    }
                    // Push the sender and message to the front of the string

                    messages.push(format!("{}: {}\n", sender.unwrap(), text_content.body));
                }
            }
        }
        if let Some(token) = batch.end {
            options = MessagesOptions::backward().from(Some(token.as_str()));
        } else {
            break;
        }
    }
    // Append the messages into a string with newlines in between, in reverse order
    Ok((
        messages.into_iter().rev().collect::<String>(),
        model_response,
    ))
}
