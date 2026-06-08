//! Unit tests for the `/agent` command module. Extracted from `agent.rs`.

use super::super::{Command, CommandContext, CommandOutcome, dispatch};
use crate::agent::AgentRegistry;
use crate::agent_db::find_agent_db;
use crate::backends::BackendManager;
use crate::hosted_index::HostedIndex;
use crate::security::SecretStore;
use crate::server::Server;
use eidetica::backend::database::InMemory;
use eidetica::{Instance, NewUser};
use std::sync::Arc;

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
    let (instance, user) =
        Instance::create_backend(Box::new(backend), NewUser::passwordless("test"))
            .await
            .unwrap();
    let agents = Arc::new(AgentRegistry::with_default_agent());
    let registry = Arc::new(
        crate::session::SessionRegistry::new(instance.clone(), user, agents.clone())
            .await
            .unwrap(),
    );
    let chaz_peer = registry.chaz_peer().clone();
    let index = HostedIndex::empty("agent");
    let bank_index = HostedIndex::empty("bank");
    let tools = Arc::new(crate::tool::ToolRegistry::new());
    let policies = Arc::new(crate::tool::ToolPolicyRegistry::empty());
    let security = crate::security::SecurityContext {
        leak_detector: crate::security::LeakDetector::new(crate::security::LeakPolicy::default()),
        auto_approved_tools: std::collections::HashSet::new(),
        approval_callback: None,
    };
    let secrets = SecretStore::new(chaz_peer).await;
    let backend_mgr = BackendManager::new(&None, secrets.clone());
    let server = Server::new(
        registry.clone(),
        agents,
        index,
        bank_index,
        crate::hosted_index::HostedIndex::empty("skill_bank"),
        tools,
        policies,
        security,
        std::collections::HashMap::new(),
        Default::default(),
        std::sync::Arc::new(crate::tool_host::NativeToolHost::new()),
        std::sync::Arc::new(crate::extension::ExtensionHub::new()),
        backend_mgr.clone(),
        std::sync::Arc::new(crate::mcp::McpRegistry::new()),
    );
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
        secrets,
        backend,
        session_db_id,
        session_db,
        current_agent: "chaz",
        session_name: None,
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
    assert!(server.agent_index().find_by_name("alpha").is_none());
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

// -------------------------------------------------------------------------
// /agent new — extended field coverage (autonomous)
// -------------------------------------------------------------------------

#[tokio::test]
async fn agent_new_accepts_autonomous() {
    let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

    let cmd = Command::AgentNew {
        name: "alpha".to_string(),
        overrides: vec![("autonomous".into(), "true".into())],
    };
    match dispatch(cmd, &ctx).await {
        CommandOutcome::Text(_) => {}
        CommandOutcome::Error(e) => panic!("unexpected error: {e}"),
        _ => panic!("expected Text"),
    }

    let agent = server.agents().get("alpha").unwrap();
    assert!(agent.autonomous);

    // And persisted to the DB.
    let user = registry.user_for_tests().await;
    let (db, _pk) = find_agent_db(&user, "alpha").await.unwrap();
    drop(user);
    let cfg = db.read_config().await.unwrap();
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

    // DB reflects it too — live hydration will read this on next message.
    let user = registry.user_for_tests().await;
    let (db, _pk) = find_agent_db(&user, "alpha").await.unwrap();
    drop(user);
    assert_eq!(
        db.read_config().await.unwrap().model.as_deref(),
        Some("opus")
    );
}

#[tokio::test]
async fn agent_set_system_prompt_refreshes_blob_ref_and_hydrates() {
    // Setting `system_prompt` must store the resolved text in the blob and
    // point `system_prompt_ref` at it, so hydration reflects the edit
    // instead of resolving an empty/stale prompt.
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

    let cmd = Command::AgentSet {
        agent_ref: "alpha".to_string(),
        field: "system_prompt".to_string(),
        value: "You are Alpha.".to_string(),
    };
    match dispatch(cmd, &ctx).await {
        CommandOutcome::Text(_) => {}
        CommandOutcome::Error(e) => panic!("unexpected error: {e}"),
        _ => panic!("expected Text"),
    }

    // DB now carries a prompt ref (the blob pointer), not just inline text.
    let user = registry.user_for_tests().await;
    let (db, _pk) = find_agent_db(&user, "alpha").await.unwrap();
    drop(user);
    let cfg = db.read_config().await.unwrap();
    assert_eq!(cfg.system_prompt, "You are Alpha.");
    assert!(cfg.system_prompt_ref.is_some(), "ref set after prompt edit");

    // And hydration resolves that prompt through the blob.
    let input = crate::agent::Agent {
        name: "alpha".to_string(),
        system_prompt: String::new(),
        system_prompt_files: vec![],
        default_model: None,
        allowed_tools: None,
        workers: std::collections::HashMap::new(),
        max_iterations: 10,
        autonomous: false,
        presets: std::collections::HashMap::new(),
        tool_profile: None,
        max_context_tokens: None,
        grants: std::collections::HashMap::new(),
    };
    let hydrated = server.hydrate_agent_from_db(input).await;
    assert_eq!(hydrated.system_prompt, "You are Alpha.");
}

#[tokio::test]
async fn agent_reload_unknown_agent_errors() {
    let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
    // No config path set on the test server → reload surfaces an error
    // rather than silently succeeding.
    match dispatch(Command::AgentReload(None), &ctx).await {
        CommandOutcome::Error(msg) => assert!(msg.contains("Reload failed"), "got {msg}"),
        _ => panic!("expected Error"),
    }
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

// -------------------------------------------------------------------------
// Co-owned Agents: /pubkey + /agent invite + /agent revoke-peer
// -------------------------------------------------------------------------

use super::super::CoOwnerPermission;

#[tokio::test]
async fn pubkey_returns_peer_default_key() {
    let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
    match dispatch(Command::Pubkey, &ctx).await {
        CommandOutcome::Text(s) => assert!(s.starts_with("ed25519:"), "got {s}"),
        CommandOutcome::Error(e) => panic!("unexpected error: {e}"),
        _ => panic!("expected Text"),
    }
}

async fn fresh_invitee_pubkey(
    registry: &crate::session::SessionRegistry,
) -> eidetica::auth::crypto::PublicKey {
    // Synthesize a second pubkey via the registry's ephemeral-key helper —
    // in real use this is a remote peer's pubkey, but for tests any valid
    // pubkey distinct from our default works.
    registry.new_ephemeral_key("invitee:test").await.unwrap()
}

#[tokio::test]
async fn agent_invite_admin_adds_key_to_agent_db_auth() {
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

    let invitee_pk = fresh_invitee_pubkey(&registry).await;
    let cmd = Command::AgentInvite {
        agent_ref: "alpha".to_string(),
        pubkey: invitee_pk.to_prefixed_string(),
        permission: CoOwnerPermission::Admin,
    };
    match dispatch(cmd, &ctx).await {
        CommandOutcome::Text(msg) => assert!(msg.contains("Invited"), "got {msg}"),
        CommandOutcome::Error(e) => panic!("unexpected error: {e}"),
        _ => panic!("expected Text"),
    }

    let entry = server.agent_index().find_by_name("alpha").unwrap();
    let agent_db = registry
        .open_agent_db(&entry.db_id, Some(&entry.pubkey))
        .await
        .unwrap()
        .unwrap();
    let auth = agent_db
        .database()
        .get_settings()
        .await
        .unwrap()
        .get_auth_key(&invitee_pk)
        .await
        .unwrap();
    assert_eq!(
        auth.permissions(),
        &eidetica::auth::types::Permission::Admin(1)
    );
    assert_eq!(auth.status(), &eidetica::auth::types::KeyStatus::Active);
}

#[tokio::test]
async fn agent_invite_write_permission() {
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
    let invitee_pk = fresh_invitee_pubkey(&registry).await;
    let _ = dispatch(
        Command::AgentInvite {
            agent_ref: "alpha".to_string(),
            pubkey: invitee_pk.to_prefixed_string(),
            permission: CoOwnerPermission::Write,
        },
        &ctx,
    )
    .await;
    let entry = server.agent_index().find_by_name("alpha").unwrap();
    let agent_db = registry
        .open_agent_db(&entry.db_id, Some(&entry.pubkey))
        .await
        .unwrap()
        .unwrap();
    let auth = agent_db
        .database()
        .get_settings()
        .await
        .unwrap()
        .get_auth_key(&invitee_pk)
        .await
        .unwrap();
    assert_eq!(
        auth.permissions(),
        &eidetica::auth::types::Permission::Write(10)
    );
}

#[tokio::test]
async fn agent_invite_read_permission() {
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
    let invitee_pk = fresh_invitee_pubkey(&registry).await;
    let _ = dispatch(
        Command::AgentInvite {
            agent_ref: "alpha".to_string(),
            pubkey: invitee_pk.to_prefixed_string(),
            permission: CoOwnerPermission::Read,
        },
        &ctx,
    )
    .await;
    let entry = server.agent_index().find_by_name("alpha").unwrap();
    let agent_db = registry
        .open_agent_db(&entry.db_id, Some(&entry.pubkey))
        .await
        .unwrap()
        .unwrap();
    let auth = agent_db
        .database()
        .get_settings()
        .await
        .unwrap()
        .get_auth_key(&invitee_pk)
        .await
        .unwrap();
    assert_eq!(auth.permissions(), &eidetica::auth::types::Permission::Read);
}

#[tokio::test]
async fn agent_invite_rejects_own_pubkey() {
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
    let own_pk = server.agent_index().find_by_name("alpha").unwrap().pubkey;
    match dispatch(
        Command::AgentInvite {
            agent_ref: "alpha".to_string(),
            pubkey: own_pk.to_prefixed_string(),
            permission: CoOwnerPermission::Admin,
        },
        &ctx,
    )
    .await
    {
        CommandOutcome::Error(msg) => assert!(msg.contains("already own"), "got {msg}"),
        _ => panic!("expected Error"),
    }
}

#[tokio::test]
async fn agent_invite_rejects_malformed_pubkey() {
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
        Command::AgentInvite {
            agent_ref: "alpha".to_string(),
            pubkey: "not a pubkey".to_string(),
            permission: CoOwnerPermission::Admin,
        },
        &ctx,
    )
    .await
    {
        CommandOutcome::Error(msg) => assert!(msg.contains("Invalid pubkey"), "got {msg}"),
        _ => panic!("expected Error"),
    }
}

#[tokio::test]
async fn agent_invite_unknown_agent_errors() {
    let (_i, server, _r, secrets, backend, sid, sdb) = fixture().await;
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
    match dispatch(
        Command::AgentInvite {
            agent_ref: "ghost".to_string(),
            pubkey: "ed25519:AAAA".to_string(),
            permission: CoOwnerPermission::Admin,
        },
        &ctx,
    )
    .await
    {
        CommandOutcome::Error(msg) => assert!(msg.contains("No hosted agent"), "got {msg}"),
        _ => panic!("expected Error"),
    }
}

#[tokio::test]
async fn agent_revoke_peer_removes_key() {
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
    let invitee_pk = fresh_invitee_pubkey(&registry).await;
    dispatch(
        Command::AgentInvite {
            agent_ref: "alpha".to_string(),
            pubkey: invitee_pk.to_prefixed_string(),
            permission: CoOwnerPermission::Admin,
        },
        &ctx,
    )
    .await;

    match dispatch(
        Command::AgentRevokePeer {
            agent_ref: "alpha".to_string(),
            pubkey: invitee_pk.to_prefixed_string(),
        },
        &ctx,
    )
    .await
    {
        CommandOutcome::Text(msg) => assert!(msg.contains("Revoked"), "got {msg}"),
        CommandOutcome::Error(e) => panic!("unexpected error: {e}"),
        _ => panic!("expected Text"),
    }

    let entry = server.agent_index().find_by_name("alpha").unwrap();
    let agent_db = registry
        .open_agent_db(&entry.db_id, Some(&entry.pubkey))
        .await
        .unwrap()
        .unwrap();
    let auth_after = agent_db
        .database()
        .get_settings()
        .await
        .unwrap()
        .get_auth_key(&invitee_pk)
        .await
        .unwrap();
    assert_ne!(
        auth_after.status(),
        &eidetica::auth::types::KeyStatus::Active
    );
}

#[tokio::test]
async fn agent_revoke_peer_refuses_own_key() {
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
    let own_pk = server.agent_index().find_by_name("alpha").unwrap().pubkey;
    match dispatch(
        Command::AgentRevokePeer {
            agent_ref: "alpha".to_string(),
            pubkey: own_pk.to_prefixed_string(),
        },
        &ctx,
    )
    .await
    {
        CommandOutcome::Error(msg) => {
            assert!(msg.contains("/agent delete"), "got {msg}")
        }
        _ => panic!("expected Error"),
    }
}

// ---- /agent rehost ---------------------------------------------------

/// Set up an agent attached to the session and return its DbEntry.
async fn setup_attached_agent(
    server: &std::sync::Arc<Server>,
    registry: &crate::session::SessionRegistry,
    sid: &str,
    ctx: &CommandContext<'_>,
    name: &str,
) -> crate::hosted_index::DbEntry {
    dispatch(
        Command::AgentNew {
            name: name.to_string(),
            overrides: vec![],
        },
        ctx,
    )
    .await;
    let entry = server.agent_index().find_by_name(name).unwrap();
    registry.attach_agent_to_session(sid, &entry).await.unwrap();
    entry
}

#[tokio::test]
async fn rehost_session_defaults_to_self_pubkey() {
    let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
    let entry = setup_attached_agent(&server, &registry, &sid, &ctx, "alpha").await;

    // Pre-condition: attach defaulted home to this peer's pubkey on the agent.
    // Rewrite it to something else so we can prove rehost-to-self changes it back.
    let other = registry.new_ephemeral_key("other").await.unwrap();
    crate::session::update_meta_on_db(&sdb, |m| {
        m.agents[0].home_pubkey = Some(other.to_string());
    })
    .await
    .unwrap();

    match dispatch(
        Command::AgentRehost {
            agent_ref: "alpha".to_string(),
            pubkey: None,
            scope: super::super::RehostScope::Session,
            clear: false,
        },
        &ctx,
    )
    .await
    {
        CommandOutcome::Text(_) => {}
        _ => panic!("expected Text"),
    }

    let meta = crate::session::read_meta_from_db(&sdb).await;
    assert_eq!(
        meta.agents[0].home_pubkey.as_deref(),
        Some(entry.pubkey.to_string()).as_deref()
    );
}

#[tokio::test]
async fn rehost_session_to_explicit_authorized_pubkey() {
    let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
    let _entry = setup_attached_agent(&server, &registry, &sid, &ctx, "alpha").await;

    let invitee = fresh_invitee_pubkey(&registry).await;
    // Invite the target peer's key so it's authorized on the agent DB.
    dispatch(
        Command::AgentInvite {
            agent_ref: "alpha".to_string(),
            pubkey: invitee.to_prefixed_string(),
            permission: CoOwnerPermission::Admin,
        },
        &ctx,
    )
    .await;

    match dispatch(
        Command::AgentRehost {
            agent_ref: "alpha".to_string(),
            pubkey: Some(invitee.to_prefixed_string()),
            scope: super::super::RehostScope::Session,
            clear: false,
        },
        &ctx,
    )
    .await
    {
        CommandOutcome::Text(msg) => assert!(msg.contains("Set session-level"), "got {msg}"),
        _ => panic!("expected Text"),
    }

    let meta = crate::session::read_meta_from_db(&sdb).await;
    assert_eq!(
        meta.agents[0].home_pubkey.as_deref(),
        Some(invitee.to_string()).as_deref()
    );
}

#[tokio::test]
async fn rehost_refuses_unauthorized_target_pubkey() {
    let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
    let _entry = setup_attached_agent(&server, &registry, &sid, &ctx, "alpha").await;

    let stranger = fresh_invitee_pubkey(&registry).await;
    // Note: NOT invited — stranger has no key on the agent DB.

    match dispatch(
        Command::AgentRehost {
            agent_ref: "alpha".to_string(),
            pubkey: Some(stranger.to_prefixed_string()),
            scope: super::super::RehostScope::Session,
            clear: false,
        },
        &ctx,
    )
    .await
    {
        CommandOutcome::Error(msg) => {
            assert!(msg.contains("not authorized"), "got {msg}")
        }
        _ => panic!("expected Error"),
    }
}

#[tokio::test]
async fn rehost_agent_level_writes_meta_home_pubkey() {
    let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
    let entry = setup_attached_agent(&server, &registry, &sid, &ctx, "alpha").await;

    let invitee = fresh_invitee_pubkey(&registry).await;
    dispatch(
        Command::AgentInvite {
            agent_ref: "alpha".to_string(),
            pubkey: invitee.to_prefixed_string(),
            permission: CoOwnerPermission::Admin,
        },
        &ctx,
    )
    .await;

    match dispatch(
        Command::AgentRehost {
            agent_ref: "alpha".to_string(),
            pubkey: Some(invitee.to_prefixed_string()),
            scope: super::super::RehostScope::Agent,
            clear: false,
        },
        &ctx,
    )
    .await
    {
        CommandOutcome::Text(msg) => assert!(msg.contains("Set agent-level"), "got {msg}"),
        _ => panic!("expected Text"),
    }

    let agent_db = registry
        .open_agent_db(&entry.db_id, Some(&entry.pubkey))
        .await
        .unwrap()
        .unwrap();
    let home = crate::db_kind::read_agent_home_pubkey(agent_db.database()).await;
    assert_eq!(home, Some(invitee));
}

#[tokio::test]
async fn rehost_clear_session_resets_home_to_none() {
    let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
    let _entry = setup_attached_agent(&server, &registry, &sid, &ctx, "alpha").await;

    // Pre-condition: attach defaulted home to self. Clear it.
    match dispatch(
        Command::AgentRehost {
            agent_ref: "alpha".to_string(),
            pubkey: None,
            scope: super::super::RehostScope::Session,
            clear: true,
        },
        &ctx,
    )
    .await
    {
        CommandOutcome::Text(msg) => {
            assert!(
                msg.contains("Cleared") && msg.contains("WARNING"),
                "got {msg}"
            )
        }
        _ => panic!("expected Text"),
    }

    let meta = crate::session::read_meta_from_db(&sdb).await;
    assert_eq!(meta.agents[0].home_pubkey, None);
}

#[tokio::test]
async fn rehost_clear_agent_resets_agent_meta_home_to_none() {
    let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
    let entry = setup_attached_agent(&server, &registry, &sid, &ctx, "alpha").await;

    // Pre-condition: agent_db was created with home = creator pubkey.
    let agent_db = registry
        .open_agent_db(&entry.db_id, Some(&entry.pubkey))
        .await
        .unwrap()
        .unwrap();
    assert!(
        crate::db_kind::read_agent_home_pubkey(agent_db.database())
            .await
            .is_some()
    );

    match dispatch(
        Command::AgentRehost {
            agent_ref: "alpha".to_string(),
            pubkey: None,
            scope: super::super::RehostScope::Agent,
            clear: true,
        },
        &ctx,
    )
    .await
    {
        CommandOutcome::Text(msg) => assert!(msg.contains("Cleared"), "got {msg}"),
        _ => panic!("expected Text"),
    }

    let agent_db = registry
        .open_agent_db(&entry.db_id, Some(&entry.pubkey))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        crate::db_kind::read_agent_home_pubkey(agent_db.database()).await,
        None
    );
}

// ---- /agent home-status ---------------------------------------------

#[tokio::test]
async fn home_status_lists_all_locally_hosted_agents() {
    let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
    setup_attached_agent(&server, &registry, &sid, &ctx, "alpha").await;
    setup_attached_agent(&server, &registry, &sid, &ctx, "beta").await;

    match dispatch(Command::AgentHomeStatus(None), &ctx).await {
        CommandOutcome::Text(out) => {
            assert!(out.contains("agent: alpha"), "missing alpha: {out}");
            assert!(out.contains("agent: beta"), "missing beta: {out}");
        }
        _ => panic!("expected Text"),
    }
}

#[tokio::test]
async fn home_status_marks_self_with_me_tag() {
    let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
    setup_attached_agent(&server, &registry, &sid, &ctx, "alpha").await;

    match dispatch(Command::AgentHomeStatus(Some("alpha".to_string())), &ctx).await {
        CommandOutcome::Text(out) => {
            assert!(out.contains("← (me)"), "expected ← (me) tag: {out}");
        }
        _ => panic!("expected Text"),
    }
}

#[tokio::test]
async fn home_status_handles_unset_session_home() {
    let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
    let _entry = setup_attached_agent(&server, &registry, &sid, &ctx, "alpha").await;

    // Clear the auto-set per-session home so it shows as legacy.
    crate::session::update_meta_on_db(&sdb, |m| {
        m.agents[0].home_pubkey = None;
    })
    .await
    .unwrap();

    match dispatch(Command::AgentHomeStatus(Some("alpha".to_string())), &ctx).await {
        CommandOutcome::Text(out) => {
            assert!(out.contains("<unset"), "expected <unset> marker: {out}");
        }
        _ => panic!("expected Text"),
    }
}

// ---- skip-counter WARN -----------------------------------------------

#[tokio::test]
async fn home_skip_counter_increments_on_record() {
    let (_i, server, _registry, _secrets, _backend, sid, _sdb) = fixture().await;
    assert_eq!(server.home_skip_count(&sid, "alpha").await, 0);
    server.record_home_skip(&sid, "alpha").await;
    server.record_home_skip(&sid, "alpha").await;
    assert_eq!(server.home_skip_count(&sid, "alpha").await, 2);
}

#[tokio::test]
async fn home_skip_counter_resets_on_run() {
    let (_i, server, _registry, _secrets, _backend, sid, _sdb) = fixture().await;
    server.record_home_skip(&sid, "alpha").await;
    server.record_home_skip(&sid, "alpha").await;
    server.reset_home_skip(&sid, "alpha").await;
    assert_eq!(server.home_skip_count(&sid, "alpha").await, 0);
}

#[tokio::test]
async fn revoke_warns_when_target_was_session_home() {
    let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
    let _entry = setup_attached_agent(&server, &registry, &sid, &ctx, "alpha").await;

    // Invite a co-owner and rehost the session to their key.
    let invitee = fresh_invitee_pubkey(&registry).await;
    dispatch(
        Command::AgentInvite {
            agent_ref: "alpha".to_string(),
            pubkey: invitee.to_prefixed_string(),
            permission: CoOwnerPermission::Admin,
        },
        &ctx,
    )
    .await;
    dispatch(
        Command::AgentRehost {
            agent_ref: "alpha".to_string(),
            pubkey: Some(invitee.to_prefixed_string()),
            scope: super::super::RehostScope::Session,
            clear: false,
        },
        &ctx,
    )
    .await;

    // Revoke the co-owner. Soft warning should mention this session.
    match dispatch(
        Command::AgentRevokePeer {
            agent_ref: "alpha".to_string(),
            pubkey: invitee.to_prefixed_string(),
        },
        &ctx,
    )
    .await
    {
        CommandOutcome::Text(msg) => {
            assert!(msg.contains("Revoked"), "no revoke confirmation: {msg}");
            assert!(
                msg.contains("WARNING") && msg.contains(&sid),
                "missing session warning: {msg}"
            );
        }
        _ => panic!("expected Text"),
    }
}

#[tokio::test]
async fn home_skip_counter_resets_on_rehost() {
    let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
    let _entry = setup_attached_agent(&server, &registry, &sid, &ctx, "alpha").await;

    server.record_home_skip(&sid, "alpha").await;
    server.record_home_skip(&sid, "alpha").await;
    assert_eq!(server.home_skip_count(&sid, "alpha").await, 2);

    dispatch(
        Command::AgentRehost {
            agent_ref: "alpha".to_string(),
            pubkey: None,
            scope: super::super::RehostScope::Session,
            clear: false,
        },
        &ctx,
    )
    .await;

    assert_eq!(server.home_skip_count(&sid, "alpha").await, 0);
}
