use crate::tool::Tool;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoryEntry {
    key: String,
    value: String,
    timestamp: String,
}

/// Store a fact in persistent memory
pub struct Remember {
    memory_file: PathBuf,
}

impl Remember {
    pub fn new(state_dir: &std::path::Path) -> Self {
        Self {
            memory_file: state_dir.join("memory.jsonl"),
        }
    }
}

impl Tool for Remember {
    fn name(&self) -> &str {
        "remember"
    }

    fn description(&self) -> &str {
        "Store a fact in persistent memory. Use this to save important information that should be recalled later across conversations."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "A short descriptive label for this fact (e.g. 'user_name', 'project_deadline')"
                },
                "value": {
                    "type": "string",
                    "description": "The fact to remember"
                }
            },
            "required": ["key", "value"]
        })
    }

    fn execute(
        &self,
        arguments: Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            let key = arguments
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'key' argument".to_string())?;
            let value = arguments
                .get("value")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'value' argument".to_string())?;

            let entry = MemoryEntry {
                key: key.to_string(),
                value: value.to_string(),
                timestamp: Utc::now().to_rfc3339(),
            };

            let json =
                serde_json::to_string(&entry).map_err(|e| format!("Serialization error: {e}"))?;

            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.memory_file)
                .map_err(|e| format!("Failed to open memory file: {e}"))?;
            writeln!(file, "{json}").map_err(|e| format!("Failed to write memory: {e}"))?;

            Ok(format!("Remembered: {key} = {value}"))
        })
    }
}

/// Search persistent memory for facts
pub struct Recall {
    memory_file: PathBuf,
}

impl Recall {
    pub fn new(state_dir: &std::path::Path) -> Self {
        Self {
            memory_file: state_dir.join("memory.jsonl"),
        }
    }
}

impl Tool for Recall {
    fn name(&self) -> &str {
        "recall"
    }

    fn description(&self) -> &str {
        "Search persistent memory for previously stored facts. Returns all matching entries."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keyword to search for in memory keys and values"
                }
            },
            "required": ["query"]
        })
    }

    fn execute(
        &self,
        arguments: Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async move {
            let query = arguments
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'query' argument".to_string())?
                .to_lowercase();

            let content = std::fs::read_to_string(&self.memory_file).unwrap_or_default();
            if content.is_empty() {
                return Ok("No memories stored yet.".to_string());
            }

            let mut matches = Vec::new();
            // Walk backwards so most recent entries win for duplicate keys
            for line in content.lines().rev() {
                if let Ok(entry) = serde_json::from_str::<MemoryEntry>(line) {
                    if entry.key.to_lowercase().contains(&query)
                        || entry.value.to_lowercase().contains(&query)
                    {
                        // Deduplicate by key — keep only the most recent
                        if !matches.iter().any(|m: &MemoryEntry| m.key == entry.key) {
                            matches.push(entry);
                        }
                    }
                }
            }

            if matches.is_empty() {
                return Ok(format!("No memories found matching '{query}'."));
            }

            let result = matches
                .iter()
                .map(|m| format!("- **{}**: {} ({})", m.key, m.value, m.timestamp))
                .collect::<Vec<_>>()
                .join("\n");

            Ok(result)
        })
    }
}
