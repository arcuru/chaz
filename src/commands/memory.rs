//! Memory Banks handlers (Memory Banks Stage 9.D).
//!
//! Stage 9.D.1 ships the peer-local CRUD: `/memory new`, `/memory list`,
//! `/memory delete`. Grant/revoke arrive in 9.D.2; share/import in 9.D.3.

use super::{CommandContext, CommandOutcome};

/// Resolve a user-supplied ref — either a bank display name or an
/// eidetica DB ID — to a `DbEntry`.
pub(super) async fn resolve_bank_ref(
    bank_ref: &str,
    ctx: &CommandContext<'_>,
) -> Result<crate::db_registry::DbEntry, String> {
    let index = ctx.server.memory_bank_index();
    if let Ok(Some(entry)) = index.find_by_name(bank_ref).await {
        return Ok(entry);
    }
    if let Ok(id) = eidetica::entry::ID::parse(bank_ref) {
        if let Ok(Some(entry)) = index.find_by_id(&id).await {
            return Ok(entry);
        }
    }
    Err(format!(
        "No hosted memory bank matches '{bank_ref}' (try a display name from /memory list or a bank DB ID)"
    ))
}

pub(super) async fn memory_new(
    name: &str,
    description: Option<&str>,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let name = name.trim();
    if name.is_empty() {
        return CommandOutcome::Error("Memory bank name required".to_string());
    }

    let meta = crate::memory_bank_db::MemoryBankMeta {
        display_name: Some(name.to_string()),
        description: description.map(|s| s.to_string()),
    };

    let (bank, pubkey) = match ctx
        .server
        .registry()
        .create_new_memory_bank(name, &meta)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            return CommandOutcome::Error(format!("Failed to create memory bank: {e}"));
        }
    };

    if let Err(e) = ctx
        .server
        .memory_bank_index()
        .register(crate::db_registry::DbEntry {
            db_id: bank.id(),
            display_name: name.to_string(),
            pubkey,
        })
        .await
    {
        return CommandOutcome::Error(format!(
            "Bank DB created but index registration failed: {e}"
        ));
    }

    CommandOutcome::Text(format!(
        "Created memory bank '{name}' (DB {}). Grant it to an agent with /memory grant.",
        bank.id()
    ))
}

pub(super) async fn memory_list(ctx: &CommandContext<'_>) -> CommandOutcome {
    let entries = match ctx.server.memory_bank_index().list().await {
        Ok(e) => e,
        Err(e) => return CommandOutcome::Error(format!("Failed to list memory banks: {e}")),
    };
    if entries.is_empty() {
        return CommandOutcome::Text(
            "No memory banks on this peer. Create one with /memory new <name>.".to_string(),
        );
    }
    let lines: Vec<String> = entries
        .iter()
        .map(|e| format!("  {} ({})", e.display_name, e.db_id))
        .collect();
    CommandOutcome::Text(format!("Memory banks on this peer:\n{}", lines.join("\n")))
}

/// Grant an agent access to a memory bank (Stage 9.D.2).
///
/// Order matters: we write auth first (the authoritative side), then
/// mirror the ref into the agent's `memory_banks` subtree. If the ref
/// write fails, best-effort revoke the auth so the two stores stay
/// consistent. The opposite order would leave a brief window where the
/// agent thinks it has access it doesn't.
pub(super) async fn memory_grant(
    bank_ref: &str,
    agent_ref: &str,
    permission: crate::agent_db::BankPermission,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let bank = match resolve_bank_ref(bank_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };
    let agent = match super::agent::resolve_agent_ref(agent_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };

    let key_label = format!("memory:{}:{}", bank.display_name, agent.display_name);
    if let Err(e) = ctx
        .server
        .registry()
        .grant_on_memory_bank(&bank.db_id, &agent.pubkey, &key_label, permission)
        .await
    {
        return CommandOutcome::Error(format!("Failed to authorize agent on bank: {e}"));
    }

    // Now mirror the ref into the agent's view. On failure, roll back the auth
    // so the two sides stay consistent.
    let agent_db = match ctx.server.registry().open_agent_db(&agent.db_id).await {
        Ok(Some(db)) => db,
        Ok(None) => {
            // Rollback — the agent is in index but we can't open its DB. Shouldn't
            // happen on this peer, but bail cleanly.
            let _ = ctx
                .server
                .registry()
                .revoke_on_memory_bank(&bank.db_id, &agent.pubkey)
                .await;
            return CommandOutcome::Error(format!(
                "Granted auth but can't open agent '{}'s DB to record the ref — rolled back",
                agent.display_name
            ));
        }
        Err(e) => {
            let _ = ctx
                .server
                .registry()
                .revoke_on_memory_bank(&bank.db_id, &agent.pubkey)
                .await;
            return CommandOutcome::Error(format!(
                "Granted auth but failed to open agent DB — rolled back: {e}"
            ));
        }
    };

    let ref_entry = crate::agent_db::MemoryBankRef {
        name: bank.display_name.clone(),
        db_id: bank.db_id.to_string(),
        permission,
    };
    if let Err(e) = agent_db.attach_memory_bank(ref_entry).await {
        let _ = ctx
            .server
            .registry()
            .revoke_on_memory_bank(&bank.db_id, &agent.pubkey)
            .await;
        return CommandOutcome::Error(format!(
            "Granted auth but failed to write ref to agent DB — rolled back: {e}"
        ));
    }

    CommandOutcome::Text(format!(
        "Granted agent '{}' {:?} access to memory bank '{}'",
        agent.display_name, permission, bank.display_name
    ))
}

/// Revoke an agent's access to a memory bank (Stage 9.D.2). Revokes
/// auth first; then best-effort detaches the ref. If the ref detach
/// fails, the agent may still list the bank but eidetica will refuse
/// writes (authority is gone) — stale state, not a security issue.
pub(super) async fn memory_revoke(
    bank_ref: &str,
    agent_ref: &str,
    ctx: &CommandContext<'_>,
) -> CommandOutcome {
    let bank = match resolve_bank_ref(bank_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };
    let agent = match super::agent::resolve_agent_ref(agent_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };

    if let Err(e) = ctx
        .server
        .registry()
        .revoke_on_memory_bank(&bank.db_id, &agent.pubkey)
        .await
    {
        return CommandOutcome::Error(format!("Failed to revoke auth: {e}"));
    }

    // Best-effort ref cleanup.
    let ref_removed = match ctx.server.registry().open_agent_db(&agent.db_id).await {
        Ok(Some(db)) => db.detach_memory_bank(&bank.display_name).await.ok(),
        _ => None,
    };

    let mut msg = format!(
        "Revoked agent '{}'s access to memory bank '{}'",
        agent.display_name, bank.display_name
    );
    if ref_removed != Some(true) {
        msg.push_str(" (note: couldn't remove the ref from the agent's memory_banks subtree — auth is revoked regardless)");
    }
    CommandOutcome::Text(msg)
}

/// Generate a DatabaseTicket URL for an existing memory bank (Stage
/// 9.D.3). Same shape as `/agent share` — the ticket contains the
/// bank's DB root ID and this peer's sync addresses.
pub(super) async fn memory_share(bank_ref: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let entry = match resolve_bank_ref(bank_ref, ctx).await {
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
        "Share this ticket to sync memory bank '{}' (DB {}):\n\n{ticket}",
        entry.display_name, entry.db_id
    ))
}

/// Sync a memory bank from a DatabaseTicket URL and register it
/// locally (Stage 9.D.3). Requires the ticket to include a key for
/// this peer — read-only imports aren't supported yet (blocked on
/// eidetica's `Database::open_unauthenticated` being pub(crate)).
pub(super) async fn memory_import(ticket_str: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
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

    let bank_db = match ctx.server.registry().open_memory_bank(&db_id).await {
        Ok(Some(db)) => db,
        Ok(None) => {
            return CommandOutcome::Error(format!(
                "Synced memory bank DB {db_id} but this peer holds no key for it. \
                 Read-only bank sharing is not supported yet — ask the owner to share a key-bearing ticket."
            ));
        }
        Err(e) => return CommandOutcome::Error(format!("Failed to open synced bank: {e}")),
    };

    let meta = match bank_db.read_meta().await {
        Ok(m) => m,
        Err(e) => return CommandOutcome::Error(format!("Failed to read bank meta: {e}")),
    };
    let display_name = meta.display_name.clone().unwrap_or_else(|| {
        format!(
            "bank-{}",
            &db_id.to_string()[..8.min(db_id.to_string().len())]
        )
    });

    let pubkey = match ctx.server.registry().find_key_for_db(&db_id).await {
        Ok(Some(k)) => k,
        _ => {
            return CommandOutcome::Error(
                "Expected a key for this DB (open succeeded) but find_key returned None"
                    .to_string(),
            );
        }
    };

    if let Err(e) = ctx
        .server
        .memory_bank_index()
        .register(crate::db_registry::DbEntry {
            db_id: db_id.clone(),
            display_name: display_name.clone(),
            pubkey,
        })
        .await
    {
        return CommandOutcome::Error(format!("Index registration failed: {e}"));
    }

    CommandOutcome::Text(format!(
        "Imported memory bank '{display_name}' (DB {db_id}). \
         Grant it to agents with /memory grant {display_name} <agent> <read|write>."
    ))
}

pub(super) async fn memory_delete(bank_ref: &str, ctx: &CommandContext<'_>) -> CommandOutcome {
    let entry = match resolve_bank_ref(bank_ref, ctx).await {
        Ok(e) => e,
        Err(msg) => return CommandOutcome::Error(msg),
    };

    if let Err(e) = ctx
        .server
        .memory_bank_index()
        .unregister(&entry.db_id)
        .await
    {
        return CommandOutcome::Error(format!("Failed to unregister from index: {e}"));
    }

    CommandOutcome::Text(format!(
        "Deleted memory bank '{}' (DB {} preserved for archive). \
         Agents with this bank in their memory_banks subtree will still see it listed — \
         use /memory revoke to remove grants, coming in Stage 9.D.2.",
        entry.display_name, entry.db_id
    ))
}

#[cfg(test)]
mod tests {
    use super::super::{Command, CommandContext, CommandOutcome, dispatch};
    use crate::agent::AgentRegistry;
    use crate::backends::BackendManager;
    use crate::db_registry::DbRegistry;
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
        let index = DbRegistry::agents(chazdb.clone());
        let bank_index = DbRegistry::memory_banks(chazdb.clone());
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
    async fn memory_new_creates_and_registers() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        match dispatch(
            Command::MemoryNew {
                name: "patrick".to_string(),
                description: Some("notes about Patrick".to_string()),
            },
            &ctx,
        )
        .await
        {
            CommandOutcome::Text(msg) => assert!(msg.contains("patrick"), "got {msg}"),
            CommandOutcome::Error(e) => panic!("unexpected error: {e}"),
            _ => panic!("expected Text"),
        }

        let banks = server.memory_bank_index().list().await.unwrap();
        assert_eq!(banks.len(), 1);
        assert_eq!(banks[0].display_name, "patrick");
    }

    #[tokio::test]
    async fn memory_new_rejects_duplicate_name() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        dispatch(
            Command::MemoryNew {
                name: "patrick".to_string(),
                description: None,
            },
            &ctx,
        )
        .await;

        match dispatch(
            Command::MemoryNew {
                name: "patrick".to_string(),
                description: None,
            },
            &ctx,
        )
        .await
        {
            CommandOutcome::Error(msg) => assert!(msg.contains("already exists"), "got {msg}"),
            _ => panic!("expected Error"),
        }
    }

    #[tokio::test]
    async fn memory_list_shows_created_banks() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        match dispatch(Command::MemoryList, &ctx).await {
            CommandOutcome::Text(msg) => assert!(msg.contains("No memory banks"), "got {msg}"),
            _ => panic!("expected Text"),
        }

        for name in ["patrick", "projects"] {
            dispatch(
                Command::MemoryNew {
                    name: name.to_string(),
                    description: None,
                },
                &ctx,
            )
            .await;
        }

        match dispatch(Command::MemoryList, &ctx).await {
            CommandOutcome::Text(msg) => {
                assert!(msg.contains("patrick"), "missing patrick: {msg}");
                assert!(msg.contains("projects"), "missing projects: {msg}");
            }
            _ => panic!("expected Text"),
        }
    }

    #[tokio::test]
    async fn memory_delete_unregisters_but_preserves_db() {
        let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

        dispatch(
            Command::MemoryNew {
                name: "patrick".to_string(),
                description: None,
            },
            &ctx,
        )
        .await;
        let db_id = server
            .memory_bank_index()
            .find_by_name("patrick")
            .await
            .unwrap()
            .unwrap()
            .db_id;

        match dispatch(Command::MemoryDelete("patrick".to_string()), &ctx).await {
            CommandOutcome::Text(msg) => assert!(msg.contains("Deleted"), "got {msg}"),
            _ => panic!("expected Text"),
        }

        // Index row gone.
        assert!(
            server
                .memory_bank_index()
                .find_by_name("patrick")
                .await
                .unwrap()
                .is_none()
        );

        // DB itself is still openable (archive preserved).
        assert!(registry.open_memory_bank(&db_id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn memory_delete_unknown_errors() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        match dispatch(Command::MemoryDelete("ghost".to_string()), &ctx).await {
            CommandOutcome::Error(msg) => {
                assert!(msg.contains("No hosted memory bank"), "got {msg}")
            }
            _ => panic!("expected Error"),
        }
    }

    // -------------------------------------------------------------------------
    // Stage 9.D.2 — grant / revoke
    // -------------------------------------------------------------------------

    /// Helper: create an agent + bank through the command dispatch so the
    /// index + runtime state are consistent. Returns the agent's DB id and
    /// the bank's DB id for assertion convenience.
    async fn provision_agent_and_bank(
        ctx: &CommandContext<'_>,
        agent: &str,
        bank: &str,
    ) -> (eidetica::entry::ID, eidetica::entry::ID) {
        dispatch(
            Command::AgentNew {
                name: agent.to_string(),
                overrides: vec![],
            },
            ctx,
        )
        .await;
        dispatch(
            Command::MemoryNew {
                name: bank.to_string(),
                description: None,
            },
            ctx,
        )
        .await;
        let a_id = ctx
            .server
            .agent_index()
            .find_by_name(agent)
            .await
            .unwrap()
            .unwrap()
            .db_id;
        let b_id = ctx
            .server
            .memory_bank_index()
            .find_by_name(bank)
            .await
            .unwrap()
            .unwrap()
            .db_id;
        (a_id, b_id)
    }

    #[tokio::test]
    async fn memory_grant_writes_auth_and_ref() {
        let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        let (agent_db_id, bank_db_id) = provision_agent_and_bank(&ctx, "alpha", "patrick").await;

        let cmd = Command::MemoryGrant {
            bank_ref: "patrick".to_string(),
            agent_ref: "alpha".to_string(),
            permission: crate::agent_db::BankPermission::Write,
        };
        match dispatch(cmd, &ctx).await {
            CommandOutcome::Text(msg) => {
                assert!(msg.contains("patrick"), "got {msg}");
                assert!(msg.contains("Write"), "got {msg}");
            }
            CommandOutcome::Error(e) => panic!("unexpected: {e}"),
            _ => panic!("expected Text"),
        }

        // Ref mirrored into agent's memory_banks subtree.
        let agent_db = registry.open_agent_db(&agent_db_id).await.unwrap().unwrap();
        let banks = agent_db.list_memory_banks().await.unwrap();
        assert_eq!(banks.len(), 1);
        assert_eq!(banks[0].name, "patrick");
        assert_eq!(banks[0].db_id, bank_db_id.to_string());
        assert_eq!(banks[0].permission, crate::agent_db::BankPermission::Write);
    }

    #[tokio::test]
    async fn memory_revoke_reverses_grant() {
        let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        let (agent_db_id, _bank_db_id) = provision_agent_and_bank(&ctx, "alpha", "patrick").await;

        dispatch(
            Command::MemoryGrant {
                bank_ref: "patrick".to_string(),
                agent_ref: "alpha".to_string(),
                permission: crate::agent_db::BankPermission::Read,
            },
            &ctx,
        )
        .await;
        // Sanity: ref present before revoke.
        let agent_db = registry.open_agent_db(&agent_db_id).await.unwrap().unwrap();
        assert_eq!(agent_db.list_memory_banks().await.unwrap().len(), 1);

        match dispatch(
            Command::MemoryRevoke {
                bank_ref: "patrick".to_string(),
                agent_ref: "alpha".to_string(),
            },
            &ctx,
        )
        .await
        {
            CommandOutcome::Text(msg) => assert!(msg.contains("Revoked"), "got {msg}"),
            CommandOutcome::Error(e) => panic!("unexpected: {e}"),
            _ => panic!("expected Text"),
        }

        // Ref removed from agent's memory_banks.
        let agent_db = registry.open_agent_db(&agent_db_id).await.unwrap().unwrap();
        assert!(agent_db.list_memory_banks().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn memory_share_unknown_bank_errors() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        match dispatch(Command::MemoryShare("ghost".to_string()), &ctx).await {
            CommandOutcome::Error(msg) => {
                assert!(msg.contains("No hosted memory bank"), "got {msg}")
            }
            _ => panic!("expected Error"),
        }
    }

    #[tokio::test]
    async fn memory_import_rejects_invalid_ticket() {
        let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
        let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
        match dispatch(Command::MemoryImport("not-a-ticket".to_string()), &ctx).await {
            CommandOutcome::Error(msg) => {
                // Could be "Sync not enabled" or "Invalid ticket" depending on fixture;
                // either surfaces cleanly as an Error, which is what we want.
                assert!(
                    msg.contains("Invalid ticket") || msg.contains("Sync not enabled"),
                    "got {msg}"
                );
            }
            _ => panic!("expected Error"),
        }
    }

    #[tokio::test]
    async fn memory_grant_unknown_bank_errors() {
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

        match dispatch(
            Command::MemoryGrant {
                bank_ref: "nope".to_string(),
                agent_ref: "alpha".to_string(),
                permission: crate::agent_db::BankPermission::Read,
            },
            &ctx,
        )
        .await
        {
            CommandOutcome::Error(msg) => {
                assert!(msg.contains("No hosted memory bank"), "got {msg}")
            }
            _ => panic!("expected Error"),
        }
    }
}
