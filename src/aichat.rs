use std::process::Command;

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

    pub fn execute(&self, model: Option<String>, prompt: String) -> Result<String, ()> {
        let mut command = Command::new(&self.binary_location);
        if let Some(model) = model {
            command.arg("--model").arg(model);
        }
        if let Some(config_dir) = &self.config_dir {
            command.env("AICHAT_CONFIG_DIR", config_dir);
        }
        command.arg("--").arg(prompt);
        eprintln!("Running command: {:?}", command);

        let output = command.output().expect("Failed to execute command");

        // return the output as a string
        String::from_utf8(output.stdout).map_err(|_| ())
    }
}
