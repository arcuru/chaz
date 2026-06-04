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
//! - `/extensions add <name> [agent]` — activate `<name>`. Default
//!   scope is the session (appends an `Activated` event, refreshes the
//!   cache). With a trailing `agent`, clears the responding agent's
//!   opt-out on its Living Agent DB instead.
//! - `/extensions remove <name> [agent]` — deactivate. Session scope
//!   survives restarts via the `record_active` reconciler's "respect
//!   Deactivated" rule. Agent scope records an opt-out that only
//!   narrows the session set for that one agent and travels with it.
//! - `/extensions settings <name>` — print the per-session settings
//!   JSON for `<name>`.
//! - `/extensions set <name> <key> <value>` — merge `key = value` into
//!   the per-session settings. `<value>` is JSON-parsed first
//!   (so `60`, `true`, `"abc"` all work); on parse failure it's stored
//!   as a plain string.

use super::{CommandContext, CommandOutcome};
use crate::extension::{ExtensionEvent, ExtensionHub, ExtensionRef, append_event, list_events};
use chrono::{DateTime, Utc};

/// Which activation log an `add`/`remove` targets.
///
/// - `Session` (default) — the session's `extensions` log; affects
///   every agent responding in the session.
/// - `Agent` — the responding agent's Living Agent DB log. The agent
///   can only *narrow* the session set (an opt-out), and its records
///   travel with the agent when it syncs to other peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtScope {
    Session,
    Agent,
}

/// Parsed `/extensions <action>` from the gateway parser.
#[derive(Debug)]
pub enum ExtensionsAction {
    List,
    Add(String, ExtScope),
    Remove(String, ExtScope),
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
        ExtensionsAction::Add(name, scope) => add(&name, scope, ctx).await,
        ExtensionsAction::Remove(name, scope) => remove(&name, scope, ctx).await,
        ExtensionsAction::Settings(name) => settings(&name, ctx).await,
        ExtensionsAction::Set { name, key, value } => set(&name, &key, &value, ctx).await,
    }
}

async fn list(ctx: &CommandContext<'_>) -> CommandOutcome {
    let hub = ctx.server.extensions();
    let active = ctx.server.active_extensions_for(ctx.session_db_id).await;
    let agent = ctx.current_agent;
    let agent_disabled = ctx.server.agent_disabled_extensions(agent).await;
    let names = hub.extension_names();
    if names.is_empty() {
        return CommandOutcome::Text("No extensions registered on this peer.".into());
    }
    let refs = hub.extension_refs();
    let ref_by_name: std::collections::HashMap<&str, &ExtensionRef> =
        refs.iter().map(|r| (r.name(), r)).collect();

    let mut lines = vec![format!(
        "Extensions on this peer (✓ = live for agent '{agent}' this session; ✗ = disabled for this agent):"
    )];
    for name in &names {
        let session_on = active.contains(*name);
        let agent_off = agent_disabled.contains(*name);
        let marker = if agent_off {
            "✗"
        } else if session_on {
            "✓"
        } else {
            " "
        };
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
        let note = if agent_off && session_on {
            "  (session: on, agent: off)"
        } else {
            ""
        };
        lines.push(format!("  {marker} {name} [{version}] — {kinds_str}{note}"));
    }
    CommandOutcome::Text(lines.join("\n"))
}

async fn add(name: &str, scope: ExtScope, ctx: &CommandContext<'_>) -> CommandOutcome {
    let hub = ctx.server.extensions();
    let Some(ext_ref) = find_ref(hub, name) else {
        return CommandOutcome::Error(format!(
            "Unknown extension '{name}'. Use `/extensions list` to see what's available."
        ));
    };

    if scope == ExtScope::Agent {
        return add_agent(name, ext_ref, ctx).await;
    }

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

/// `add … agent` clears the agent's opt-out (writes an `Activated`
/// into the agent DB's sparse log). It can only undo a prior
/// `remove … agent` — the session set is the upper bound.
async fn add_agent(name: &str, ext_ref: ExtensionRef, ctx: &CommandContext<'_>) -> CommandOutcome {
    let agent = ctx.current_agent;
    let Some(adb) = ctx.server.open_agent_db_by_name(agent).await else {
        return CommandOutcome::Error(format!(
            "Agent '{agent}' isn't hosted on this peer, so its extension set can't be edited here."
        ));
    };
    let disabled = ctx.server.agent_disabled_extensions(agent).await;
    if !disabled.contains(name) {
        return CommandOutcome::Text(format!(
            "'{name}' is already enabled for agent '{agent}' (agents allow everything the session allows unless explicitly removed)."
        ));
    }
    let timestamp = monotonic_timestamp_after(adb.database()).await;
    let event = ExtensionEvent::Activated {
        name: name.to_string(),
        extension_ref: ext_ref,
        timestamp,
    };
    if let Err(e) = append_event(adb.database(), event).await {
        return CommandOutcome::Error(format!("Failed to record agent activation: {e}"));
    }
    CommandOutcome::Text(format!(
        "Re-enabled '{name}' for agent '{agent}'. Takes effect on the agent's next turn (still subject to the session's active set)."
    ))
}

async fn remove(name: &str, scope: ExtScope, ctx: &CommandContext<'_>) -> CommandOutcome {
    let hub = ctx.server.extensions();
    if !hub.extension_names().contains(&name) {
        return CommandOutcome::Error(format!(
            "Unknown extension '{name}'. Use `/extensions list` to see what's registered."
        ));
    }

    if scope == ExtScope::Agent {
        return remove_agent(name, ctx).await;
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

/// `remove … agent` records an opt-out on the agent DB. The extension
/// stays available to other agents in the session; this agent just
/// stops seeing it.
async fn remove_agent(name: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let agent = ctx.current_agent;
    let Some(adb) = ctx.server.open_agent_db_by_name(agent).await else {
        return CommandOutcome::Error(format!(
            "Agent '{agent}' isn't hosted on this peer, so its extension set can't be edited here."
        ));
    };
    let disabled = ctx.server.agent_disabled_extensions(agent).await;
    if disabled.contains(name) {
        return CommandOutcome::Text(format!("'{name}' is already disabled for agent '{agent}'."));
    }
    let timestamp = monotonic_timestamp_after(adb.database()).await;
    let event = ExtensionEvent::Deactivated {
        name: name.to_string(),
        timestamp,
    };
    if let Err(e) = append_event(adb.database(), event).await {
        return CommandOutcome::Error(format!("Failed to record agent deactivation: {e}"));
    }
    CommandOutcome::Text(format!(
        "Disabled '{name}' for agent '{agent}'. Other agents in this session keep it. Takes effect on the agent's next turn."
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

/// Split an `add`/`remove` argument into `(name, scope)`. A trailing
/// `agent` or `session` word selects the scope; otherwise the whole
/// string is the name and the scope defaults to `Session`. Shared by
/// the Matrix and TUI parsers.
pub fn split_ext_scope(rest: &str) -> (String, ExtScope) {
    let rest = rest.trim();
    if let Some((name, last)) = rest.rsplit_once(char::is_whitespace) {
        match last.trim() {
            "agent" => return (name.trim().to_string(), ExtScope::Agent),
            "session" => return (name.trim().to_string(), ExtScope::Session),
            _ => {}
        }
    }
    (rest.to_string(), ExtScope::Session)
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

#[cfg(test)]
mod tests {
    //! Integration tests for the `/extensions` command + per-session
    //! filtering. Each test builds a real `Arc<Server>` with the full
    //! built-in extension set registered so the assertions exercise the
    //! same code path the TUI/Matrix gateways take. No LLM backend is
    //! involved — tests don't run agent turns, they verify state
    //! observable through `Server::active_extensions_for`,
    //! `dispatch_extension`, and `ScopedTools::definitions`.

    use super::*;
    use crate::agent::AgentRegistry;
    use crate::backends::BackendManager;
    use crate::commands::{Command, dispatch};
    use crate::extension::ExtensionHub;
    use crate::extensions::{BuiltinDeps, all_builtins};
    use crate::hosted_index::HostedIndex;
    use crate::security::{LeakDetector, LeakPolicy, SecretStore, SecurityContext};
    use crate::server::Server;
    use crate::session::SessionRegistry;
    use crate::tool::{ScopedTools, ToolPolicyRegistry, ToolProfile, ToolRegistry};
    use crate::tool_host::NativeToolHost;
    use eidetica::backend::database::InMemory;
    use eidetica::{Instance, NewUser};
    use std::sync::{Arc, OnceLock};

    /// Full-fat fixture: Server with every built-in extension registered,
    /// a fresh session, and the dependencies needed to drive
    /// `commands::dispatch`. `_instance` and `_registry` are kept alive
    /// by the caller for the duration of the test.
    struct Fixture {
        _instance: Instance,
        _registry: Arc<SessionRegistry>,
        server: Arc<Server>,
        secrets: SecretStore,
        backend: BackendManager,
        tool_registry: Arc<ToolRegistry>,
        session_db_id: String,
        session_db: eidetica::Database,
    }

    async fn fixture() -> Fixture {
        let backend = InMemory::new();
        let (instance, user) =
            Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
                .await
                .unwrap();
        let agents = Arc::new(AgentRegistry::with_default_agent());
        let registry = Arc::new(
            SessionRegistry::new(instance.clone(), user, agents.clone())
                .await
                .unwrap(),
        );
        let chaz_peer = registry.chaz_peer().clone();
        let agent_index = HostedIndex::empty("agent");
        let bank_index = HostedIndex::empty("bank");

        // Host a "chaz" agent so per-agent `/extensions … agent` paths
        // have a real Living Agent DB to write opt-outs into.
        {
            use crate::agent_db::{AgentDbConfig, AgentMeta, create_agent_db};
            use crate::hosted_index::DbEntry;
            let mut user = registry.user_for_tests().await;
            let (agent_db, pubkey) = create_agent_db(
                &mut user,
                "chaz",
                &AgentDbConfig::default(),
                &AgentMeta {
                    display_name: Some("chaz".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            agent_index.register(DbEntry {
                db_id: agent_db.id(),
                display_name: "chaz".into(),
                pubkey,
            });
        }
        let policies = Arc::new(ToolPolicyRegistry::empty());
        let security = SecurityContext {
            leak_detector: LeakDetector::new(LeakPolicy::default()),
            auto_approved_tools: Default::default(),
            approval_callback: None,
        };

        // Install every built-in extension on the hub via the cap-based
        // install path, then drain the hub's tool list into a
        // ToolRegistry (mirroring main.rs).
        let mut hub = ExtensionHub::new();
        hub.reserve_builtin_commands(crate::commands::BUILTIN_COMMAND_NAMES.iter().copied());
        hub.set_session_registry(registry.clone());
        hub.set_hosted_index(agent_index.clone());
        let skill_bank_index = crate::hosted_index::HostedIndex::empty("skill_bank");
        let spawn_cell = Arc::new(OnceLock::new());
        // Wire peer_handles so migrated extensions instantiate during
        // `install_all` — without this the global drain path is skipped
        // and tool-only / hook-only extensions silently register nothing.
        hub.set_peer_handles(Arc::new(crate::extension::PeerHandles {
            registry: registry.clone(),
            agent_index: agent_index.clone(),
            memory_bank_index: bank_index.clone(),
            skill_bank_index: skill_bank_index.clone(),
            embedder: None,
            secrets: None,
            server_cell: spawn_cell.clone(),
            mcp_registry: Arc::new(crate::mcp::McpRegistry::new()),
            agent_state_allowlist: Default::default(),
        }));
        let secrets = SecretStore::new(chaz_peer).await;
        let backend_mgr = BackendManager::new(&None, secrets.clone());
        hub.install_all(all_builtins(BuiltinDeps {
            agent_index: agent_index.clone(),
            memory_bank_index: crate::hosted_index::HostedIndex::empty("bank"),
            skill_bank_index: skill_bank_index.clone(),
            session_registry: registry.clone(),
            embedder: None,
            web_search_backends: Vec::new(),
            spawn_server_cell: spawn_cell.clone(),
            backend_manager: backend_mgr.clone(),
            security: security.clone(),
        }))
        .await
        .unwrap();
        let mut tool_registry = ToolRegistry::new();
        for (owner, _name, tool) in hub.tools_for_registry() {
            tool_registry.register_arc_owned(tool, Some(owner));
        }
        let tool_registry = Arc::new(tool_registry);
        let hub = Arc::new(hub);

        let server = Server::new(
            registry.clone(),
            agents,
            agent_index,
            bank_index,
            crate::hosted_index::HostedIndex::empty("skill_bank"),
            tool_registry.clone(),
            policies,
            security,
            Default::default(),
            Default::default(),
            Arc::new(NativeToolHost::new()),
            hub,
            backend_mgr.clone(),
            Arc::new(crate::mcp::McpRegistry::new()),
        );
        let _ = spawn_cell.set(server.clone());

        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session_db_id = session_db.root_id().to_string();

        // Seed the session the way `Server::fire_session_start_hook`
        // would in production — record_active writes default-include
        // Activated events, then we refresh the cache. Skipping this
        // would leave the active set empty (no log to fold from), and
        // every test assertion against "what's active by default"
        // would fail in a misleading way.
        server
            .extensions()
            .record_active(&session_db)
            .await
            .unwrap();
        server.refresh_active_extensions(&session_db_id).await;

        Fixture {
            _instance: instance,
            _registry: registry,
            server,
            secrets,
            backend: backend_mgr,
            tool_registry,
            session_db_id,
            session_db,
        }
    }

    fn ctx<'a>(f: &'a Fixture) -> CommandContext<'a> {
        CommandContext {
            server: &f.server,
            secrets: &f.secrets,
            backend: &f.backend,
            session_db_id: &f.session_db_id,
            session_db: &f.session_db,
            current_agent: "chaz",
            session_name: None,
        }
    }

    /// Build a `ScopedTools` view that mirrors what the runtime would
    /// hand the agent for this session — full registry, no allowlist,
    /// per-session active-extension filter.
    async fn scoped_for(f: &Fixture) -> ScopedTools {
        let active = f.server.active_extensions_for(&f.session_db_id).await;
        ScopedTools::new(f.tool_registry.clone(), None).with_active_extensions(Some(active))
    }

    /// Visible tool names according to `ScopedTools::definitions`.
    fn visible_tool_names(scoped: &ScopedTools) -> std::collections::HashSet<String> {
        scoped
            .definitions(&ToolProfile::default())
            .into_iter()
            .map(|d| d.name)
            .collect()
    }

    // -------------------------------------------------------------------------
    // listing
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn list_shows_every_builtin_active_on_a_fresh_session() {
        let f = fixture().await;
        // Touch the active set so session_start fires record_active
        // and seeds the default-include events.
        let _ = f.server.active_extensions_for(&f.session_db_id).await;

        let out = dispatch(Command::Extensions(ExtensionsAction::List), &ctx(&f)).await;
        let text = match out {
            CommandOutcome::Text(s) => s,
            _ => panic!("expected CommandOutcome::Text"),
        };
        for name in [
            "core",
            "fs",
            "system",
            "web",
            "memory",
            "schedule",
            "path_normalizer",
            "security_warnings",
        ] {
            assert!(text.contains(name), "list missing {name}:\n{text}");
        }
        // Default-everything-active: every line is marked ✓.
        assert!(
            !text.lines().skip(1).any(|l| l.starts_with("   ")),
            "every extension should be active on a fresh session, but a row was un-marked:\n{text}"
        );
    }

    // -------------------------------------------------------------------------
    // add / remove flow
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn remove_hides_owned_tools_from_scoped_tools_definitions() {
        let f = fixture().await;
        // Seed the default-active set.
        let _ = f.server.active_extensions_for(&f.session_db_id).await;
        let before = visible_tool_names(&scoped_for(&f).await);
        assert!(before.contains("remember"));
        assert!(before.contains("recall"));
        assert!(before.contains("list_memory_banks"));

        let out = dispatch(
            Command::Extensions(ExtensionsAction::Remove("memory".into(), ExtScope::Session)),
            &ctx(&f),
        )
        .await;
        assert!(matches!(out, CommandOutcome::Text(_)));

        let after = visible_tool_names(&scoped_for(&f).await);
        assert!(!after.contains("remember"), "remember should be hidden");
        assert!(!after.contains("recall"), "recall should be hidden");
        assert!(
            !after.contains("list_memory_banks"),
            "list_memory_banks should be hidden"
        );
        // Tools from other still-active extensions must remain visible.
        assert!(after.contains("read_file"), "fs tools unaffected");
        assert!(after.contains("shell"), "core tools unaffected");
    }

    #[tokio::test]
    async fn add_after_remove_restores_tools_to_the_scope() {
        let f = fixture().await;
        let _ = f.server.active_extensions_for(&f.session_db_id).await;
        dispatch(
            Command::Extensions(ExtensionsAction::Remove("memory".into(), ExtScope::Session)),
            &ctx(&f),
        )
        .await;
        assert!(!visible_tool_names(&scoped_for(&f).await).contains("remember"));

        dispatch(
            Command::Extensions(ExtensionsAction::Add("memory".into(), ExtScope::Session)),
            &ctx(&f),
        )
        .await;
        assert!(visible_tool_names(&scoped_for(&f).await).contains("remember"));
    }

    #[tokio::test]
    async fn remove_survives_simulated_session_restart() {
        // After a remove + a fresh record_active (mimicking the
        // session_start reconciler on next startup), the deactivation
        // should stand — `record_active`'s respect-Deactivated rule.
        let f = fixture().await;
        let _ = f.server.active_extensions_for(&f.session_db_id).await;
        dispatch(
            Command::Extensions(ExtensionsAction::Remove("memory".into(), ExtScope::Session)),
            &ctx(&f),
        )
        .await;

        f.server
            .extensions()
            .record_active(&f.session_db)
            .await
            .unwrap();
        f.server.refresh_active_extensions(&f.session_db_id).await;

        let active = f.server.active_extensions_for(&f.session_db_id).await;
        assert!(
            !active.contains("memory"),
            "memory should stay removed after a record_active reconcile, got: {active:?}"
        );
    }

    // -------------------------------------------------------------------------
    // per-agent scope
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn agent_remove_narrows_only_that_agent_not_the_session() {
        let f = fixture().await;
        let _ = f.server.active_extensions_for(&f.session_db_id).await;

        // Disable memory for agent "chaz".
        let out = dispatch(
            Command::Extensions(ExtensionsAction::Remove("memory".into(), ExtScope::Agent)),
            &ctx(&f),
        )
        .await;
        assert!(matches!(out, CommandOutcome::Text(_)));

        // Session set is untouched...
        let session_active = f.server.active_extensions_for(&f.session_db_id).await;
        assert!(
            session_active.contains("memory"),
            "agent opt-out must not change the session set"
        );
        // ...but the agent's effective set drops memory.
        let agent_active = f
            .server
            .active_extensions_for_agent(&f.session_db_id, "chaz")
            .await;
        assert!(
            !agent_active.contains("memory"),
            "memory should be hidden from agent 'chaz', got: {agent_active:?}"
        );
        // A different agent (no records) is unaffected.
        let other = f
            .server
            .active_extensions_for_agent(&f.session_db_id, "someone-else")
            .await;
        assert!(other.contains("memory"), "other agents keep memory");
    }

    #[tokio::test]
    async fn agent_add_restores_after_agent_remove() {
        let f = fixture().await;
        let _ = f.server.active_extensions_for(&f.session_db_id).await;
        dispatch(
            Command::Extensions(ExtensionsAction::Remove("memory".into(), ExtScope::Agent)),
            &ctx(&f),
        )
        .await;
        assert!(
            !f.server
                .active_extensions_for_agent(&f.session_db_id, "chaz")
                .await
                .contains("memory")
        );

        dispatch(
            Command::Extensions(ExtensionsAction::Add("memory".into(), ExtScope::Agent)),
            &ctx(&f),
        )
        .await;
        assert!(
            f.server
                .active_extensions_for_agent(&f.session_db_id, "chaz")
                .await
                .contains("memory"),
            "agent add should clear the opt-out"
        );
    }

    #[tokio::test]
    async fn agent_remove_cannot_widen_past_session() {
        // If the session has removed an extension, an agent-scope add
        // can't bring it back — the session set is the upper bound.
        let f = fixture().await;
        let _ = f.server.active_extensions_for(&f.session_db_id).await;
        dispatch(
            Command::Extensions(ExtensionsAction::Remove("memory".into(), ExtScope::Session)),
            &ctx(&f),
        )
        .await;
        dispatch(
            Command::Extensions(ExtensionsAction::Add("memory".into(), ExtScope::Agent)),
            &ctx(&f),
        )
        .await;
        let agent_active = f
            .server
            .active_extensions_for_agent(&f.session_db_id, "chaz")
            .await;
        assert!(
            !agent_active.contains("memory"),
            "session removal wins over agent add"
        );
    }

    // -------------------------------------------------------------------------
    // command dispatch under inactive extensions
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn inactive_extension_command_returns_helpful_error() {
        let f = fixture().await;
        let _ = f.server.active_extensions_for(&f.session_db_id).await;
        dispatch(
            Command::Extensions(ExtensionsAction::Remove(
                "schedule".into(),
                ExtScope::Session,
            )),
            &ctx(&f),
        )
        .await;

        let out = super::super::dispatch_extension("schedule", "list", &ctx(&f)).await;
        match out {
            CommandOutcome::Error(msg) => {
                assert!(msg.contains("schedule"), "msg: {msg}");
                assert!(
                    msg.contains("not active") && msg.contains("/extensions add"),
                    "msg should point user at /extensions add: {msg}"
                );
            }
            _ => panic!("expected CommandOutcome::Error with the helpful pointer"),
        }
    }

    // -------------------------------------------------------------------------
    // multi-session isolation
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn two_sessions_have_independent_active_sets() {
        let f = fixture().await;
        // Create a second session on the same server. We need to
        // bypass the Fixture struct (it only carries one session DB).
        let (_, session_b) = f._registry.create_session(Some("other")).await.unwrap();
        let session_b_id = session_b.root_id().to_string();
        // Seed session B the same way the fixture seeds session A.
        f.server
            .extensions()
            .record_active(&session_b)
            .await
            .unwrap();
        f.server.refresh_active_extensions(&session_b_id).await;

        // Remove memory from session A only.
        dispatch(
            Command::Extensions(ExtensionsAction::Remove("memory".into(), ExtScope::Session)),
            &ctx(&f),
        )
        .await;

        let active_a = f.server.active_extensions_for(&f.session_db_id).await;
        let active_b = f.server.active_extensions_for(&session_b_id).await;
        assert!(!active_a.contains("memory"), "session A should drop memory");
        assert!(active_b.contains("memory"), "session B unaffected");
    }

    // -------------------------------------------------------------------------
    // settings round-trip
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn settings_round_trip_via_set_and_settings_subcommands() {
        let f = fixture().await;
        dispatch(
            Command::Extensions(ExtensionsAction::Set {
                name: "memory".into(),
                key: "custom_limit".into(),
                value: "8".into(),
            }),
            &ctx(&f),
        )
        .await;

        let out = dispatch(
            Command::Extensions(ExtensionsAction::Settings("memory".into())),
            &ctx(&f),
        )
        .await;
        let text = match out {
            CommandOutcome::Text(s) => s,
            _ => panic!("expected CommandOutcome::Text"),
        };
        assert!(
            text.contains("custom_limit"),
            "settings missing key: {text}"
        );
        // JSON-parse of "8" should land an integer, not the string "8".
        assert!(
            text.contains("8") && !text.contains("\"8\""),
            "value should be the integer 8, not the string: {text}"
        );
    }

    // -------------------------------------------------------------------------
    // validation
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn add_unknown_extension_errors() {
        let f = fixture().await;
        let out = dispatch(
            Command::Extensions(ExtensionsAction::Add(
                "doesnotexist".into(),
                ExtScope::Session,
            )),
            &ctx(&f),
        )
        .await;
        match out {
            CommandOutcome::Error(msg) => {
                assert!(msg.contains("doesnotexist"), "msg: {msg}");
                assert!(msg.contains("Unknown extension"), "msg: {msg}");
            }
            _ => panic!("expected CommandOutcome::Error"),
        }
    }

    #[tokio::test]
    async fn remove_unknown_extension_errors() {
        let f = fixture().await;
        let out = dispatch(
            Command::Extensions(ExtensionsAction::Remove(
                "doesnotexist".into(),
                ExtScope::Session,
            )),
            &ctx(&f),
        )
        .await;
        match out {
            CommandOutcome::Error(msg) => assert!(msg.contains("Unknown extension"), "msg: {msg}"),
            _ => panic!("expected CommandOutcome::Error"),
        }
    }
}
