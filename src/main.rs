mod aichat;
use aichat::AiChat;

mod role;
use role::{get_role, prepend_role, RoleDetails};

mod defaults;
use defaults::DEFAULT_CONFIG;

use clap::Parser;
use headjack::*;
use lazy_static::lazy_static;
use matrix_sdk::{
    media::{MediaFileHandle, MediaFormat, MediaRequest},
    room::MessagesOptions,
    ruma::{
        events::room::message::{MessageType, RoomMessageEventContent},
        OwnedUserId,
    },
    Room, RoomMemberships,
};
use regex::Regex;
use serde::Deserialize;
use std::format;
use std::{collections::HashMap, fs::File, io::Read, path::PathBuf, sync::Mutex};
use tracing::{error, info};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct ChazArgs {
    /// path to config file
    #[arg(short, long)]
    config: PathBuf,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    homeserver_url: String,
    username: String,
    /// Optionally specify the password, if not set it will be asked for on cmd line
    password: Option<String>,
    /// Allow list of which accounts we will respond to
    allow_list: Option<String>,
    /// Per-account message limit while the bot is running
    message_limit: Option<u64>,
    /// Room size limit to respond to
    room_size_limit: Option<usize>,
    /// Set the state directory for chaz
    /// Defaults to $XDG_STATE_HOME/chaz
    state_dir: Option<String>,
    /// Set the config directory for aichat
    /// Allows for multiple instances setups of aichat
    aichat_config_dir: Option<String>,
    /// Model to use for summarizing chats
    /// Used for setting the room name/topic
    chat_summary_model: Option<String>,
    /// Default role
    role: Option<String>,
    /// Definitions of roles
    roles: Option<Vec<RoleDetails>>,
    /// Disable sending media context to aichat
    disable_media_context: Option<bool>,
}

lazy_static! {
    /// Holds the config for the bot
    static ref GLOBAL_CONFIG: Mutex<Option<Config>> = Mutex::new(None);

    /// Count of the global messages per user
    static ref GLOBAL_MESSAGES: Mutex<HashMap<String, u64>> = Mutex::new(HashMap::new());
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // Read in the config file
    let args = ChazArgs::parse();
    let mut file = File::open(args.config)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;

    let config: Config = serde_yaml::from_str(&contents)?;
    *GLOBAL_CONFIG.lock().unwrap() = Some(config.clone());

    // The config file is read, now we can start the bot
    let mut bot = Bot::new(BotConfig {
        command_prefix: None,
        room_size_limit: config.room_size_limit,
        login: Login {
            homeserver_url: config.homeserver_url,
            username: config.username.clone(),
            password: config.password,
        },
        name: Some("chaz".to_string()),
        allow_list: config.allow_list,
        state_dir: config.state_dir,
    })
    .await;

    if let Err(e) = bot.login().await {
        error!("Error logging in: {e}");
    }

    // React to invites.
    // We set this up before the initial sync so that we join rooms
    // even if they were invited before the bot was started.
    bot.join_rooms();

    // Syncs to the current state
    if let Err(e) = bot.sync().await {
        info!("Error syncing: {e}");
    }

    info!("The client is ready! Listening to new messages…");

    // The party command is from the matrix-rust-sdk examples
    // Keeping it as an easter egg
    // TODO: Remove `party` from the help text
    bot.register_text_command(
        "party",
        "".to_string(),
        "Party!".to_string(),
        |_, _, room| async move {
            let content = RoomMessageEventContent::notice_plain(".🎉🎊🥳 let's PARTY!! 🥳🎊🎉");
            room.send(content).await.unwrap();
            Ok(())
        },
    )
    .await;

    bot.register_text_command(
        "print",
        None,
        Some("Print the conversation".to_string()),
        |_, _, room| async move {
            let context = get_context(&room).await.unwrap();
            let content = RoomMessageEventContent::notice_plain(context.string_prompt());
            room.send(content).await.unwrap();
            Ok(())
        },
    )
    .await;

    bot.register_text_command(
        "send",
        "<message>".to_string(),
        "Send a message without context".to_string(),
        |sender, text, room| async move {
            if rate_limit(&room, &sender).await {
                return Ok(());
            }
            // Skip over the command, which is "!chaz send"
            let input = text
                .split_whitespace()
                .skip(2)
                .collect::<Vec<&str>>()
                .join(" ");

            // But we do need to read the context to figure out the model to use
            let context = get_context(&room).await.unwrap();
            let no_context = ChatContext {
                messages: vec![Message {
                    sender: "USER".to_string(),
                    content: input.to_string(),
                }],
                model: context.model,
                role: context.role,
                media: Vec::new(),
            };

            info!(
                "Request: {} - {}",
                sender.as_str(),
                input.replace('\n', " ")
            );
            if let Ok(result) = get_backend().execute(&no_context) {
                // Add the prefix ".response:\n" to the result
                // That way we can identify our own responses and ignore them for context
                info!(
                    "Response: {} - {}",
                    sender.as_str(),
                    result.replace('\n', " ")
                );
                let content = RoomMessageEventContent::notice_plain(result);

                room.send(content).await.unwrap();
            }
            Ok(())
        },
    )
    .await;

    bot.register_text_command(
        "model",
        "<model>".to_string(),
        "Select the model to use".to_string(),
        model,
    )
    .await;

    bot.register_text_command(
        "list",
        "".to_string(),
        "List available models".to_string(),
        list_models,
    )
    .await;

    bot.register_text_command(
        "clear",
        "".to_string(),
        "Ignore all messages before this point".to_string(),
        |_, _, room| async move {
            room.send(RoomMessageEventContent::notice_plain(
                "!chaz clear: All messages before this will be ignored",
            ))
            .await
            .unwrap();
            Ok(())
        },
    )
    .await;

    bot.register_text_command(
        "rename",
        "".to_string(),
        "Rename the room and set the topic based on the chat content".to_string(),
        rename,
    )
    .await;

    // The text handler is called for every non-command message
    // It is also called if _only_ `!chaz` is sent. That sounds like a feature to me.
    bot.register_text_handler(|sender, body: String, room, event| async move {
        // If this room is not marked as a direct message, ignore messages
        // Direct message detection/conversion may be buggy? Recognize a direct message by either the room setting _or_ number of members
        let is_direct = room.is_direct().await.unwrap_or(false) || room.joined_members_count() < 3;

        // If the message is not a command, check if it mentions the bot
        let mentions_bot = event
            .content
            .mentions
            .as_ref()
            .map(|mentions| {
                mentions
                    .user_ids
                    .iter()
                    .any(|mention| mention == room.client().user_id().unwrap())
            })
            .unwrap_or(false);

        if !(is_direct || body.starts_with("!chaz") || mentions_bot) {
            return Ok(());
        }

        if rate_limit(&room, &sender).await {
            return Ok(());
        }
        // If it's not a command, we should send the full context without commands to the server
        if let Ok(context) = get_context(&room).await {
            match get_backend().execute(&context) {
                Ok(stdout) => {
                    info!("Response: {}", stdout.replace('\n', " "));
                    // Most LLMs like responding with Markdown
                    room.send(RoomMessageEventContent::text_markdown(stdout))
                        .await
                        .unwrap();
                }
                Err(stderr) => {
                    let err = format!("!chaz Error: {}", stderr.replace('\n', " "));
                    error!(err);
                    room.send(RoomMessageEventContent::notice_plain(err))
                        .await
                        .unwrap();
                }
            }
        }
        Ok(())
    });

    // Run the bot, this should never return except on error
    if let Err(e) = bot.run().await {
        error!("Error running bot: {e}");
    }

    Ok(())
}

/// Rate limit the user to a set number of messages
/// Returns true if the user is being rate limited
async fn rate_limit(room: &Room, sender: &OwnedUserId) -> bool {
    let room_size = room
        .members(RoomMemberships::ACTIVE)
        .await
        .unwrap_or(Vec::new())
        .len();
    let message_limit = GLOBAL_CONFIG
        .lock()
        .unwrap()
        .clone()
        .unwrap()
        .message_limit
        .unwrap_or(u64::MAX);
    let room_size_limit = GLOBAL_CONFIG
        .lock()
        .unwrap()
        .clone()
        .unwrap()
        .room_size_limit
        .unwrap_or(usize::MAX);
    let count = {
        let mut messages = GLOBAL_MESSAGES.lock().unwrap();
        let count = match messages.get_mut(sender.as_str()) {
            Some(count) => count,
            None => {
                // Insert the user with a val of 0 and return a mutable reference to the value
                messages.insert(sender.as_str().to_string(), 0);
                messages.get_mut(sender.as_str()).unwrap()
            }
        };
        // If the room is too big we will silently ignore the message
        // This is to prevent the bot from spamming large rooms
        if room_size > room_size_limit {
            return true;
        }
        if *count < message_limit {
            *count += 1;
            return false;
        }
        *count
    };
    error!("User {} has sent {} messages", sender, count);
    room.send(RoomMessageEventContent::notice_plain(format!(
        "!chaz Error: you have used up your message limit of {} messages.",
        message_limit
    )))
    .await
    .unwrap();
    true
}

/// List the available models
async fn list_models(_: OwnedUserId, _: String, room: Room) -> Result<(), ()> {
    let context = get_context(&room).await.unwrap();
    let response = format!(
        "!chaz Current Model: {}\n\nAvailable Models:\n{}",
        context.model.unwrap_or(get_backend().default_model()),
        get_backend().list_models().join("\n")
    );
    room.send(RoomMessageEventContent::notice_plain(response))
        .await
        .unwrap();
    Ok(())
}

async fn model(sender: OwnedUserId, text: String, room: Room) -> Result<(), ()> {
    // Verify the command is fine
    // Get the third word in the command, `!chaz model <model>`
    let model = text.split_whitespace().nth(2);
    if let Some(model) = model {
        let models = get_backend().list_models();
        if models.contains(&model.to_string()) {
            // Set the model
            let response = format!("!chaz Model set to \"{}\"", model);
            room.send(RoomMessageEventContent::notice_plain(response))
                .await
                .unwrap();
        } else {
            let response = format!(
                "!chaz Error: Model \"{}\" not found.\n\nAvailable models:\n{}",
                model,
                models.join("\n")
            );
            room.send(RoomMessageEventContent::notice_plain(response))
                .await
                .unwrap();
        }
    } else {
        list_models(sender, text, room).await?;
    }
    Ok(())
}

async fn rename(sender: OwnedUserId, _: String, room: Room) -> Result<(), ()> {
    if rate_limit(&room, &sender).await {
        return Ok(());
    }
    if let Ok(context) = get_context(&room).await {
        let mut context = context;
        context.model = get_chat_summary_model();
        context.messages.push(Message {
            sender: "USER".to_string(),
            content: [
                "Summarize this conversation in less than 20 characters to use as the title of this conversation.",
                "The output should be a single line of text describing the conversation.",
                "Do not output anything except for the summary text.",
                "Only the first 20 characters will be used.",
                ].join(" "),
        });

        let response = get_backend().execute(&context);
        if let Ok(result) = response {
            info!(
                "Response: {} - {}",
                sender.as_str(),
                result.replace('\n', " ")
            );
            let result = clean_summary_response(&result, None);
            if room.set_name(result).await.is_err() {
                room.send(RoomMessageEventContent::notice_plain(
                    "!chaz Error: I don't have permission to rename the room",
                ))
                .await
                .unwrap();

                // If we can't set the name, we can't set the topic either
                return Ok(());
            }
        }
        // Remove the title summary request
        context.messages.pop();

        context.model = get_chat_summary_model();
        context.messages.push(Message {
            sender: "USER".to_string(),
            content: [
                "Summarize this conversation in less than 50 characters.",
                "Do not output anything except for the summary text.",
                "Do not include any commentary or context, only the summary.",
            ]
            .join(" "),
        });

        let response = get_backend().execute(&context);
        if let Ok(result) = response {
            info!(
                "Response: {} - {}",
                sender.as_str(),
                result.replace('\n', " ")
            );
            let result = clean_summary_response(&result, None);
            if room.set_room_topic(&result).await.is_err() {
                room.send(RoomMessageEventContent::notice_plain(
                    "!chaz Error: I don't have permission to set the topic",
                ))
                .await
                .unwrap();
            }
        }
    }
    Ok(())
}

/// Returns the backend based on the global config
fn get_backend() -> AiChat {
    let config = GLOBAL_CONFIG.lock().unwrap().clone().unwrap();
    AiChat::new("aichat".to_string(), config.aichat_config_dir.clone())
}

/// Try to clean up the response from the model containing a summary
/// Sometimes the models will return extra info, so we want to clean it if possible
fn clean_summary_response(response: &str, max_length: Option<usize>) -> String {
    let response = {
        // Try to clean the response
        // Should look for the first quoted string
        let re = Regex::new(r#""([^"]*)""#).unwrap();
        // If there are any matches, return the first one
        if let Some(caps) = re.captures(response) {
            caps.get(1).map_or("", |m| m.as_str())
        } else {
            response
        }
    };
    if let Some(max_length) = max_length {
        return response.chars().take(max_length).collect::<String>();
    }
    response.to_string()
}

/// Get the chat summary model from the global config
fn get_chat_summary_model() -> Option<String> {
    let config = GLOBAL_CONFIG.lock().unwrap().clone().unwrap();
    config.chat_summary_model
}

struct Message {
    sender: String,
    content: String,
}

impl Message {
    fn new<S: Into<String>>(sender: S, content: S) -> Message {
        Message {
            sender: sender.into(),
            content: content.into(),
        }
    }
}

struct ChatContext {
    messages: Vec<Message>,
    model: Option<String>,
    media: Vec<MediaFileHandle>,
    role: Option<RoleDetails>,
}

impl ChatContext {
    /// Convert messages into a single string.
    fn string_prompt(&self) -> String {
        // TODO: consider making this markdown
        let mut prompt = String::new();
        for message in self.messages.iter() {
            prompt.push_str(&format!("{}: {}\n", message.sender, message.content));
        }
        // Indicate that the assistant needs to speak next
        prompt.push_str("ASSISTANT: ");
        prompt
    }

    /// Convert messages into a single string with the role prepended
    fn string_prompt_with_role(&self) -> String {
        let prompt = self.string_prompt();
        if let Some(role) = &self.role {
            prepend_role(prompt, role)
        } else {
            prompt
        }
    }
}

/// Gets the context of the current conversation
///
/// The token_limit is the maximum number of tokens to add into the context.
/// If no token_limit is given, the context will include the full room
async fn get_context(room: &Room) -> Result<ChatContext, ()> {
    let mut context = ChatContext {
        messages: Vec::new(),
        model: None,
        media: Vec::new(),
        role: None,
    };
    {
        let config = GLOBAL_CONFIG.lock().unwrap().clone().unwrap();
        context.role = get_role(
            config.role.clone(),
            config.roles.clone(),
            DEFAULT_CONFIG.roles.clone(),
        );
    }

    let mut options = MessagesOptions::backward();

    let config = GLOBAL_CONFIG.lock().unwrap().clone().unwrap();
    let enable_media_context = !config.disable_media_context.unwrap_or(false);

    'outer: while let Ok(batch) = room.messages(options).await {
        // This assumes that the messages are in reverse order, which they should be
        for message in batch.chunk {
            if let Some((sender, content)) = message
                .event
                .get_field::<String>("sender")
                .unwrap_or(None)
                .zip(
                    message
                        .event
                        .get_field::<RoomMessageEventContent>("content")
                        .unwrap_or(None),
                )
            {
                match &content.msgtype {
                    MessageType::Image(image_content) => {
                        if enable_media_context {
                            let request = MediaRequest {
                                source: image_content.source.clone(),
                                format: MediaFormat::File,
                            };
                            let mime = image_content
                                .info
                                .as_ref()
                                .unwrap()
                                .mimetype
                                .clone()
                                .unwrap()
                                .parse()
                                .unwrap();
                            let x = room
                                .client()
                                .media()
                                .get_media_file(&request, None, &mime, true, None)
                                .await
                                .unwrap();
                            context.media.push(x);
                        }
                    }
                    MessageType::Text(text_content) => {
                        // Commands are always prefixed with a !, regardless of the name
                        if is_command("!", &text_content.body) {
                            // if the message is a valid model command, set the model
                            // FIXME: hardcoded name
                            if text_content.body.starts_with("!chaz model")
                                && context.model.is_none()
                            {
                                let model = text_content.body.split_whitespace().nth(2);
                                if let Some(model) = model {
                                    // Add the config_dir from the global config
                                    let models = get_backend().list_models();
                                    if models.contains(&model.to_string()) {
                                        context.model = Some(model.to_string());
                                    }
                                }
                            }
                            // if the message was a clear command, we are finished
                            if text_content.body.starts_with("!chaz clear") {
                                break 'outer;
                            }
                            // if it's not a recognized command, remove the "!chaz" and add that to messages
                            if text_content.body.starts_with("!chaz") {
                                let command = text_content.body.trim_start_matches("!chaz").trim();
                                if command.is_empty() {
                                    continue;
                                }
                                if let Some(command) = command.split_whitespace().next() {
                                    // Recognized command, so skip adding it
                                    if [
                                        "help", "party", "send", "list", "rename", "print",
                                        "model", "clear",
                                    ]
                                    .contains(&command.to_lowercase().as_str())
                                    {
                                        continue;
                                    }
                                }
                                if room
                                    .client()
                                    .user_id()
                                    .is_some_and(|uid| sender == uid.as_str())
                                {
                                    context.messages.push(Message::new(
                                        "ASSISTANT".to_string(),
                                        command.to_string(),
                                    ));
                                } else {
                                    context.messages.push(Message::new(
                                        "USER".to_string(),
                                        command.to_string(),
                                    ));
                                }
                            }
                        } else {
                            // Push the sender and message to the front of the string
                            if room
                                .client()
                                .user_id()
                                .is_some_and(|uid| sender == uid.as_str())
                            {
                                // Sender is the bot
                                context.messages.push(Message::new(
                                    "ASSISTANT".to_string(),
                                    text_content.body.clone(),
                                ));
                            } else {
                                context.messages.push(Message::new(
                                    "USER".to_string(),
                                    text_content.body.clone(),
                                ));
                            }
                        }
                    }
                    _ => {}
                };
            }
        }
        if let Some(token) = batch.end {
            options = MessagesOptions::backward().from(Some(token.as_str()));
        } else {
            break;
        }
    }
    // Reverse context so that it's in the correct order
    context.messages.reverse();
    context.media.reverse();
    Ok(context)
}
