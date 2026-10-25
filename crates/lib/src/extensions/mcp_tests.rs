use super::*;
use crate::extension::instance::PeerHandles;
use crate::hosted_index::HostedIndex;
use crate::mcp::{McpServerStatus, test_support::Fixture};
use std::sync::atomic::{AtomicBool, Ordering};

async fn peer() -> PeerHandles {
    let (_, registry) = crate::test_support::fresh_session_registry().await;
    PeerHandles {
        registry,
        agent_index: HostedIndex::empty("agent"),
        memory_bank_index: HostedIndex::empty("bank"),
        skill_bank_index: HostedIndex::empty("skill_bank"),
        embedder: None,
        secrets: None,
        server_slot: Default::default(),
        mcp_registry: Arc::new(McpRegistry::new()),
        agent_state_allowlist: Default::default(),
        tool_registry: Arc::new(ToolRegistry::new()),
    }
}

async fn install(peer: &PeerHandles, f: &Fixture) {
    McpExtension::new(f.config.clone())
        .instantiate(ScopeCtx::Global { peer })
        .await
        .unwrap();
}

#[tokio::test]
async fn initialize_and_discovery_timeouts_explicitly_reap_before_settling() {
    for (mode, marker) in [("hang-init", "initialize"), ("hang-discovery", "discovery")] {
        let peer = peer().await;
        let f = Fixture::new(mode);
        install(&peer, &f).await;
        f.wait(marker).await;
        assert_eq!(peer.mcp_registry.pending_count(), 1);
        tokio::time::timeout(Duration::from_secs(5), peer.mcp_registry.wait_ready())
            .await
            .unwrap();
        let snapshot = peer.mcp_registry.snapshot();
        assert!(
            matches!(&snapshot[0].status, McpServerStatus::Failed { error } if error.contains("did not start within"))
        );
        assert!(f.has("eof"));
        assert!(peer.tool_registry.is_empty());
        assert!(
            peer.tool_registry
                .pending_source_for("fixture__echo")
                .is_none()
        );
        peer.mcp_registry.shutdown().await.unwrap();
        assert!(peer.mcp_registry.snapshot().is_empty());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn close_joins_initializing_task_before_return_including_unpolled_startup() {
    for polled in [false, true] {
        let peer = peer().await;
        let f = Fixture::new("hang-init");
        install(&peer, &f).await;
        if polled {
            f.wait("initialize").await;
        }
        peer.mcp_registry.shutdown().await.unwrap();
        assert!(peer.mcp_registry.snapshot().is_empty());
        assert!(
            peer.tool_registry
                .pending_source_for("fixture__echo")
                .is_none()
        );
        assert!(peer.tool_registry.is_empty());
        assert!(f.has("eof"));
        assert_eq!(f.spawns(), 1);
    }
}

#[tokio::test]
async fn publication_gate_rejects_late_tools_and_new_servers_after_close() {
    let peer = peer().await;
    let f = Fixture::new("eof");
    let server = Arc::new(McpServer::start(&f.config).unwrap());
    server.initialize().await.unwrap();
    let tools = server
        .discover_and_wrap_tools("fixture")
        .await
        .unwrap()
        .into_iter()
        .map(|t| Arc::new(t) as Arc<dyn Tool>)
        .collect();
    peer.mcp_registry
        .insert_running("fixture".into(), server.clone(), None);
    peer.mcp_registry.begin_close().unwrap();
    assert!(!peer.mcp_registry.publish(
        "fixture",
        server.clone(),
        None,
        tools,
        &peer.tool_registry,
        "mcp-fixture"
    ));
    assert!(peer.tool_registry.is_empty());
    let called = AtomicBool::new(false);
    assert!(!peer.mcp_registry.launch("late".into(), || {
        called.store(true, Ordering::SeqCst);
        unreachable!()
    }));
    install(&peer, &f).await;
    assert!(!called.load(Ordering::SeqCst));
    assert_eq!(f.spawns(), 1);
    peer.mcp_registry.shutdown().await.unwrap();
    assert!(
        server
            .call_tool("echo", serde_json::json!({}))
            .await
            .unwrap_err()
            .contains("closed")
    );
    assert_eq!(f.spawns(), 1);
}

#[tokio::test]
async fn duplicate_instantiation_still_spawns_once_with_original_tool_identity() {
    let peer = peer().await;
    let f = Fixture::new("eof");
    install(&peer, &f).await;
    install(&peer, &f).await;
    peer.mcp_registry.wait_ready().await;
    let tool = peer.tool_registry.get("fixture__echo").unwrap();
    assert_eq!(tool.descriptor().name, "fixture__echo");
    assert_eq!(
        peer.tool_registry.owner_of("fixture__echo"),
        Some("mcp-fixture")
    );
    assert_eq!(f.spawns(), 1);
    let server = match &peer.mcp_registry.snapshot()[0].status {
        McpServerStatus::Running { server, .. } => server.clone(),
        _ => panic!("fixture did not run"),
    };
    assert_eq!(
        server
            .call_tool("echo", serde_json::json!({}))
            .await
            .unwrap(),
        "ok"
    );
    peer.mcp_registry.shutdown().await.unwrap();
    assert!(
        server
            .call_tool("echo", serde_json::json!({}))
            .await
            .is_err()
    );
    assert_eq!(f.spawns(), 1);
}

#[tokio::test]
async fn incomplete_and_cancelled_registry_close_retain_owners_for_retry() {
    let peer = peer().await;
    let f = Fixture::new("term");
    install(&peer, &f).await;
    peer.mcp_registry.wait_ready().await;
    let server = match &peer.mcp_registry.snapshot()[0].status {
        McpServerStatus::Running { server, .. } => server.clone(),
        _ => panic!("fixture did not run"),
    };
    let pid = server.process_id().await;
    server.inject_cleanup_failure(1);
    assert!(
        peer.mcp_registry
            .shutdown()
            .await
            .unwrap_err()
            .contains("incomplete")
    );
    assert_eq!(peer.mcp_registry.snapshot().len(), 1);
    assert_eq!(server.process_id().await, pid);
    install(&peer, &f).await;
    assert_eq!(f.spawns(), 1);
    server.inject_cleanup_failure(0);
    {
        let close = peer.mcp_registry.shutdown();
        tokio::pin!(close);
        assert!(futures::poll!(&mut close).is_pending());
    }
    assert_eq!(peer.mcp_registry.snapshot().len(), 1);
    peer.mcp_registry.shutdown().await.unwrap();
    assert!(peer.mcp_registry.snapshot().is_empty());
    assert!(server.process_id().await.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn sequential_servers_have_per_child_budgets_not_one_peer_deadline() {
    let peer = peer().await;
    let a = Fixture::new("term");
    let mut b = Fixture::new("term");
    b.config.name = "second".into();
    install(&peer, &a).await;
    install(&peer, &b).await;
    peer.mcp_registry.wait_ready().await;
    let start = tokio::time::Instant::now();
    peer.mcp_registry.shutdown().await.unwrap();
    assert!(start.elapsed() >= Duration::from_secs(4));
    assert!(start.elapsed() < Duration::from_secs(6));
    assert!(a.has("eof") && a.has("term"));
    assert!(b.has("eof") && b.has("term"));
}
