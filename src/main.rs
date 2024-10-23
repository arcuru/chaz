mod aichat;
mod backends;
mod openai;
use backends::{BackendManager, ChatContext, Message};

mod role;
use openai_api_rs::v1::chat_completion::MessageRole;
use role::{get_role, RoleDetails};

mod defaults;
use defaults::DEFAULT_CONFIG;

use clap::Parser;
use headjack::Tags;
use headjack::*;
use lazy_static::lazy_static;
use matrix_sdk::{
    media::{MediaFormat, MediaRequest},
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

/// Configuration info for a backend
///
/// Holds the config info for an OpenAPI compatible backend
#[derive(Debug, Deserialize, Clone)]
struct Backend {
    /// The type of backend
    ///
    /// Currently only supports AIChat or OpenAICompatible
    #[serde(rename = "type")]
    backend_type: BackendType,
    /// The base URL for the API
    api_base: Option<String>,
    /// The API key to use for the API
    api_key: Option<String>,
    /// Available models for this backend
    models: Option<Vec<Model>>,
    /// Name of this backend
    ///
    /// Will be used by Chaz to name the model as "name:model_name"
    /// Will default to the backend_type, "aichat" or "openai"
    name: Option<String>,
    /// Set the config directory
    /// Used by the aichat backend
    #[allow(dead_code)]
    config_dir: Option<String>,
}

impl Backend {
    pub fn new(backend_type: BackendType) -> Self {
        Backend {
            backend_type,
            api_base: None,
            api_key: None,
            models: None,
            name: None,
            config_dir: None,
        }
    }

    /// Get the name for this bacckend
    pub fn get_name(&self) -> String {
        if let Some(name) = &self.name {
            name.clone()
        } else {
            match self.backend_type {
                BackendType::AIChat => "aichat".to_string(),
                BackendType::OpenAICompatible => "openai".to_string(),
            }
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
struct Model {
    /// The name of the model
    ///
    /// This is passed to the backend to select the model, e.g. "gpt-3.5-turbo"
    name: String,
    // TODO: add other params, e.g. https://github.com/sigoden/aichat/blob/main/models.yaml
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
enum BackendType {
    AIChat,
    OpenAICompatible,
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
    /// Model to use for summarizing chats
    /// Used for setting the room name/topic
    chat_summary_model: Option<String>,
    /// Default role
    role: Option<String>,
    /// Definitions of roles
    roles: Option<Vec<RoleDetails>>,
    /// Disable sending media context to aichat
    disable_media_context: Option<bool>,
    /// Backend configuration
    ///
    /// If set, this will be used instead of AiChat
    backends: Option<Vec<Backend>>,
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
                messages: vec![Message::new(MessageRole::user, input.to_string())],
                model: context.model,
                role: context.role,
                media: Vec::new(),
            };

            info!(
                "Request: {} - {}",
                sender.as_str(),
                input.replace('\n', " ")
            );
            if let Ok(result) = get_backend(&room).await.execute(&no_context).await {
                // Add the prefix ".response:\n" to the result
                // That way we can identify our own responses and ignore them for context
                info!(
                    "Response: {} - {}",
                    sender.as_str(),
                    result.replace('\n', " ")
                );
                let content = RoomMessageEventContent::notice_plain(result.clone());

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
        "backend",
        "<name> <api_base> <api_key>".to_string(),
        "Manually enter an OpenAI Compatible Backend".to_string(),
        set_backend,
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
            match get_backend(&room).await.execute(&context).await {
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
    let backends = get_backend(&room).await;
    let response = format!(
        "!chaz Current Model: {}\n\nKnown Backends:\n{}\n\nKnown Models:\n{}",
        context
            .model
            .unwrap_or(backends.default_model().unwrap_or("unknown".to_string())),
        backends.list_known_backends().join("\n"),
        backends.list_known_models().join("\n")
    );
    room.send(RoomMessageEventContent::notice_plain(response))
        .await
        .unwrap();
    Ok(())
}

/// Add a backend provider into the room tags
async fn set_backend(_: OwnedUserId, text: String, room: Room) -> Result<(), ()> {
    // Skip to the 3rd word in the command, we know the first two are "!chaz backend"
    let mut split = text.split_whitespace();
    split.next();
    split.next();
    if let (Some(name), Some(url), Some(token)) = (split.next(), split.next(), split.next()) {
        let mut tags = Tags::new(&room, "is.chaz.backend").await;
        // The Scheme is like so:
        // chazdefault=<name>
        // <name>.url=<url>
        // <name>.token=<token>
        // <other name>.url=<url>
        // <other name>.token=<token>
        //
        // TODO: Support "is.chaz.backend.<name>.model.<known models>"
        // That will make it so that Chaz can validate and list those models
        tags.replace_kv("chazdefault", name);
        tags.replace_kv(&format!("{}.url", name), url);
        tags.replace_kv(&format!("{}.token", name), token);
        tags.sync().await;
        room.send(RoomMessageEventContent::notice_plain(format!(
            "!chaz Successfully added backend {}",
            name
        )))
        .await
        .unwrap();
    } else {
        room.send(RoomMessageEventContent::notice_plain(
            "!chaz Error: invalid arguments. Usage: !chaz backend <name> <api_base> <api_key>",
        ))
        .await
        .unwrap();
        return Ok(());
    }
    Ok(())
}

/// Set the model to use for this chat
async fn model(sender: OwnedUserId, text: String, room: Room) -> Result<(), ()> {
    // Get the third word in the command, `!chaz model <model>`
    let model = text.split_whitespace().nth(2);
    if let Some(model) = model {
        let backend = get_backend(&room).await;
        if backend.is_known_model(model) {
            let response = format!("!chaz Model set to \"{}\"", model);
            room.send(RoomMessageEventContent::notice_plain(response))
                .await
                .unwrap();
        } else if let Err(e) = backend.validate_model(model) {
            let response = format!("!chaz Error: {}", e);
            room.send(RoomMessageEventContent::notice_plain(response))
                .await
                .unwrap();
        } else {
            let response = format!("!chaz Model {} is unknown, but may be valid. Please manually verify that it is supported by your desired backend.", model);
            room.send(RoomMessageEventContent::notice_plain(response))
                .await
                .unwrap();
        }
        let mut tags = Tags::new(&room, "is.chaz.model").await;
        tags.replace_kv("default", model);
        tags.sync().await;
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
        context.messages.push(Message::new(
            MessageRole::user,
            [
                "Summarize this conversation in less than 20 characters to use as the title of this conversation.",
                "The output should be a single line of text describing the conversation.",
                "Do not output anything except for the summary text.",
                "Only the first 20 characters will be used.",
                ].join(" ")));

        let response = get_backend(&room).await.execute(&context).await;
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
        context.messages.push(Message::new(
            MessageRole::user,
            [
                "Summarize this conversation in less than 50 characters.",
                "Do not output anything except for the summary text.",
                "Do not include any commentary or context, only the summary.",
            ]
            .join(" "),
        ));

        let response = get_backend(&room).await.execute(&context).await;
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

/// Get the backend defined in the room tags.
async fn get_tag_backend(room: &Room) -> Option<Vec<Backend>> {
    let mut backends = Vec::new();
    let tags = Tags::new(room, "is.chaz.backend").await;
    let default_backend = tags.get_value("chazdefault");
    for tag in tags.tags() {
        if tag.split('.').nth(1).is_some_and(|x| x.starts_with("url")) {
            let name = tag.split('.').next().unwrap_or_default();
            let mut backend = Backend::new(BackendType::OpenAICompatible);
            backend.name = Some(name.to_string());
            backend.api_base = tags.get_value(&format!("{}.url", name));
            backend.api_key = tags.get_value(&format!("{}.token", name));
            if backend.api_base.is_some() && backend.api_key.is_some() {
                backends.push(backend);
            }
        }
    }
    if let Some(default_backend) = default_backend {
        // Reorder so that the default backend is first
        if let Some(index) = backends
            .iter()
            .position(|x| x.name == Some(default_backend.to_string()))
        {
            backends.swap(0, index);
        }
    }
    Some(backends)
}

/// Returns the backend based on the global config
async fn get_backend(room: &Room) -> BackendManager {
    let mut backends = Vec::new();
    if let Some(tag_backends) = get_tag_backend(room).await {
        backends = tag_backends;
    }
    // Pull the tags in the current room, and add that backend
    let config = GLOBAL_CONFIG.lock().unwrap().clone().unwrap();
    if let Some(config_backends) = config.backends {
        backends.extend(config_backends);
    }
    if backends.is_empty() {
        BackendManager::new(&None)
    } else {
        BackendManager::new(&Some(backends))
    }
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
                            // This is being deprecated in favor of storing the models in the tags
                            if text_content.body.starts_with("!chaz model")
                                && context.model.is_none()
                            {
                                let model = text_content.body.split_whitespace().nth(2);
                                if let Some(model) = model {
                                    if get_backend(room).await.validate_model(model).is_ok() {
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
                                        MessageRole::assistant,
                                        command.to_string(),
                                    ));
                                } else {
                                    context
                                        .messages
                                        .push(Message::new(MessageRole::user, command.to_string()));
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
                                    MessageRole::assistant,
                                    text_content.body.clone(),
                                ));
                            } else {
                                context.messages.push(Message::new(
                                    MessageRole::user,
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
    // Get the model name from the tags if it exists
    // This is the new preferred method, so it just overwrites whatever we found above
    let tags = Tags::new(room, "is.chaz.model").await;
    if let Some(model) = tags.get_value("default") {
        context.model = Some(model);
    }
    // Reverse context so that it's in the correct order
    context.messages.reverse();
    context.media.reverse();
    Ok(context)
}
