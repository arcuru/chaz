mod aichat;
use aichat::AiChat;

mod role;
use role::RoleDetails;

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
    Room,
};
use regex::Regex;
use serde::Deserialize;
use std::{fs::File, io::Read, path::PathBuf, sync::Mutex};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct ChazArgs {
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
}

lazy_static! {
    static ref GLOBAL_CONFIG: Mutex<Option<Config>> = Mutex::new(None);
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
        login: Login {
            homeserver_url: config.homeserver_url,
            username: config.username.clone(),
            password: config.password,
        },
        name: Some(config.username.clone()),
        allow_list: config.allow_list,
        state_dir: config.state_dir,
    })
    .await;

    if let Err(e) = bot.login().await {
        eprintln!("Error logging in: {e}");
    }

    // React to invites.
    // We set this up before the initial sync so that we join rooms
    // even if they were invited before the bot was started.
    bot.join_rooms();

    // Syncs to the current state
    if let Err(e) = bot.sync().await {
        eprintln!("Error syncing: {e}");
    }

    eprintln!("The client is ready! Listening to new messages…");

    // The party command is from the matrix-rust-sdk examples
    // Keeping it as an easter egg
    bot.register_text_command("party", None, |_, _, room| async move {
        let content = RoomMessageEventContent::text_plain(".🎉🎊🥳 let's PARTY!! 🥳🎊🎉");
        room.send(content).await.unwrap();
        Ok(())
    })
    .await;

    bot.register_text_command(
        "print",
        "Print the conversation".to_string(),
        |_, _, room| async move {
            let (context, _, _) = get_context(&room).await.unwrap();
            let mut context = add_role(&context);
            context.insert_str(0, ".context:\n");
            let content = RoomMessageEventContent::text_plain(context);
            room.send(content).await.unwrap();
            Ok(())
        },
    )
    .await;

    bot.register_text_command(
        "send",
        "<message> - Send this message without context".to_string(),
        |_, text, room| async move {
            let input = text.trim_start_matches(".send").trim();

            // But we do need to read the context to figure out the model to use
            let (_, model, _) = get_context(&room).await.unwrap();

            if let Ok(result) = get_backend().execute(&model, input.to_string(), Vec::new()) {
                // Add the prefix ".response:\n" to the result
                // That way we can identify our own responses and ignore them for context
                let result = format!(".response:\n{}", result);
                let content = RoomMessageEventContent::text_plain(result);

                room.send(content).await.unwrap();
            }
            Ok(())
        },
    )
    .await;

    bot.register_text_command(
        "model",
        "<model> - Select the model to use".to_string(),
        model,
    )
    .await;

    bot.register_text_command("list", "List available models".to_string(), list_models)
        .await;

    bot.register_text_command(
        "clear",
        "Ignore all messages before this point".to_string(),
        |_, _, room| async move {
            room.send(RoomMessageEventContent::text_plain(
                ".clear: All messages before this will be ignored",
            ))
            .await
            .unwrap();
            Ok(())
        },
    )
    .await;

    bot.register_text_command(
        "rename",
        "Rename the room and set the topic based on the chat content".to_string(),
        rename,
    )
    .await;

    bot.register_text_handler(|_, _, room| async move {
        // If it's not a command, we should send the full context without commands to the server
        if let Ok((context, model, media)) = get_context(&room).await {
            let mut context = add_role(&context);
            // Append "ASSISTANT: " to the context string to indicate the assistant is speaking
            context.push_str("ASSISTANT: ");

            if let Ok(result) = get_backend().execute(&model, context, media) {
                let content = if result.is_empty() {
                    RoomMessageEventContent::text_plain(".error: No response")
                } else {
                    RoomMessageEventContent::text_plain(result)
                };
                room.send(content).await.unwrap();
            } else {
                room.send(RoomMessageEventContent::text_plain(
                    ".error: Failed to generate response",
                ))
                .await
                .unwrap();
            }
        }
        Ok(())
    });

    // Run the bot, this should never return except on error
    if let Err(e) = bot.run().await {
        eprintln!("Error running bot: {e}");
    }

    Ok(())
}

/// Prepend the role defined in the global config
fn add_role(context: &str) -> String {
    let config = GLOBAL_CONFIG.lock().unwrap().clone().unwrap();
    role::prepend_role(
        context.to_string(),
        config.role.clone(),
        config.roles.clone(),
    )
}

/// List the available models
async fn list_models(_: OwnedUserId, _: String, room: Room) -> Result<(), ()> {
    let (_, current_model, _) = get_context(&room).await.unwrap();
    let response = format!(
        ".models:\n\ncurrent: {}\n\nAvailable Models:\n{}",
        current_model.unwrap_or(get_backend().default_model()),
        get_backend().list_models().join("\n")
    );
    room.send(RoomMessageEventContent::text_plain(response))
        .await
        .unwrap();
    Ok(())
}

async fn model(sender: OwnedUserId, text: String, room: Room) -> Result<(), ()> {
    // Verify the command is fine
    // Get the second word in the command
    let model = text.split_whitespace().nth(1);
    if let Some(model) = model {
        let models = get_backend().list_models();
        if models.contains(&model.to_string()) {
            // Set the model
            let response = format!(".model: Set to \"{}\"", model);
            room.send(RoomMessageEventContent::text_plain(response))
                .await
                .unwrap();
        } else {
            let response = format!(
                ".error: Model \"{}\" not found.\n\nAvailable models:\n{}",
                model,
                models.join("\n")
            );
            room.send(RoomMessageEventContent::text_plain(response))
                .await
                .unwrap();
        }
    } else {
        list_models(sender, text, room).await?;
    }
    Ok(())
}

async fn rename(_: OwnedUserId, _: String, room: Room) -> Result<(), ()> {
    if let Ok((context, _, _)) = get_context(&room).await {
        let title_prompt= [
                            &context,
                            "\nUSER: Summarize this conversation in less than 20 characters to use as the title of this conversation. ",
                            "The output should be a single line of text describing the conversation. ",
                            "Do not output anything except for the summary text. ",
                            "Only the first 20 characters will be used. ",
                            "\nASSISTANT: ",
                        ].join("");
        let model = get_chat_summary_model();

        let response = get_backend().execute(&model, title_prompt, Vec::new());
        if let Ok(result) = response {
            eprintln!("Result: {}", result);
            let result = clean_summary_response(&result, None);
            if room.set_name(result).await.is_err() {
                room.send(RoomMessageEventContent::text_plain(
                    ".error: I don't have permission to rename the room",
                ))
                .await
                .unwrap();

                // If we can't set the name, we can't set the topic either
                return Ok(());
            }
        }

        let topic_prompt = [
            &context,
            "\nUSER: Summarize this conversation in less than 50 characters. ",
            "Do not output anything except for the summary text. ",
            "Do not include any commentary or context, only the summary. ",
            "\nASSISTANT: ",
        ]
        .join("");

        let response = get_backend().execute(&model, topic_prompt, Vec::new());
        if let Ok(result) = response {
            eprintln!("Result: {}", result);
            let result = clean_summary_response(&result, None);
            if room.set_room_topic(&result).await.is_err() {
                room.send(RoomMessageEventContent::text_plain(
                    ".error: I don't have permission to set the topic",
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

/// Gets the context of the current conversation
/// Returns a model if it was ever entered
async fn get_context(room: &Room) -> Result<(String, Option<String>, Vec<MediaFileHandle>), ()> {
    // Read all the messages in the room, place them into a single string, and print them out
    let mut messages = Vec::new();

    let mut options = MessagesOptions::backward();
    let mut model_response = None;
    let mut media = Vec::new();

    'outer: while let Ok(batch) = room.messages(options).await {
        // This assumes that the messages are in reverse order
        for message in batch.chunk {
            if let Ok(content) = message
                .event
                .get_field::<RoomMessageEventContent>("content")
            {
                let Ok(sender) = message.event.get_field::<String>("sender") else {
                    continue;
                };
                if let Some(content) = content {
                    if let MessageType::Image(image_content) = &content.msgtype {
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
                        media.insert(0, x);
                        continue;
                    }
                    let MessageType::Text(text_content) = content.msgtype else {
                        continue;
                    };
                    if is_command(&text_content.body) {
                        // if the message is a valid model command, set the model
                        if text_content.body.starts_with(".model") && model_response.is_none() {
                            let model = text_content.body.split_whitespace().nth(1);
                            if let Some(model) = model {
                                // Add the config_dir from the global config
                                let models = get_backend().list_models();
                                if models.contains(&model.to_string()) {
                                    model_response = Some(model.to_string());
                                }
                            }
                        }
                        // if the message was a clear command, we are finished
                        if text_content.body.starts_with(".clear") {
                            break 'outer;
                        }
                        // Ignore other commands
                        continue;
                    }
                    // Push the sender and message to the front of the string
                    let sender = sender.unwrap_or("".to_string());
                    if sender == room.client().user_id().unwrap().as_str() {
                        // If the sender is the bot, prefix the message with "ASSISTANT: "
                        messages.push(format!("ASSISTANT: {}\n", text_content.body));
                    } else {
                        // Otherwise, prefix the message with "USER: "
                        messages.push(format!("USER: {}\n", text_content.body));
                    }
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
        media,
    ))
}
