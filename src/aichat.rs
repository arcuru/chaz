use std::process::Command;
use tracing::info;

use crate::ChatContext;

pub struct AiChat {
    binary_location: String,
    config_dir: Option<String>,
}

impl Default for AiChat {
    fn default() -> Self {
        AiChat::new("aichat".to_string(), None)
    }
}

impl AiChat {
    pub fn new(binary_location: String, config_dir: Option<String>) -> Self {
        AiChat {
            binary_location,
            config_dir,
        }
    }

    /// List the models available to the aichat binary
    pub fn list_models(&self) -> Vec<String> {
        let mut command = Command::new(self.binary_location.clone());
        command.arg("--list-models");

        // Add the config dir if it exists
        if let Some(config_dir) = &self.config_dir {
            command.env("AICHAT_CONFIG_DIR", config_dir);
        }

        let output = command.output().expect("Failed to execute command");

        // split each line of the output into it's own string and return
        output
            .stdout
            .split(|c| *c == b'\n')
            .map(|s| String::from_utf8(s.to_vec()).unwrap())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Get the default model for the current aichat config
    pub fn default_model(&self) -> String {
        let mut command = Command::new(self.binary_location.clone());
        command.arg("--info");

        // Add the config dir if it exists
        if let Some(config_dir) = &self.config_dir {
            command.env("AICHAT_CONFIG_DIR", config_dir);
        }

        let output = command.output().expect("Failed to execute command");

        // The model is returned on it's own line beginning with "model"
        // so we can split the output by newlines and find the line that starts with "model"
        // Then we can split that line by whitespace and take the second element
        output
            .stdout
            .split(|c| *c == b'\n')
            .map(|s| String::from_utf8(s.to_vec()).unwrap())
            .find(|s| s.starts_with("model"))
            .map(|s| s.split_whitespace().nth(1).unwrap().to_string())
            .unwrap_or("default".to_string())
    }

    pub fn execute(&self, context: &ChatContext) -> Result<String, String> {
        let mut command = Command::new(&self.binary_location);
        if let Some(model) = &context.model {
            command.arg("--model").arg(model);
        }
        if let Some(config_dir) = &self.config_dir {
            command.env("AICHAT_CONFIG_DIR", config_dir);
        }
        // For each media file, add the media flag and the path to the file
        // Note that we must not consume the media files, the handles need to persist until the command is finished
        if !context.media.is_empty() {
            command.arg("--file");
            for media_file in &context.media {
                command.arg(media_file.path());
            }
        }
        // Adds the full prompt as just a string
        command.arg("--").arg(context.string_prompt_with_role());
        info!("Running command: {:?}", command);

        let output = command.output().expect("Failed to execute command");

        info!("Output: {:?}", output);

        // return the output as a string
        if output.stdout.is_empty() {
            // if stdout is empty, something is clearly wrong and we actually have an error
            let stderr =
                String::from_utf8(output.stderr).map_err(|_| "Error decoding stderr".to_string());
            if let Ok(err) = stderr {
                Result::Err(err)
            } else {
                stderr
            }
        } else {
            String::from_utf8(output.stdout).map_err(|_| "Error decoding stdout".to_string())
        }
    }
}
