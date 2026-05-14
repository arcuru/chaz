//! Handlers for the built-in `/extensions` slash command.
//!
//! `/extensions` is a *framework* command — it controls which extensions
//! are active on the current session, and which session-scoped settings
//! they see. Implementing it as a built-in (rather than an extension
//! command) avoids a chicken-and-egg problem: an extension that
//! controlled `/extensions` could itself be removed, leaving the user
//! stuck with no way to add it back.
//!
//! Subcommands:
//!
//! - `/extensions` / `/extensions list` — list every extension on this peer
//!   with active/inactive status, [`crate::extension::ExtensionRef`]
//!   (so version drift is visible), and declared hook kinds.
//! - `/extensions add <name>` — activate `<name>` on this session
//!   (appends an `Activated` event and refreshes the cache).
//! - `/extensions remove <name>` — deactivate; survives restarts via
//!   the `record_active` reconciler's "respect Deactivated" rule.
//! - `/extensions settings <name>` — print the per-session settings
//!   JSON for `<name>`.
//! - `/extensions set <name> <key> <value>` — merge `key = value` into
//!   the per-session settings. `<value>` is JSON-parsed first
//!   (so `60`, `true`, `"abc"` all work); on parse failure it's stored
//!   as a plain string.

use super::{CommandContext, CommandOutcome};
use crate::extension::{ExtensionEvent, ExtensionHub, ExtensionRef, append_event, list_events};
use chrono::{DateTime, Utc};

/// Parsed `/extensions <action>` from the gateway parser.
#[derive(Debug)]
pub enum ExtensionsAction {
    List,
    Add(String),
    Remove(String),
    Settings(String),
    Set {
        name: String,
        key: String,
        value: String,
    },
}

pub async fn dispatch(action: ExtensionsAction, ctx: &CommandContext<'_>) -> CommandOutcome {
    match action {
        ExtensionsAction::List => list(ctx).await,
        ExtensionsAction::Add(name) => add(&name, ctx).await,
        ExtensionsAction::Remove(name) => remove(&name, ctx).await,
        ExtensionsAction::Settings(name) => settings(&name, ctx).await,
        ExtensionsAction::Set { name, key, value } => set(&name, &key, &value, ctx).await,
    }
}

async fn list(ctx: &CommandContext<'_>) -> CommandOutcome {
    let hub = ctx.server.extensions();
    let active = ctx.server.active_extensions_for(ctx.session_db_id).await;
    let names = hub.extension_names();
    if names.is_empty() {
        return CommandOutcome::Text("No extensions registered on this peer.".into());
    }
    let refs = hub.extension_refs();
    let ref_by_name: std::collections::HashMap<&str, &ExtensionRef> =
        refs.iter().map(|r| (r.name(), r)).collect();

    let mut lines = vec!["Extensions on this peer (✓ = active on this session):".to_string()];
    for name in &names {
        let marker = if active.contains(*name) { "✓" } else { " " };
        let version = ref_by_name
            .get(name)
            .map(|r| r.version().to_string())
            .unwrap_or_else(|| "?".into());
        let mut kinds: Vec<String> = hub
            .hooks_for(name)
            .into_iter()
            .map(|k| format!("{k:?}"))
            .collect();
        kinds.sort();
        let kinds_str = if kinds.is_empty() {
            "—".to_string()
        } else {
            kinds.join(", ")
        };
        lines.push(format!("  {marker} {name} [{version}] — {kinds_str}"));
    }
    CommandOutcome::Text(lines.join("\n"))
}

async fn add(name: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let hub = ctx.server.extensions();
    let Some(ext_ref) = find_ref(hub, name) else {
        return CommandOutcome::Error(format!(
            "Unknown extension '{name}'. Use `/extensions list` to see what's available."
        ));
    };
    let active = ctx.server.active_extensions_for(ctx.session_db_id).await;
    if active.contains(name) {
        return CommandOutcome::Text(format!("'{name}' is already active on this session."));
    }
    let timestamp = monotonic_timestamp_after(ctx.session_db).await;
    let event = ExtensionEvent::Activated {
        name: name.to_string(),
        extension_ref: ext_ref,
        timestamp,
    };
    if let Err(e) = append_event(ctx.session_db, event).await {
        return CommandOutcome::Error(format!("Failed to record activation: {e}"));
    }
    ctx.server
        .refresh_active_extensions(ctx.session_db_id)
        .await;
    CommandOutcome::Text(format!(
        "Activated '{name}' on this session. Hooks and tools take effect on the next agent turn."
    ))
}

async fn remove(name: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let hub = ctx.server.extensions();
    if !hub.extension_names().contains(&name) {
        return CommandOutcome::Error(format!(
            "Unknown extension '{name}'. Use `/extensions list` to see what's registered."
        ));
    }
    let active = ctx.server.active_extensions_for(ctx.session_db_id).await;
    if !active.contains(name) {
        return CommandOutcome::Text(format!("'{name}' is already inactive on this session."));
    }
    let timestamp = monotonic_timestamp_after(ctx.session_db).await;
    let event = ExtensionEvent::Deactivated {
        name: name.to_string(),
        timestamp,
    };
    if let Err(e) = append_event(ctx.session_db, event).await {
        return CommandOutcome::Error(format!("Failed to record deactivation: {e}"));
    }
    ctx.server
        .refresh_active_extensions(ctx.session_db_id)
        .await;
    CommandOutcome::Text(format!(
        "Deactivated '{name}' on this session. Hooks stop firing and tools disappear from the LLM tool list on the next agent turn."
    ))
}

async fn settings(name: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let hub = ctx.server.extensions();
    if !hub.extension_names().contains(&name) {
        return CommandOutcome::Error(format!("Unknown extension '{name}'."));
    }
    let stored = crate::extension::read_settings(ctx.session_db, name).await;
    let pretty = serde_json::to_string_pretty(&stored).unwrap_or_else(|_| stored.to_string());
    CommandOutcome::Text(format!("Settings for '{name}' on this session:\n{pretty}"))
}

async fn set(name: &str, key: &str, value: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let hub = ctx.server.extensions();
    if !hub.extension_names().contains(&name) {
        return CommandOutcome::Error(format!("Unknown extension '{name}'."));
    }
    // Try JSON-parse the value first so `60`, `true`, `"abc"`, `null`,
    // `[1,2]` all behave correctly. Fall back to storing the raw string
    // if it doesn't parse — covers the common `foo` literal case.
    let parsed_value: serde_json::Value = serde_json::from_str(value)
        .unwrap_or_else(|_| serde_json::Value::String(value.to_string()));

    let mut current = crate::extension::read_settings(ctx.session_db, name).await;
    if !current.is_object() {
        current = serde_json::json!({});
    }
    current
        .as_object_mut()
        .expect("forced object above")
        .insert(key.to_string(), parsed_value.clone());

    if let Err(e) = crate::extension::write_settings(ctx.session_db, name, current).await {
        return CommandOutcome::Error(format!("Failed to write settings: {e}"));
    }
    CommandOutcome::Text(format!("Set {name}.{key} = {parsed_value}"))
}

fn find_ref(hub: &ExtensionHub, name: &str) -> Option<ExtensionRef> {
    hub.extension_refs().into_iter().find(|r| r.name() == name)
}

/// Compute a timestamp guaranteed to be strictly after every event
/// already in the log — same monotonicity guard `record_active` uses,
/// so a concurrent peer's future-dated event can't make this write
/// "older" than the deactivation it's overwriting.
async fn monotonic_timestamp_after(session_db: &eidetica::Database) -> DateTime<Utc> {
    let events = list_events(session_db).await.unwrap_or_default();
    let max_seen = events
        .iter()
        .map(|e| e.timestamp())
        .max()
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    std::cmp::max(Utc::now(), max_seen + chrono::Duration::milliseconds(1))
}

// Lower-level event-log and settings round-trips already have unit
// tests in src/extension/mod.rs; the dispatch glue here is exercised
// via the gateway parsers + integration paths.
