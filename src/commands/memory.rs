//! Memory Banks handlers (Memory Banks Stage 9.D).
//!
//! Stage 9.D.1 ships the peer-local CRUD: `/memory new`, `/memory list`,
//! `/memory delete`. Grant/revoke arrive in 9.D.2; share/import in 9.D.3.

use super::{CommandContext, CommandOutcome};

/// Resolve a user-supplied ref — either a bank display name or an
/// eidetica DB ID — to a `MemoryBankIndexEntry`.
pub(super) async fn resolve_bank_ref(
    bank_ref: &str,
    ctx: &CommandContext<'_>,
) -> Result<crate::memory_bank_index::MemoryBankIndexEntry, String> {
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
        .register(crate::memory_bank_index::MemoryBankIndexEntry {
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
    use super::super::{dispatch, Command, CommandContext, CommandOutcome};
    use crate::agent::AgentRegistry;
    use crate::agent_index::AgentIndex;
    use crate::backends::BackendManager;
    use crate::memory_bank_index::MemoryBankIndex;
    use crate::security::SecretStore;
    use crate::server::Server;
    use eidetica::backend::database::InMemory;
    use eidetica::Instance;
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
        let central = registry.central_db().clone();
        let index = AgentIndex::new(central.clone());
        let bank_index = MemoryBankIndex::new(central.clone());
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
        let secrets = SecretStore::new(central).await;
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
        assert!(server
            .memory_bank_index()
            .find_by_name("patrick")
            .await
            .unwrap()
            .is_none());

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
}
