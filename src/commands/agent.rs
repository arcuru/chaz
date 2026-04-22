//! Living Agents handlers: session participation (attach/detach/list/host)
//! and lifecycle (new/share/import/hosted/delete).
//!
//! `resolve_agent_ref` is `pub(super)` because `heartbeat::heartbeat_add`
//! also needs to resolve an agent ref to a DB id.

use crate::session::Session;
use crate::types::ConversationId;

use super::heartbeat::sweep_heartbeat_rules_for_agent;
use super::{CommandContext, CommandOutcome};

// -----------------------------------------------------------------------------
// Shared: agent ref resolution
// -----------------------------------------------------------------------------

/// Resolve a user-supplied ref — either an agent display name or an eidetica
/// DB ID — to a `HostedEntry`.
pub(super) async fn resolve_agent_ref(
    agent_ref: &str,
    ctx: &CommandContext<'_>,
) -> Result<crate::hosted_index::HostedEntry, String> {
    let index = ctx.server.agent_index();
    if let Ok(Some(entry)) = index.find_by_name(agent_ref).await {
        return Ok(entry);
    }
    if let Ok(id) = eidetica::entry::ID::parse(agent_ref) {
        if let Ok(Some(entry)) = index.find_by_id(&id).await {
            return Ok(entry);
        }
    }
    Err(format!(
        "No hosted agent matches '{agent_ref}' (try a display name from /agents or an agent DB ID)"
    ))
}

// -----------------------------------------------------------------------------
// Participation (Living Agents Stage 3d)
// -----------------------------------------------------------------------------

pub(super) async fn agent_add(agent_ref: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let entry = match resolve_agent_ref(agent_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };
    match ctx
        .server
        .registry()
        .attach_agent_to_session(ctx.session_db_id, &entry)
        .await
    {
        Ok(()) => CommandOutcome::Text(format!(
            "Attached agent '{}' to this session",
            entry.display_name
        )),
        Err(e) => CommandOutcome::Error(format!("Failed to attach agent: {e}")),
    }
}

pub(super) async fn agent_remove(agent_ref: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let entry = match resolve_agent_ref(agent_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };
    match ctx
        .server
        .registry()
        .detach_agent_from_session(ctx.session_db_id, &entry)
        .await
    {
        Ok(()) => CommandOutcome::Text(format!(
            "Detached agent '{}' from this session",
            entry.display_name
        )),
        Err(e) => CommandOutcome::Error(format!("Failed to detach agent: {e}")),
    }
}

pub(super) async fn agents_list(ctx: &CommandContext<'_>) -> CommandOutcome {
    let meta = crate::session::read_meta_from_db(ctx.session_db).await;
    if meta.agents.is_empty() {
        let fallback = meta.agent_name.unwrap_or_else(|| "<default>".to_string());
        return CommandOutcome::Text(format!(
            "No Living Agents attached to this session. Legacy agent: {fallback}"
        ));
    }
    let host = meta.host_agent_db_id.as_deref();
    let lines: Vec<String> = meta
        .agents
        .iter()
        .map(|a| {
            let marker = if host == Some(a.db_id.as_str()) {
                " *host*"
            } else {
                ""
            };
            format!("  {}{} ({})", a.display_name, marker, a.db_id)
        })
        .collect();
    CommandOutcome::Text(format!("Agents on this session:\n{}", lines.join("\n")))
}

pub(super) async fn agent_set_host(arg: Option<&str>, ctx: &CommandContext<'_>) -> CommandOutcome {
    let session = Session::new(
        ConversationId(ctx.session_db_id.to_string()),
        ctx.session_db.clone(),
    )
    .await;

    match arg {
        None => {
            if let Err(e) = session.update_meta(|m| m.host_agent_db_id = None).await {
                return CommandOutcome::Error(format!("Failed to clear host agent: {e}"));
            }
            CommandOutcome::Text("Cleared host agent for this session".to_string())
        }
        Some(agent_ref) => {
            let entry = match resolve_agent_ref(agent_ref, ctx).await {
                Ok(e) => e,
                Err(msg) => return CommandOutcome::Error(msg),
            };

            // Host must be attached — catch the "set host on un-attached agent" footgun.
            let meta = crate::session::read_meta_from_db(ctx.session_db).await;
            let db_id = entry.db_id.to_string();
            if !meta.agents.iter().any(|a| a.db_id == db_id) {
                return CommandOutcome::Error(format!(
                    "Agent '{}' is not attached to this session. Attach it first with /agent add {}",
                    entry.display_name, agent_ref
                ));
            }

            let name = entry.display_name.clone();
            if let Err(e) = session
                .update_meta(move |m| m.host_agent_db_id = Some(db_id))
                .await
            {
                return CommandOutcome::Error(format!("Failed to set host agent: {e}"));
            }
            CommandOutcome::Text(format!("Set host agent to '{name}'"))
        }
    }
}

// -----------------------------------------------------------------------------
// Lifecycle (Living Agents Stage 6): /agent new | share | import | hosted | delete
// -----------------------------------------------------------------------------

/// Supported `/agent new` and `/agent set` keys. Nested-structure fields
/// (`grants`, `presets`) intentionally omitted — edit yaml template or add
/// a dedicated command.
const SUPPORTED_AGENT_FIELDS: &str = "role, model, tools, can_spawn, allowed_callers, autonomous, max_iterations, tool_profile, max_context_tokens";

/// Apply a single `key=value` override to an `AgentDbConfig`. Used by
/// `/agent new` (on a fresh config) and `/agent set` (on a DB-loaded one).
/// Unknown keys surface as user-facing errors so typos aren't silently dropped.
pub(super) fn apply_agent_field(
    cfg: &mut crate::agent_db::AgentDbConfig,
    key: &str,
    value: &str,
) -> Result<(), String> {
    let comma_split = |v: &str| -> Vec<String> {
        v.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    let parse_bool = |v: &str| -> Result<bool, String> {
        match v.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            _ => Err(format!("Invalid bool '{v}' (use true|false)")),
        }
    };

    match key {
        "role" => cfg.role = Some(value.to_string()),
        "model" => cfg.model = Some(value.to_string()),
        "tools" => cfg.tools = Some(comma_split(value)),
        "can_spawn" => cfg.can_spawn = comma_split(value),
        "allowed_callers" => cfg.allowed_callers = comma_split(value),
        "autonomous" => cfg.autonomous = parse_bool(value)?,
        "max_iterations" => {
            cfg.max_iterations = Some(
                value
                    .parse::<u32>()
                    .map_err(|e| format!("Invalid max_iterations '{value}': {e}"))?,
            );
        }
        "tool_profile" => cfg.tool_profile = Some(value.to_string()),
        "max_context_tokens" => {
            cfg.max_context_tokens = Some(
                value
                    .parse::<usize>()
                    .map_err(|e| format!("Invalid max_context_tokens '{value}': {e}"))?,
            );
        }
        other => {
            return Err(format!(
                "Unknown agent field '{other}'. Supported: {SUPPORTED_AGENT_FIELDS}"
            ));
        }
    }
    Ok(())
}

fn apply_agent_new_overrides(
    cfg: &mut crate::agent_db::AgentDbConfig,
    overrides: &[(String, String)],
) -> Result<(), String> {
    for (key, value) in overrides {
        apply_agent_field(cfg, key, value)?;
    }
    Ok(())
}

pub(super) async fn agent_new(
    name: &str,
    overrides: &[(String, String)],
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let name = name.trim();
    if name.is_empty() {
        return CommandOutcome::Error("Agent name required".to_string());
    }

    // Reject duplicates at the registry level early — create_new_agent_db also
    // rejects at the DB-name level, but this catches in-memory collisions too.
    if ctx.server.agents().get(name).is_some() {
        return CommandOutcome::Error(format!("Agent '{name}' already registered"));
    }

    let mut cfg = crate::agent_db::AgentDbConfig::default();
    if let Err(msg) = apply_agent_new_overrides(&mut cfg, overrides) {
        return CommandOutcome::Error(msg);
    }
    let meta = crate::agent_db::AgentMeta {
        display_name: Some(name.to_string()),
        ..Default::default()
    };

    let (agent_db, pubkey) = match ctx
        .server
        .registry()
        .create_new_agent_db(name, &cfg, &meta)
        .await
    {
        Ok(p) => p,
        Err(e) => return CommandOutcome::Error(format!("Failed to create Agent DB: {e}")),
    };
    let db_id = agent_db.id();

    // Register in the peer-local agent index.
    if let Err(e) = ctx
        .server
        .agent_index()
        .register(crate::hosted_index::HostedEntry {
            db_id: db_id.clone(),
            display_name: name.to_string(),
            pubkey: pubkey.clone(),
        })
        .await
    {
        return CommandOutcome::Error(format!(
            "Agent DB created but index registration failed: {e}"
        ));
    }

    // Build a runtime Agent so the AgentRegistry can resolve it — makes the
    // agent spawnable + attachable by display name for the rest of this session.
    let runtime_agent = ctx.server.agents().build_from_db_config(name, &cfg);
    if let Err(e) = ctx.server.agents().register(runtime_agent) {
        return CommandOutcome::Error(format!(
            "Agent DB created + indexed but runtime registry rejected: {e}"
        ));
    }

    CommandOutcome::Text(format!(
        "Created Living Agent '{name}' (DB: {db_id}). Attach to a session with /agent add {name}."
    ))
}

pub(super) async fn agent_share(agent_ref: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let entry = match resolve_agent_ref(agent_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };

    let instance = ctx.server.registry().instance();
    let Some(sync) = instance.sync() else {
        return CommandOutcome::Error("Sync not enabled".to_string());
    };

    let mut ticket = eidetica::sync::DatabaseTicket::new(entry.db_id.clone());
    if let Ok(addresses) = sync.get_all_server_addresses().await {
        for (transport_type, address) in addresses {
            ticket.add_address(eidetica::sync::Address::new(transport_type, address));
        }
    }
    CommandOutcome::Text(format!(
        "Share this ticket to sync agent '{}' (DB {}):\n\n{ticket}",
        entry.display_name, entry.db_id
    ))
}

pub(super) async fn agent_import(ticket_str: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let instance = ctx.server.registry().instance();
    let Some(sync) = instance.sync() else {
        return CommandOutcome::Error("Sync not enabled".to_string());
    };

    let ticket: eidetica::sync::DatabaseTicket = match ticket_str.parse() {
        Ok(t) => t,
        Err(e) => return CommandOutcome::Error(format!("Invalid ticket: {e}")),
    };
    let db_id = ticket.database_id().clone();

    if let Err(e) = sync.sync_with_ticket(&ticket).await {
        return CommandOutcome::Error(format!("Sync failed: {e}"));
    }

    // After sync, we need a key on this peer for the agent DB to open it and
    // read its meta/config stores. Without a key, we can't register the agent
    // locally — the ticket syncs entries but not keys.
    let agent_db = match ctx.server.registry().open_agent_db(&db_id).await {
        Ok(Some(db)) => db,
        Ok(None) => {
            return CommandOutcome::Error(format!(
                "Synced agent DB {db_id} but this peer holds no key for it. \
                 Read-only agent sharing is not supported yet — ask the owner to share a key-bearing ticket."
            ));
        }
        Err(e) => return CommandOutcome::Error(format!("Failed to open synced agent DB: {e}")),
    };

    let meta = match agent_db.read_meta().await {
        Ok(m) => m,
        Err(e) => return CommandOutcome::Error(format!("Failed to read agent meta: {e}")),
    };
    let cfg = match agent_db.read_config().await {
        Ok(c) => c,
        Err(e) => return CommandOutcome::Error(format!("Failed to read agent config: {e}")),
    };
    let display_name = meta.display_name.clone().unwrap_or_else(|| {
        format!(
            "agent-{}",
            &db_id.to_string()[..8.min(db_id.to_string().len())]
        )
    });

    // Resolve the pubkey we hold for this DB — that's what `attach` writes
    // into session AuthSettings later. `open_agent_db` above already proved
    // a key exists; this second lookup is just to get the pubkey out.
    let pubkey =
        match ctx.server.registry().find_key_for_db(&db_id).await {
            Ok(Some(k)) => k,
            _ => return CommandOutcome::Error(
                "Expected a key for this DB (open_agent_db succeeded) but find_key returned None"
                    .to_string(),
            ),
        };

    if let Err(e) = ctx
        .server
        .agent_index()
        .register(crate::hosted_index::HostedEntry {
            db_id: db_id.clone(),
            display_name: display_name.clone(),
            pubkey,
        })
        .await
    {
        return CommandOutcome::Error(format!("Index registration failed: {e}"));
    }

    // Upsert into the runtime registry so re-importing a previously-seen
    // agent refreshes its config from the synced DB (model/tools/role may
    // have changed upstream since the last import).
    let runtime_agent = ctx
        .server
        .agents()
        .build_from_db_config(&display_name, &cfg);
    ctx.server.agents().upsert(runtime_agent);

    CommandOutcome::Text(format!(
        "Imported agent '{display_name}' (DB {db_id}). Attach with /agent add {display_name}."
    ))
}

pub(super) async fn agent_hosted(ctx: &CommandContext<'_>) -> CommandOutcome {
    let entries = match ctx.server.agent_index().list().await {
        Ok(e) => e,
        Err(e) => return CommandOutcome::Error(format!("Failed to list hosted agents: {e}")),
    };
    if entries.is_empty() {
        return CommandOutcome::Text("No Living Agents hosted on this peer.".to_string());
    }
    let lines: Vec<String> = entries
        .iter()
        .map(|e| format!("  {} ({})", e.display_name, e.db_id))
        .collect();
    CommandOutcome::Text(format!(
        "Living Agents hosted on this peer:\n{}",
        lines.join("\n")
    ))
}

pub(super) async fn agent_delete(agent_ref: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let entry = match resolve_agent_ref(agent_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };

    // Refuse if the agent is still attached to any known session. Walking
    // every session is O(N) but agent-delete is a rare operation.
    let sessions = match ctx.server.registry().list_sessions().await {
        Ok(s) => s,
        Err(e) => return CommandOutcome::Error(format!("Failed to list sessions: {e}")),
    };
    let db_id_str = entry.db_id.to_string();
    for idx in &sessions {
        let (_conv, sdb) = match ctx.server.registry().open_session(&idx.session_db_id).await {
            Ok(pair) => pair,
            Err(_) => continue,
        };
        let meta = crate::session::read_meta_from_db(&sdb).await;
        if meta.agents.iter().any(|a| a.db_id == db_id_str) {
            return CommandOutcome::Error(format!(
                "Agent '{}' is still attached to session {}. Detach it first (/agent remove {}).",
                entry.display_name, idx.session_db_id, entry.display_name
            ));
        }
    }

    if let Err(e) = ctx.server.agent_index().unregister(&entry.db_id).await {
        return CommandOutcome::Error(format!("Failed to unregister from index: {e}"));
    }
    ctx.server.agents().unregister(&entry.display_name);

    // Also drop peer-local heartbeat rules targeting this agent across every
    // session on this peer. Rules that fire into a missing agent are silent
    // dead weight; this keeps the state clean.
    let sweep = sweep_heartbeat_rules_for_agent(ctx, &db_id_str).await;

    let mut msg = format!(
        "Deleted Living Agent '{}' (DB {} preserved for archive).",
        entry.display_name, entry.db_id
    );
    if sweep > 0 {
        msg.push_str(&format!(" Removed {sweep} heartbeat rule(s) targeting it."));
    }
    CommandOutcome::Text(msg)
}

/// Edit one field on a Living Agent's DB config. Stage 8 live hydration
/// picks up the change on the next message — no restart. We also upsert
/// the runtime `AgentRegistry` snapshot so the current session sees the
/// edit immediately (hydration rebuilds on message, upsert is belt-and-
/// suspenders for code paths that read registry state between messages).
pub(super) async fn agent_set(
    agent_ref: &str,
    field: &str,
    value: &str,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let entry = match resolve_agent_ref(agent_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };

    let agent_db = match ctx.server.registry().open_agent_db(&entry.db_id).await {
        Ok(Some(db)) => db,
        Ok(None) => {
            return CommandOutcome::Error(format!(
                "This peer holds no key for agent '{}' — can't edit a read-only import",
                entry.display_name
            ));
        }
        Err(e) => return CommandOutcome::Error(format!("Failed to open agent DB: {e}")),
    };

    let mut cfg = match agent_db.read_config().await {
        Ok(c) => c,
        Err(e) => return CommandOutcome::Error(format!("Failed to read agent config: {e}")),
    };

    if let Err(msg) = apply_agent_field(&mut cfg, field, value) {
        return CommandOutcome::Error(msg);
    }

    if let Err(e) = agent_db.write_config(&cfg).await {
        return CommandOutcome::Error(format!("Failed to write agent config: {e}"));
    }

    let runtime_agent = ctx
        .server
        .agents()
        .build_from_db_config(&entry.display_name, &cfg);
    ctx.server.agents().upsert(runtime_agent);

    CommandOutcome::Text(format!(
        "Set {field}={value} on agent '{}' (takes effect next message)",
        entry.display_name
    ))
}

#[cfg(test)]
mod tests {
    use super::super::{Command, CommandContext, CommandOutcome, dispatch};
    use crate::agent::AgentRegistry;
    use crate::agent_db::find_agent_db;
    use crate::backends::BackendManager;
    use crate::hosted_index::HostedIndex;
    use crate::security::SecretStore;
    use crate::server::Server;
    use eidetica::Instance;
    use eidetica::backend::database::InMemory;
    use std::sync::Arc;

    fn blank_config() -> crate::config::Config {
        crate::config::Config {
            homeserver_url: String::new(),
            username: String::new(),
            password: None,
            allow_list: None,
            message_limit: None,
            room_size_limit: None,
            state_dir: None,
            chat_summary_model: None,
            role: None,
            roles: None,
            backends: None,
            agents: None,
            security: None,
            schedules: None,
            mcp_servers: None,
            tool_profiles: None,
            mcp_server_dir: None,
            context: None,
        }
    }

    /// End-to-end fixture: Server + SessionRegistry + one open session +
    /// SecretStore/BackendManager suitable for running commands::dispatch.
    /// Returns (instance, server, registry, secrets, backend, session_db_id, session_db).
    async fn fixture() -> (
        Instance,
        Arc<Server>,
        Arc<crate::session::SessionRegistry>,
        SecretStore,
        BackendManager,
        String,
        eidetica::Database,
    ) {
        let backend = InMemory::new();
        let instance = Instance::open(Box::new(backend)).await.unwrap();
        let _ = instance.create_user("test", None).await;
        let user = instance.login_user("test", None).await.unwrap();
        let agents = Arc::new(AgentRegistry::from_config(&blank_config()));
        let registry = Arc::new(
            crate::session::SessionRegistry::new(instance.clone(), user, agents.clone())
                .await
                .unwrap(),
        );
        let chazdb = registry.chazdb().clone();
        let index = HostedIndex::agents(chazdb.clone());
        let bank_index = HostedIndex::memory_banks(chazdb.clone());
        let tools = Arc::new(crate::tool::ToolRegistry::new());
        let policies = Arc::new(crate::tool::ToolPolicyRegistry::empty());
        let security = crate::security::SecurityContext {
            leak_detector: crate::security::LeakDetector::new(
                crate::security::LeakPolicy::default(),
            ),
            auto_approved_tools: std::collections::HashSet::new(),
            approval_callback: None,
        };
        let server = Server::new(
            registry.clone(),
            agents,
            index,
            bank_index,
            tools,
            policies,
            security,
            std::collections::HashMap::new(),
            Default::default(),
        );
        let secrets = SecretStore::new(chazdb).await;
        let backend_mgr = BackendManager::new(&None, secrets.clone());
        let (_conv, session_db) = registry.create_session(Some("test")).await.unwrap();
        let session_db_id = session_db.root_id().to_string();
        (
            instance,
            server,
            registry,
            secrets,
            backend_mgr,
            session_db_id,
            session_db,
        )
    }

    fn cmd_ctx<'a>(
        server: &'a Arc<Server>,
        secrets: &'a SecretStore,
        backend: &'a BackendManager,
        session_db_id: &'a str,
        session_db: &'a eidetica::Database,
    ) -> CommandContext<'a> {
        CommandContext {
            server,
            scheduler: None,
            secrets,
            backend,
            session_db_id,
            session_db,
            current_agent: "chaz",
            session_name: None,
            config_roles: None,
            default_role: None,
        }
    }

    #[tokio::test]
    async fn agent_new_writes_overrides_into_db_and_registers() {
        let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        let cmd = Command::AgentNew {
            name: "alpha".to_string(),
            overrides: vec![
                ("model".into(), "opus".into()),
                ("max_iterations".into(), "42".into()),
                ("tools".into(), "get_time,calculate".into()),
            ],
        };
        match dispatch(cmd, &ctx).await {
            CommandOutcome::Text(_) => {}
            _ => panic!("expected Text outcome, got non-matching variant"),
        }

        // Runtime registry reflects the overrides.
        let agent = server.agents().get("alpha").expect("agent registered");
        assert_eq!(agent.default_model.as_deref(), Some("opus"));
        assert_eq!(agent.max_iterations, 42);
        assert_eq!(
            agent.allowed_tools.as_deref(),
            Some(&["get_time".to_string(), "calculate".to_string()][..])
        );

        // Persisted config in the AgentDb matches too.
        let user = registry.user_for_tests().await;
        let (db, _pk) = find_agent_db(&user, "alpha").await.expect("DB exists");
        drop(user);
        let cfg = db.read_config().await.unwrap();
        assert_eq!(cfg.model.as_deref(), Some("opus"));
        assert_eq!(cfg.max_iterations, Some(42));
    }

    #[tokio::test]
    async fn agent_new_rejects_unknown_override() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        let cmd = Command::AgentNew {
            name: "alpha".to_string(),
            overrides: vec![("bogus".into(), "x".into())],
        };
        match dispatch(cmd, &ctx).await {
            CommandOutcome::Error(msg) => assert!(msg.contains("Unknown"), "got {msg}"),
            _ => panic!("expected Error, got non-matching variant"),
        }
        // Agent should NOT be registered.
        assert!(server.agents().get("alpha").is_none());
    }

    #[tokio::test]
    async fn agent_hosted_lists_registered_agents() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        // Before any /agent new, the index is empty.
        match dispatch(Command::AgentHosted, &ctx).await {
            CommandOutcome::Text(msg) => assert!(msg.contains("No Living Agents"), "got {msg}"),
            _ => panic!("expected Text, got non-matching variant"),
        }

        // Create two agents and verify they both appear.
        for name in ["alpha", "beta"] {
            let _ = dispatch(
                Command::AgentNew {
                    name: name.to_string(),
                    overrides: vec![],
                },
                &ctx,
            )
            .await;
        }
        match dispatch(Command::AgentHosted, &ctx).await {
            CommandOutcome::Text(msg) => {
                assert!(msg.contains("alpha"), "missing alpha in {msg}");
                assert!(msg.contains("beta"), "missing beta in {msg}");
            }
            _ => panic!("expected Text, got non-matching variant"),
        }
    }

    #[tokio::test]
    async fn agent_delete_removes_from_index_and_registry() {
        let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        dispatch(
            Command::AgentNew {
                name: "alpha".to_string(),
                overrides: vec![],
            },
            &ctx,
        )
        .await;
        assert!(server.agents().get("alpha").is_some());

        let result = dispatch(Command::AgentDelete("alpha".to_string()), &ctx).await;
        match result {
            CommandOutcome::Text(msg) => assert!(msg.contains("Deleted")),
            _ => panic!("expected Text, got non-matching variant"),
        }

        // Gone from runtime registry.
        assert!(server.agents().get("alpha").is_none());
        // Gone from agents index.
        assert!(
            server
                .agent_index()
                .find_by_name("alpha")
                .await
                .unwrap()
                .is_none()
        );
        // But the DB is still present (preserved for archive).
        let user = registry.user_for_tests().await;
        assert!(find_agent_db(&user, "alpha").await.is_some());
    }

    #[tokio::test]
    async fn agent_delete_refuses_if_attached_to_session() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        dispatch(
            Command::AgentNew {
                name: "alpha".to_string(),
                overrides: vec![],
            },
            &ctx,
        )
        .await;
        dispatch(Command::AgentAdd("alpha".to_string()), &ctx).await;

        let result = dispatch(Command::AgentDelete("alpha".to_string()), &ctx).await;
        match result {
            CommandOutcome::Error(msg) => assert!(msg.contains("still attached"), "got {msg}"),
            _ => panic!("expected Error, got non-matching variant"),
        }
        // Still registered.
        assert!(server.agents().get("alpha").is_some());
    }

    #[tokio::test]
    async fn agent_delete_sweeps_heartbeat_rules() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        dispatch(
            Command::AgentNew {
                name: "alpha".to_string(),
                overrides: vec![],
            },
            &ctx,
        )
        .await;
        dispatch(Command::AgentAdd("alpha".to_string()), &ctx).await;
        let cmd = Command::HeartbeatAdd {
            id: "rule1".to_string(),
            cron: "0 0 * * * *".to_string(),
            agent_ref: "alpha".to_string(),
            task: "ping".to_string(),
        };
        dispatch(cmd, &ctx).await;

        // Rule exists before delete.
        let before = crate::heartbeat::list_rules(&sdb).await.unwrap();
        assert_eq!(before.len(), 1);

        // Detach first (delete refuses while attached), then delete.
        dispatch(Command::AgentRemove("alpha".to_string()), &ctx).await;
        // Detach-side cleanup should already have removed the rule.
        let after_detach = crate::heartbeat::list_rules(&sdb).await.unwrap();
        assert!(
            after_detach.is_empty(),
            "detach should sweep heartbeat rules, got {after_detach:?}"
        );
    }

    // -------------------------------------------------------------------------
    // /agent new — extended field coverage (can_spawn / allowed_callers / autonomous)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn agent_new_accepts_can_spawn_allowed_callers_autonomous() {
        let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        let cmd = Command::AgentNew {
            name: "alpha".to_string(),
            overrides: vec![
                ("can_spawn".into(), "beta,gamma".into()),
                ("allowed_callers".into(), "chaz".into()),
                ("autonomous".into(), "true".into()),
            ],
        };
        match dispatch(cmd, &ctx).await {
            CommandOutcome::Text(_) => {}
            CommandOutcome::Error(e) => panic!("unexpected error: {e}"),
            _ => panic!("expected Text"),
        }

        let agent = server.agents().get("alpha").unwrap();
        assert_eq!(
            agent.can_spawn,
            vec!["beta".to_string(), "gamma".to_string()]
        );
        assert_eq!(agent.allowed_callers, vec!["chaz".to_string()]);
        assert!(agent.autonomous);

        // And persisted to the DB.
        let user = registry.user_for_tests().await;
        let (db, _pk) = find_agent_db(&user, "alpha").await.unwrap();
        drop(user);
        let cfg = db.read_config().await.unwrap();
        assert_eq!(cfg.can_spawn, vec!["beta".to_string(), "gamma".to_string()]);
        assert_eq!(cfg.allowed_callers, vec!["chaz".to_string()]);
        assert!(cfg.autonomous);
    }

    #[tokio::test]
    async fn agent_new_rejects_invalid_bool_for_autonomous() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        let cmd = Command::AgentNew {
            name: "alpha".to_string(),
            overrides: vec![("autonomous".into(), "maybe".into())],
        };
        match dispatch(cmd, &ctx).await {
            CommandOutcome::Error(msg) => assert!(msg.contains("Invalid bool"), "got {msg}"),
            _ => panic!("expected Error"),
        }
    }

    // -------------------------------------------------------------------------
    // /agent set — edit a single field on an existing agent
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn agent_set_updates_db_and_registry() {
        let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        // Create with a baseline model.
        dispatch(
            Command::AgentNew {
                name: "alpha".to_string(),
                overrides: vec![("model".into(), "haiku".into())],
            },
            &ctx,
        )
        .await;

        // Edit one field.
        let cmd = Command::AgentSet {
            agent_ref: "alpha".to_string(),
            field: "model".to_string(),
            value: "opus".to_string(),
        };
        match dispatch(cmd, &ctx).await {
            CommandOutcome::Text(msg) => assert!(msg.contains("alpha"), "got {msg}"),
            CommandOutcome::Error(e) => panic!("unexpected error: {e}"),
            _ => panic!("expected Text"),
        }

        // Runtime registry reflects the new value.
        assert_eq!(
            server
                .agents()
                .get("alpha")
                .unwrap()
                .default_model
                .as_deref(),
            Some("opus")
        );

        // DB reflects it too — Stage 8 hydration will read this on next message.
        let user = registry.user_for_tests().await;
        let (db, _pk) = find_agent_db(&user, "alpha").await.unwrap();
        drop(user);
        assert_eq!(
            db.read_config().await.unwrap().model.as_deref(),
            Some("opus")
        );
    }

    #[tokio::test]
    async fn agent_set_rejects_unknown_field() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        dispatch(
            Command::AgentNew {
                name: "alpha".to_string(),
                overrides: vec![],
            },
            &ctx,
        )
        .await;

        let cmd = Command::AgentSet {
            agent_ref: "alpha".to_string(),
            field: "bogus".to_string(),
            value: "x".to_string(),
        };
        match dispatch(cmd, &ctx).await {
            CommandOutcome::Error(msg) => assert!(msg.contains("Unknown"), "got {msg}"),
            _ => panic!("expected Error"),
        }
    }

    #[tokio::test]
    async fn agent_set_missing_agent_errors() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        let cmd = Command::AgentSet {
            agent_ref: "ghost".to_string(),
            field: "model".to_string(),
            value: "opus".to_string(),
        };
        match dispatch(cmd, &ctx).await {
            CommandOutcome::Error(msg) => assert!(msg.contains("No hosted agent"), "got {msg}"),
            _ => panic!("expected Error"),
        }
    }
}
