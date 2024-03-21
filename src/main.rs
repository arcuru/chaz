use clap::Parser;
use lazy_static::lazy_static;
use matrix_sdk::{
    config::SyncSettings,
    room::MessagesOptions,
    ruma::events::room::{
        member::StrippedRoomMemberEvent,
        message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
    },
    Client, Room, RoomState,
};
use ollama_rs::{generation::completion::request::GenerationRequest, Ollama};
use regex::Regex;
use serde::Deserialize;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;
use std::{collections::HashMap, fs::File};
use tokio::time::{sleep, Duration};

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
    password: String,
    /// Allow list of which accounts we will respond to
    allow_list: Option<String>,
    ollama: Option<HashMap<String, OllamaConfig>>,
}

#[derive(Debug, Deserialize, Clone)]
struct OllamaConfig {
    model: String,
    endpoint: Option<EndpointConfig>,
}

#[derive(Debug, Deserialize, Clone)]
struct EndpointConfig {
    host: String,
    port: u16,
}

lazy_static! {
    static ref GLOBAL_CONFIG: Mutex<Option<Config>> = Mutex::new(None);
}

/// This is the starting point of the app. `main` is called by rust binaries to
/// run the program in this case, we use tokio (a reactor) to allow us to use
/// an `async` function run.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // set up some simple stderr logging. You can configure it by changing the env
    // var `RUST_LOG`
    tracing_subscriber::fmt::init();

    let args = HeadJackArgs::parse();

    let mut file = File::open(args.config)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;

    let config: Config = serde_yaml::from_str(&contents)?;
    *GLOBAL_CONFIG.lock().unwrap() = Some(config.clone());

    // our actual runner
    login_and_sync(config.homeserver_url, &config.username, &config.password).await?;
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

// The core sync loop we have running.
async fn login_and_sync(
    homeserver_url: String,
    username: &str,
    password: &str,
) -> anyhow::Result<()> {
    // First, we set up the client.

    // Note that when encryption is enabled, you should use a persistent store to be
    // able to restore the session with a working encryption setup.
    // See the `persist_session` example.
    let client = Client::builder()
        // We use the convenient client builder to set our custom homeserver URL on it.
        .homeserver_url(homeserver_url)
        .build()
        .await?;

    // Then let's log that client in
    client
        .matrix_auth()
        .login_username(username, password)
        .initial_device_display_name("headjack-bot")
        .await?;

    // It worked!
    println!("logged in as {username}");

    // Now, we want our client to react to invites. Invites sent us stripped member
    // state events so we want to react to them. We add the event handler before
    // the sync, so this happens also for older messages. All rooms we've
    // already entered won't have stripped states anymore and thus won't fire
    client.add_event_handler(on_stripped_state_member);

    // An initial sync to set up state and so our bot doesn't respond to old
    // messages. If the `StateStore` finds saved state in the location given the
    // initial sync will be skipped in favor of loading state from the store
    let sync_token = client
        .sync_once(SyncSettings::default())
        .await
        .unwrap()
        .next_batch;

    // now that we've synced, let's attach a handler for incoming room messages, so
    // we can react on it
    client.add_event_handler(on_room_message);

    // since we called `sync_once` before we entered our sync loop we must pass
    // that sync token to `sync`
    let settings = SyncSettings::default().token(sync_token);
    // this keeps state from the server streaming in to the bot via the
    // EventHandler trait
    client.sync(settings).await?; // this essentially loops until we kill the bot

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

    // The event handlers are called before the next sync begins, but
    // methods that change the state of a room (joining, leaving a room)
    // wait for the sync to return the new room state so we need to spawn
    // a new task for them.
    tokio::spawn(async move {
        println!("Autojoining room {}", room.room_id());
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
        println!("Successfully joined room {}", room.room_id());
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

    // If we start with a single '!', interpret as a command
    let text = text_content.body.trim_start();
    if is_command(text) {
        let command = text.split_whitespace().next();
        if let Some(command) = command {
            // Write a match statement to match the first word in the body
            match &command[1..] {
                "party" => {
                    let content =
                        RoomMessageEventContent::text_plain("!party\n🎉🎊🥳 let's PARTY!! 🥳🎊🎉");
                    // send our message to the room we found the "!party" command in
                    room.send(content).await.unwrap();
                }
                "ollama" => {
                    // Send just this 1 message to the ollama server
                    let input = text_content.body.trim_start_matches("!ollama").trim();

                    if let Ok(result) = send_to_ollama_server(input.to_string()).await {
                        // Add the prefix "!response:\n" to the result
                        // That way we can identify our own responses and ignore them for context
                        let result = format!("!response:\n{}", result);
                        let content = RoomMessageEventContent::text_plain(result);

                        room.send(content).await.unwrap();
                    }
                }
                "help" => {
                    let content = RoomMessageEventContent::text_plain(
                        "!help\n\nAvailable commands:\n- !party - Start a party!\n- !ollama <input> - Send <input> to the ollama server without context\n- !print - Print the full context of the conversation\n- !help - Print this message",
                    );
                    room.send(content).await.unwrap();
                }
                "print" => {
                    // Prints the full context back to the room
                    let mut context = get_context(&room).await.unwrap();
                    context.insert_str(0, "!context\n");
                    let content = RoomMessageEventContent::text_plain(context);
                    room.send(content).await.unwrap();
                }
                _ => {
                    println!("Unknown command");
                }
            }
        }
    } else {
        // If it's not a command, we should send the full context without commands to the ollama server
        if let Ok(mut context) = get_context(&room).await {
            let prefix = format!("Here is the full text of our ongoing conversation. Your name is {}, and your messages are prefixed by {}:. My name is {}, and my messages are prefixed by {}:. Send the next response in this conversation. Do not prefix your response with your name or any other text. Do not greet me again if you've already done so. Send only the text of your response.\n",
                        room.client().user_id().unwrap(), room.client().user_id().unwrap(), event.sender, event.sender);
            context.insert_str(0, &prefix);
            if let Ok(result) = send_to_ollama_server(context).await {
                let content = RoomMessageEventContent::text_plain(result);
                room.send(content).await.unwrap();
            }
        }
    }
}

fn is_command(text: &str) -> bool {
    text.starts_with('!') && !text.starts_with("!!")
}

/// Gets the context of the current conversation
async fn get_context(room: &Room) -> Result<String, ()> {
    // Read all the messages in the room, place them into a single string, and print them out
    let mut messages = Vec::new();

    let mut options = MessagesOptions::backward();

    // FIXME: I think this doesn't work because we aren't saving the session
    // It will only work for the messages in the current session

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
    Ok(messages.into_iter().rev().collect::<String>())
}

// Send the current conversation to the configured ollama server
async fn send_to_ollama_server(input: String) -> Result<String, ()> {
    let config = GLOBAL_CONFIG.lock().unwrap().clone().unwrap();
    if config.ollama.is_none() {
        return Err(());
    }
    let ollama = config.ollama.unwrap();
    if ollama.is_empty() {
        return Err(());
    }

    let server = ollama.values().next().unwrap();

    // Just pull the first thing we see
    let ollama_server = if let Some(endpoint) = &server.endpoint {
        Ollama::new(endpoint.host.clone(), endpoint.port)
    } else {
        Ollama::default()
    };

    let prompt = input;

    let res = ollama_server
        .generate(GenerationRequest::new(server.model.clone(), prompt))
        .await;

    if let Ok(res) = res {
        // Strip leading and trailing whitespace from res.response
        return Ok(res.response.trim().to_string());
    }
    Err(())
}
