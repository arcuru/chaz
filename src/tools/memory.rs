use crate::tool::{Tool, ToolContext, ToolDescriptor};
use chrono::Utc;
use eidetica::store::Table;
use eidetica::Database;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

/// Derive the eidetica store name for an agent's memory.
///
/// Each agent gets its own isolated memory namespace in the central DB.
fn memory_store_name(agent_name: &str) -> String {
    format!("memory:{agent_name}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub key: String,
    pub value: String,
    pub timestamp: String,
}

/// Store a fact in persistent memory (eidetica-backed)
pub struct Remember {
    database: Database,
}

impl Remember {
    pub fn new(database: Database) -> Self {
        Self { database }
    }
}

impl Tool for Remember {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "remember".to_string(),
            description: "Store a fact in persistent memory. Use this to save important information that should be recalled later across conversations.".to_string(),
            parameters: serde_json::json!({
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
            }),
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
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

            let store_name = memory_store_name(&ctx.agent_name);
            let txn = self
                .database
                .new_transaction()
                .await
                .map_err(|e| format!("Failed to create transaction: {e}"))?;
            let store = txn
                .get_store::<Table<MemoryEntry>>(&store_name)
                .await
                .map_err(|e| format!("Failed to open memory store: {e}"))?;
            store
                .insert(entry)
                .await
                .map_err(|e| format!("Failed to store memory: {e}"))?;
            txn.commit()
                .await
                .map_err(|e| format!("Failed to commit memory: {e}"))?;

            Ok(format!("Remembered: {key} = {value}"))
        })
    }
}

/// Search persistent memory for facts (eidetica-backed)
pub struct Recall {
    database: Database,
}

impl Recall {
    pub fn new(database: Database) -> Self {
        Self { database }
    }
}

impl Tool for Recall {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "recall".to_string(),
            description: "Search persistent memory for previously stored facts. Returns all matching entries.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keyword to search for in memory keys and values"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            let query = arguments
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing 'query' argument".to_string())?
                .to_lowercase();

            let store_name = memory_store_name(&ctx.agent_name);
            let txn = self
                .database
                .new_transaction()
                .await
                .map_err(|e| format!("Failed to create transaction: {e}"))?;
            let store = txn
                .get_store::<Table<MemoryEntry>>(&store_name)
                .await
                .map_err(|e| format!("Failed to open memory store: {e}"))?;

            let records = store
                .search(|entry: &MemoryEntry| {
                    entry.key.to_lowercase().contains(&query)
                        || entry.value.to_lowercase().contains(&query)
                })
                .await
                .map_err(|e| format!("Failed to search memory: {e}"))?;

            if records.is_empty() {
                return Ok(format!("No memories found matching '{query}'."));
            }

            // Deduplicate by key — keep only the most recent
            let mut by_key: std::collections::HashMap<String, MemoryEntry> =
                std::collections::HashMap::new();
            for (_, entry) in records {
                by_key
                    .entry(entry.key.clone())
                    .and_modify(|existing| {
                        if entry.timestamp > existing.timestamp {
                            *existing = entry.clone();
                        }
                    })
                    .or_insert(entry);
            }

            let result = by_key
                .values()
                .map(|m| format!("- **{}**: {} ({})", m.key, m.value, m.timestamp))
                .collect::<Vec<_>>()
                .join("\n");

            Ok(result)
        })
    }
}
