use std::process::Command;

pub struct AiChat {
    binary_location: String,
}

impl Default for AiChat {
    fn default() -> Self {
        AiChat::new("aichat".to_string())
    }
}

impl AiChat {
    pub fn new(binary_location: String) -> Self {
        AiChat { binary_location }
    }

    pub fn list_models(&self) -> Vec<String> {
        // Run the binary with the `list` argument
        let output = Command::new(&self.binary_location)
            .arg("--list-models")
            .output()
            .expect("Failed to execute command");

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
        command.arg("--").arg(prompt);
        eprintln!("Running command: {:?}", command);

        let output = command.output().expect("Failed to execute command");

        // return the output as a string
        String::from_utf8(output.stdout).map_err(|_| ())
    }
}
