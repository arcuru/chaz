//! Unit tests for the extension framework. Extracted from `mod.rs`.

use super::test_doubles::*;
use super::*;
use crate::runtime::RuntimeMessage;
use crate::tool::ToolRegistry;
use crate::types::ConversationId;

#[path = "tool_call_failure_tests.rs"]
mod tool_call_failure_tests;

#[test]
fn builtin_ref_carries_binary_version() {
    let r = ExtensionRef::builtin("heartbeat");
    match &r {
        ExtensionRef::Builtin { name, chaz_version } => {
            assert_eq!(name, "heartbeat");
            assert_eq!(chaz_version, env!("CARGO_PKG_VERSION"));
        }
        other => panic!("expected Builtin, got {other:?}"),
    }
    assert_eq!(r.name(), "heartbeat");
    assert_eq!(r.version(), env!("CARGO_PKG_VERSION"));
}

#[test]
fn name_and_version_accessors_cover_every_variant() {
    let cases = [
        (
            ExtensionRef::Builtin {
                name: "a".into(),
                chaz_version: "0.1.0".into(),
            },
            "a",
            "0.1.0",
        ),
        (
            ExtensionRef::Eidetica {
                name: "b".into(),
                db_id: "db".into(),
                version: "v1".into(),
            },
            "b",
            "v1",
        ),
        (
            ExtensionRef::Ipld {
                name: "c".into(),
                cid: "bafy...".into(),
            },
            "c",
            "bafy...",
        ),
        (
            ExtensionRef::Git {
                name: "d".into(),
                repo: "https://example.com/r".into(),
                sha: "deadbeef".into(),
            },
            "d",
            "deadbeef",
        ),
    ];
    for (r, expected_name, expected_version) in &cases {
        assert_eq!(r.name(), *expected_name);
        assert_eq!(r.version(), *expected_version);
    }
}

#[test]
fn extension_ref_serde_round_trips_with_tag() {
    let original = ExtensionRef::Git {
        name: "loop_detector".into(),
        repo: "https://github.com/x/y".into(),
        sha: "abc123".into(),
    };
    let json = serde_json::to_string(&original).unwrap();
    // `#[serde(tag = "kind")]` produces a flat, discoverable shape.
    assert!(json.contains("\"kind\":\"git\""), "got: {json}");
    let parsed: ExtensionRef = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, original);
}

#[tokio::test]
async fn context_augmentation_order_is_stable_and_filtered() {
    let mut hub = test_hub().await;
    let names = ["zulu", "echo", "alpha", "delta", "bravo", "charlie"];
    let mut extensions: Vec<Arc<dyn Extension>> = names
        .iter()
        .map(|&name| {
            let scopes = if name == "echo" {
                vec![instance::Scope::PerAgent, instance::Scope::PerSession]
            } else {
                vec![instance::Scope::PerSession]
            };
            Arc::new(
                TestExt::new(name)
                    .scopes(scopes)
                    .prompt_augmentation(Arc::new(FixedAug(name)))
                    .context_tail(Arc::new(FixedAug(name))),
            ) as Arc<dyn Extension>
        })
        .collect();
    extensions.push(Arc::new(
        TestExt::new("global")
            .prompt_augmentation(Arc::new(FixedAug("GLOBAL")))
            .context_tail(Arc::new(FixedAug("GLOBAL"))),
    ));
    extensions.push(Arc::new(
        TestExt::new("foxtrot")
            .scopes(vec![instance::Scope::PerAgent])
            .prompt_augmentation(Arc::new(FixedAug("foxtrot")))
            .context_tail(Arc::new(FixedAug("foxtrot"))),
    ));
    hub.install_all(extensions).await.unwrap();

    // Wire the real agent-name lookup, then seed distinguishable cached
    // agent output for the same-named session provider to override.
    let peer = hub.peer_handles.as_ref().unwrap().clone();
    let registry = peer.registry.clone();
    let agent = crate::agent_db::ensure_agent_db(&mut *registry.user_for_tests().await, "chaz")
        .await
        .unwrap();
    peer.agent_index.register(crate::hosted_index::DbEntry {
        db_id: agent.db.id(),
        display_name: "chaz".into(),
        pubkey: agent.pubkey,
    });
    hub.ensure_agent_instances("chaz", agent.db.database())
        .await;
    hub.agent_instances.write().await.insert(
        (agent.db.id(), "echo".into()),
        Arc::new(TestInstance {
            manifest: TestExt::new("echo")
                .context_tail(Arc::new(FixedAug("AGENT-ECHO")))
                .manifest(),
            parts: TestParts {
                prompt_augmentation: Some(Arc::new(FixedAug("AGENT-ECHO"))),
                context_tail: Some(Arc::new(FixedAug("AGENT-ECHO"))),
                ..Default::default()
            },
        }),
    );
    let hub = Arc::new(hub);
    let secrets = crate::security::SecretStore::new(registry.chaz_peer().clone()).await;
    let server = crate::server::Server::new(
        registry.clone(),
        registry.agents.clone(),
        peer.agent_index.clone(),
        peer.memory_bank_index.clone(),
        peer.skill_bank_index.clone(),
        peer.tool_registry.clone(),
        Arc::new(crate::tool::ToolPolicyRegistry::empty()),
        crate::test_support::permissive_security(),
        Default::default(),
        Default::default(),
        Arc::new(crate::tool_host::NativeToolHost::new()),
        hub.clone(),
        crate::backends::BackendManager::new(&None, secrets),
        peer.mcp_registry.clone(),
        None,
    );
    peer.server_slot.set(server.clone());
    assert_eq!(
        hub.augment_system_prompt("chaz", &[], &[], None, None)
            .await,
        "AGENT-ECHO\n\nfoxtrot"
    );
    assert_eq!(
        hub.context_tails("chaz", &[], None, None).await,
        "AGENT-ECHO\n\nfoxtrot"
    );

    let (_instance, db) = make_session_db().await;
    hub.record_active(&db).await.unwrap();
    let expected = "alpha\n\nbravo\n\ncharlie\n\ndelta\n\necho\n\nfoxtrot\n\nzulu";

    // Each collection builds a fresh name map; cache hits must preserve the
    // same model-facing bytes as the initial lazy instantiation.
    for _ in 0..8 {
        assert_eq!(
            hub.augment_system_prompt("chaz", &[], &[], None, Some(&db))
                .await,
            expected
        );
        assert_eq!(
            hub.context_tails("chaz", &[], None, Some(&db)).await,
            expected
        );
    }
    let active = vec!["zulu".to_string(), "bravo".to_string()];
    assert_eq!(
        hub.augment_system_prompt("chaz", &[], &[], Some(&active), Some(&db))
            .await,
        "bravo\n\nzulu"
    );
    assert_eq!(
        hub.context_tails("chaz", &[], Some(&active), Some(&db))
            .await,
        "bravo\n\nzulu"
    );
    peer.server_slot.clear();
    server.shutdown().await;
}

#[tokio::test]
async fn ensure_agent_instances_instantiates_per_agent_extensions() {
    let mut hub = test_hub().await;
    hub.install_all(vec![Arc::new(
        TestExt::new("per_agent_ext")
            .scopes(vec![instance::Scope::PerAgent])
            .prompt_augmentation(Arc::new(FixedAug("AGENT-AUG"))),
    )])
    .await
    .unwrap();

    // A PerAgent-only extension is not drained as a Global instance.
    assert!(hub.global_instances.is_empty());

    let (_inst, agent_db) = make_session_db().await;
    let got = hub.ensure_agent_instances("alpha", &agent_db).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].manifest().name, "per_agent_ext");
    assert!(got[0].prompt_augmentation().is_some());

    // Idempotent: a second call returns the same cached instance.
    let again = hub.ensure_agent_instances("alpha", &agent_db).await;
    assert_eq!(again.len(), 1);
    assert!(Arc::ptr_eq(&got[0], &again[0]));
}

#[tokio::test]
async fn ensure_agent_instances_keyed_per_agent_db() {
    let mut hub = test_hub().await;
    hub.install_all(vec![Arc::new(
        TestExt::new("per_agent_ext").scopes(vec![instance::Scope::PerAgent]),
    )])
    .await
    .unwrap();

    let (_i1, db_a) = make_session_db().await;
    let (_i2, db_b) = make_session_db().await;
    let a = hub.ensure_agent_instances("alpha", &db_a).await;
    let b = hub.ensure_agent_instances("beta", &db_b).await;
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);
    // Distinct agent DBs get distinct instances.
    assert!(!Arc::ptr_eq(&a[0], &b[0]));
    assert_eq!(hub.agent_instances.read().await.len(), 2);
}

#[tokio::test]
async fn ensure_agent_instances_noop_without_per_agent_scope() {
    // A Global-only extension yields no per-agent instances.
    let mut hub = test_hub().await;
    hub.install_all(vec![Arc::new(TestExt::new("global_ext"))])
        .await
        .unwrap();
    let (_inst, agent_db) = make_session_db().await;
    assert!(
        hub.ensure_agent_instances("alpha", &agent_db)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn settings_missing_key_returns_empty_object() {
    let (_inst, db) = make_session_db().await;
    let got = read_settings(&db, "memory").await;
    assert_eq!(got, serde_json::json!({}));
}

#[tokio::test]
async fn settings_round_trip_via_helpers() {
    let (_inst, db) = make_session_db().await;
    let value = serde_json::json!({
        "max_results": 8,
        "embedder": "nomic"
    });
    write_settings(&db, "memory", value.clone()).await.unwrap();
    let got = read_settings(&db, "memory").await;
    assert_eq!(got, value);
}

#[tokio::test]
async fn settings_for_two_extensions_dont_collide() {
    let (_inst, db) = make_session_db().await;
    write_settings(&db, "memory", serde_json::json!({"k": 1}))
        .await
        .unwrap();
    write_settings(&db, "heartbeat", serde_json::json!({"k": 2}))
        .await
        .unwrap();
    assert_eq!(
        read_settings(&db, "memory").await,
        serde_json::json!({"k": 1})
    );
    assert_eq!(
        read_settings(&db, "heartbeat").await,
        serde_json::json!({"k": 2})
    );
}

#[tokio::test]
async fn settings_overwrite_replaces_prior_value() {
    let (_inst, db) = make_session_db().await;
    write_settings(&db, "x", serde_json::json!({"a": 1}))
        .await
        .unwrap();
    write_settings(&db, "x", serde_json::json!({"b": 2}))
        .await
        .unwrap();
    assert_eq!(read_settings(&db, "x").await, serde_json::json!({"b": 2}));
}

fn status_doc(key: &str, text: &str) -> ExtensionOutput {
    ExtensionOutput {
        status: BTreeMap::from([(key.to_string(), text.to_string())]),
    }
}

#[tokio::test]
async fn extension_outputs_round_trip_and_idempotent_write() {
    let (_inst, db) = make_session_db().await;
    let mut fresh = BTreeMap::new();
    fresh.insert("core".to_string(), status_doc("mcp", "3 mcp"));

    write_extension_outputs(&db, &fresh).await.unwrap();
    assert_eq!(read_extension_outputs(&db).await, fresh);

    // Write-on-change: rewriting the identical map is a no-op and the
    // store still reads back the same value.
    write_extension_outputs(&db, &fresh).await.unwrap();
    assert_eq!(read_extension_outputs(&db).await, fresh);
}

#[tokio::test]
async fn extension_outputs_drops_vanished_extension() {
    let (_inst, db) = make_session_db().await;
    let mut two = BTreeMap::new();
    two.insert("core".to_string(), status_doc("mcp", "2 mcp"));
    two.insert("memory".to_string(), status_doc("recall", "on"));
    write_extension_outputs(&db, &two).await.unwrap();
    assert_eq!(read_extension_outputs(&db).await.len(), 2);

    // Re-collect with only one provider: the other's doc must be dropped,
    // not left stale in the store.
    let mut one = BTreeMap::new();
    one.insert("core".to_string(), status_doc("mcp", "2 mcp"));
    write_extension_outputs(&db, &one).await.unwrap();
    assert_eq!(read_extension_outputs(&db).await, one);
}

#[tokio::test]
async fn status_outputs_collects_from_global_instance() {
    // Regression: status providers are Global-scoped (e.g. `core`), so the
    // collector must walk global instances, not just agent/session ones.
    let mut hub = test_hub().await;
    hub.install_all(vec![Arc::new(
        TestExt::new("statusy").status_segment(Arc::new(FixedStatus("mode", "online"))),
    )])
    .await
    .unwrap();

    let (_inst, db) = make_session_db().await;
    hub.refresh_status_outputs("chaz", &db).await;

    let outputs = read_extension_outputs(&db).await;
    assert_eq!(
        outputs.get("statusy").map(|o| &o.status),
        Some(&BTreeMap::from([(
            "mode".to_string(),
            "online".to_string()
        )])),
    );
}

#[tokio::test]
async fn hook_context_settings_round_trip() {
    // Build the ctx inline so the `Instance` stays alive for the
    // duration of the read/write calls — `fixture_ctx` drops it
    // before returning, which is fine for fire_* tests that don't
    // touch the DB but not for settings ops.
    let (_inst, db) = make_session_db().await;
    let session = Session::new(ConversationId("conv".into()), db).await;
    let ctx = HookContext {
        agent_name: "test_agent".into(),
        model: None,
        call_depth: 0,
        session: Arc::new(Mutex::new(session)),
        active_extensions: HashSet::new(),
        routine_engine: None,
    };
    ctx.set_settings("heartbeat", serde_json::json!({"poll_secs": 60}))
        .await
        .unwrap();
    let got = ctx.get_settings("heartbeat").await;
    assert_eq!(got, serde_json::json!({"poll_secs": 60}));
}

#[tokio::test]
async fn read_active_on_empty_db_returns_empty() {
    let (_inst, db) = make_session_db().await;
    let active = read_active(&db).await.unwrap();
    assert!(active.is_empty());
}

#[tokio::test]
async fn record_active_writes_events_for_each_extension() {
    let (_inst, db) = make_session_db().await;
    let mut hub = ExtensionHub::new();
    hub.install_all(vec![
        Arc::new(NamedExt("alpha")),
        Arc::new(NamedExt("beta")),
    ])
    .await
    .unwrap();

    hub.record_active(&db).await.unwrap();

    let events = list_events(&db).await.unwrap();
    assert_eq!(events.len(), 2);
    let names: std::collections::HashSet<_> = events.iter().map(|e| e.name()).collect();
    assert!(names.contains("alpha"));
    assert!(names.contains("beta"));
    for e in &events {
        assert!(matches!(e, ExtensionEvent::Activated { .. }));
    }
}

#[tokio::test]
async fn record_active_is_idempotent_when_set_unchanged() {
    let (_inst, db) = make_session_db().await;
    let mut hub = ExtensionHub::new();
    hub.install_all(vec![Arc::new(NamedExt("alpha"))])
        .await
        .unwrap();

    hub.record_active(&db).await.unwrap();
    let after_first = list_events(&db).await.unwrap().len();
    assert_eq!(after_first, 1);

    // Second call with no changes must not append a duplicate.
    hub.record_active(&db).await.unwrap();
    let after_second = list_events(&db).await.unwrap().len();
    assert_eq!(after_second, 1);
}

#[tokio::test]
async fn record_active_respects_deactivation_across_restarts() {
    // `record_active` is the session_start reconciler. The old behavior
    // re-activated anything currently in the hub regardless of prior
    // Deactivated events — which would have undone every
    // `/extensions remove X` on the next session_start. The new
    // contract: respect explicit removal across restarts.
    let (_inst, db) = make_session_db().await;
    let mut hub = ExtensionHub::new();
    hub.install_all(vec![Arc::new(NamedExt("alpha"))])
        .await
        .unwrap();

    hub.record_active(&db).await.unwrap();
    append_event(
        &db,
        ExtensionEvent::Deactivated {
            name: "alpha".into(),
            timestamp: Utc::now() + chrono::Duration::seconds(1),
        },
    )
    .await
    .unwrap();
    assert!(read_active(&db).await.unwrap().is_empty());

    // A subsequent record_active does NOT reactivate — the Deactivated
    // event stands. Only an explicit `/extensions add X` (which writes
    // a fresh Activated) can bring it back.
    hub.record_active(&db).await.unwrap();
    assert!(
        read_active(&db).await.unwrap().is_empty(),
        "record_active should respect prior Deactivated"
    );
}

#[tokio::test]
async fn read_active_folds_to_latest_event_per_name() {
    let (_inst, db) = make_session_db().await;
    let t0 = Utc::now();
    // alpha: Activated at t0, then Deactivated at t1 — should not be active.
    append_event(
        &db,
        ExtensionEvent::Activated {
            name: "alpha".into(),
            extension_ref: ExtensionRef::builtin("alpha"),
            timestamp: t0,
        },
    )
    .await
    .unwrap();
    append_event(
        &db,
        ExtensionEvent::Deactivated {
            name: "alpha".into(),
            timestamp: t0 + chrono::Duration::seconds(10),
        },
    )
    .await
    .unwrap();
    // beta: Activated at t0 only — should be active.
    append_event(
        &db,
        ExtensionEvent::Activated {
            name: "beta".into(),
            extension_ref: ExtensionRef::builtin("beta"),
            timestamp: t0,
        },
    )
    .await
    .unwrap();

    let active = read_active(&db).await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].name(), "beta");
}

#[tokio::test]
async fn read_disabled_returns_only_latest_deactivated_names() {
    let (_inst, db) = make_session_db().await;
    let t0 = Utc::now();
    // memory: removed (Deactivated) — should be in the disabled set.
    append_event(
        &db,
        ExtensionEvent::Deactivated {
            name: "memory".into(),
            timestamp: t0,
        },
    )
    .await
    .unwrap();
    // web: removed then re-added — latest is Activated, so NOT disabled.
    append_event(
        &db,
        ExtensionEvent::Deactivated {
            name: "web".into(),
            timestamp: t0,
        },
    )
    .await
    .unwrap();
    append_event(
        &db,
        ExtensionEvent::Activated {
            name: "web".into(),
            extension_ref: ExtensionRef::builtin("web"),
            timestamp: t0 + chrono::Duration::seconds(10),
        },
    )
    .await
    .unwrap();

    let disabled = read_disabled(&db).await.unwrap();
    assert_eq!(disabled.len(), 1);
    assert!(disabled.contains("memory"));
    assert!(!disabled.contains("web"));
}

struct VersionedExt(&'static str, &'static str);
impl Extension for VersionedExt {
    fn name(&self) -> &'static str {
        self.0
    }
    fn extension_ref(&self) -> ExtensionRef {
        ExtensionRef::Git {
            name: self.0.to_string(),
            repo: "repo".to_string(),
            sha: self.1.to_string(),
        }
    }
    fn supported_hooks(&self) -> &[HookKind] {
        &[]
    }
}

#[tokio::test]
async fn record_active_writes_new_event_when_version_bumps() {
    let (_inst, db) = make_session_db().await;

    let mut hub_v1 = ExtensionHub::new();
    hub_v1
        .install_all(vec![Arc::new(VersionedExt("loop", "sha1"))])
        .await
        .unwrap();
    hub_v1.record_active(&db).await.unwrap();
    assert_eq!(list_events(&db).await.unwrap().len(), 1);

    // Fresh hub with the same name but different SHA: must write a new
    // event so the upgrade is captured in the log.
    let mut hub_v2 = ExtensionHub::new();
    hub_v2
        .install_all(vec![Arc::new(VersionedExt("loop", "sha2"))])
        .await
        .unwrap();
    hub_v2.record_active(&db).await.unwrap();
    let events = list_events(&db).await.unwrap();
    assert_eq!(events.len(), 2);
    let active = read_active(&db).await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].version(), "sha2");
}

#[tokio::test]
async fn hub_records_owner_for_each_hook_registration() {
    let mut hub = test_hub().await;
    hub.install_all(vec![tool_call_ext("alpha"), tool_call_ext("beta")])
        .await
        .unwrap();

    let alpha_kinds = hub.hooks_for("alpha");
    assert!(alpha_kinds.contains(&HookKind::ToolCall));
    let beta_kinds = hub.hooks_for("beta");
    assert!(beta_kinds.contains(&HookKind::ToolCall));
    // Other kinds untouched.
    assert!(!alpha_kinds.contains(&HookKind::ToolResult));
}

#[tokio::test]
async fn extensions_for_kind_returns_only_handlers_in_registration_order() {
    let mut hub = test_hub().await;
    hub.install_all(vec![
        Arc::new(NamedExt("noop")),
        tool_call_ext("alpha"),
        tool_call_ext("beta"),
    ])
    .await
    .unwrap();
    let owners = hub.extensions_for_kind(HookKind::ToolCall);
    assert_eq!(owners, vec!["alpha", "beta"]);
    let none = hub.extensions_for_kind(HookKind::AgentEnd);
    assert!(none.is_empty());
}

#[tokio::test]
async fn commands_track_owner_and_are_queryable() {
    let mut hub = test_hub().await;
    hub.install_all(vec![Arc::new(
        TestExt::new("with_command").command("dance", Arc::new(DummyCmd)),
    )])
    .await
    .unwrap();
    assert!(hub.commands_for("with_command").contains("dance"));
    assert_eq!(hub.command_owner("dance"), Some("with_command"));
    assert_eq!(hub.command_owner("not_real"), None);
}

#[tokio::test]
async fn hub_extension_refs_returns_one_per_extension_in_order() {
    let mut hub = test_hub().await;
    hub.install_all(vec![
        Arc::new(NamedExt("alpha")),
        Arc::new(NamedExt("beta")),
    ])
    .await
    .unwrap();
    let refs = hub.extension_refs();
    assert_eq!(refs.len(), 2);
    assert_eq!(refs[0].name(), "alpha");
    assert_eq!(refs[1].name(), "beta");
    for r in &refs {
        assert!(matches!(r, ExtensionRef::Builtin { .. }));
    }
}

#[tokio::test]
async fn before_agent_start_runs_in_registration_order() {
    let mut hub = test_hub().await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    hub.install_all(vec![
        counting_ext("a", calls.clone()),
        counting_ext("b", calls.clone()),
    ])
    .await
    .unwrap();
    let ctx = fixture_ctx_with_active(all_active(&["a", "b"])).await;
    let injected = hub.fire_before_agent_start(&ctx).await;
    assert_eq!(injected.len(), 2);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn inactive_extension_does_not_fire_hooks() {
    // Only "a" is active; "b" must be skipped despite being registered.
    let mut hub = test_hub().await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    hub.install_all(vec![
        counting_ext("a", calls.clone()),
        counting_ext("b", calls.clone()),
    ])
    .await
    .unwrap();
    let ctx = fixture_ctx_with_active(all_active(&["a"])).await;
    hub.fire_before_agent_start(&ctx).await;
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn panicking_hook_is_isolated_and_subsequent_hooks_still_run() {
    let mut hub = test_hub().await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    hub.install_all(vec![
        Arc::new(TestExt::new("boom").before_agent_start(Arc::new(PanickingBeforeAgentStartHook))),
        counting_ext("after", calls.clone()),
    ])
    .await
    .unwrap();
    let ctx = fixture_ctx_with_active(all_active(&["boom", "after"])).await;

    // Must not panic-propagate out of `fire_*` — that's the whole
    // point of the catch_unwind wrap.
    let injected = hub.fire_before_agent_start(&ctx).await;

    // The second handler still ran (one message injected, counter
    // incremented). The panicking handler contributed nothing.
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(injected.len(), 1);
}

#[tokio::test]
async fn tool_call_block_short_circuits_and_mutation_propagates() {
    let mut hub = test_hub().await;
    hub.install_all(vec![
        Arc::new(TestExt::new("mutating").tool_call(Arc::new(MutatingHook))),
        Arc::new(TestExt::new("blocking").tool_call(Arc::new(BlockingHook))),
    ])
    .await
    .unwrap();
    let ctx = fixture_ctx_with_active(all_active(&["mutating", "blocking"])).await;

    let mut args = serde_json::json!({});
    let decision = hub.fire_tool_call(&ctx, "read_file", &mut args).await;
    assert!(matches!(decision, ToolCallDecision::Continue));
    assert_eq!(args.get("touched").and_then(|v| v.as_bool()), Some(true));

    let mut args2 = serde_json::json!({});
    let decision2 = hub.fire_tool_call(&ctx, "shell", &mut args2).await;
    assert!(matches!(
        decision2,
        ToolCallDecision::Block { ref reason } if reason == "blocked by test"
    ));
}

#[tokio::test]
async fn command_collision_with_builtin_is_rejected() {
    let mut hub = test_hub().await;
    hub.reserve_builtin_commands(["info"]);
    hub.install_all(vec![cmd_ext("ext", "info")]).await.unwrap();
    assert!(!hub.has_command("info"));
}

#[tokio::test]
async fn duplicate_extension_command_keeps_first() {
    let mut hub = test_hub().await;
    struct OtherCmd;
    impl ExtensionCommand for OtherCmd {
        fn description(&self) -> &'static str {
            "other"
        }
        fn invoke<'a>(
            &'a self,
            _args: &'a str,
            _ctx: &'a HookContext,
        ) -> Pin<Box<dyn Future<Output = ExtensionCommandOutcome> + Send + 'a>> {
            Box::pin(async move { ExtensionCommandOutcome::Text("other".into()) })
        }
    }
    // Drain order = vec order: "first" registers "greet" before
    // "second" tries to — first-write-wins keeps "first".
    hub.install_all(vec![
        cmd_ext("first", "greet"),
        Arc::new(TestExt::new("second").command("greet", Arc::new(OtherCmd))),
    ])
    .await
    .unwrap();
    // "first" wins the collision and owns the command; needs to be
    // in the active set or dispatch will return None.
    let ctx = fixture_ctx_with_active(all_active(&["first"])).await;
    let out = hub
        .try_dispatch_command("greet", "x", &ctx)
        .await
        .expect("command registered");
    match out {
        ExtensionCommandOutcome::Text(s) => assert_eq!(s, "got: x"),
        _ => panic!("expected text outcome"),
    }
}

// -----------------------------------------------------------------
// install_all coverage
// -----------------------------------------------------------------

#[tokio::test]
async fn command_install_does_not_wait_for_executor_extensions_or_drain_tools() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct HeldRuntime {
        entered: Arc<AtomicUsize>,
        release: Arc<tokio::sync::Semaphore>,
    }
    impl Extension for HeldRuntime {
        fn name(&self) -> &'static str {
            "held_runtime"
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &[HookKind::Tool]
        }
        fn instantiate<'a>(&'a self, _: ScopeCtx<'a>) -> instance::InstantiateFuture<'a> {
            Box::pin(async move {
                self.entered.fetch_add(1, Ordering::SeqCst);
                self.release.acquire().await.unwrap().forget();
                Ok(Arc::new(TestInstance {
                    manifest: self.manifest(),
                    parts: TestParts::default(),
                }) as Arc<dyn ExtensionInstance>)
            })
        }
    }
    let entered = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let runtime: Arc<dyn Extension> = Arc::new(HeldRuntime {
        entered: entered.clone(),
        release: release.clone(),
    });
    let command = TestExt::new("command_provider")
        .command("data", Arc::new(DummyCmd))
        .before_agent_start(Arc::new(CountingHook {
            calls: Arc::new(AtomicUsize::new(0)),
        }))
        .tool(Arc::new(crate::tools::ShellExec));
    let mut client = test_hub().await;
    tokio::time::timeout(
        Duration::from_secs(1),
        client.install_commands(vec![runtime.clone(), Arc::new(command)]),
    )
    .await
    .expect("client must not instantiate the held runtime")
    .unwrap();
    assert_eq!(entered.load(Ordering::SeqCst), 0);
    assert!(client.extension_names().contains(&"held_runtime"));
    assert!(!client.global_instances.contains_key("held_runtime"));
    assert_eq!(client.command_owner("data"), Some("command_provider"));
    let ctx = fixture_ctx_with_active(all_active(&["command_provider"])).await;
    assert!(matches!(
        client.try_dispatch_command("data", "", &ctx).await,
        Some(ExtensionCommandOutcome::Text(_))
    ));
    let mut inactive = ctx;
    inactive.active_extensions.clear();
    assert!(
        client
            .try_dispatch_command("data", "", &inactive)
            .await
            .is_none()
    );
    assert!(client.tools_for_registry().is_empty());
    assert_eq!(
        client.hooks_for("command_provider"),
        HashSet::from([HookKind::Command])
    );

    // The same held component really does block executor installation.
    let mut executor = test_hub().await;
    let install = executor.install_all(vec![runtime]);
    tokio::pin!(install);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), install.as_mut())
            .await
            .is_err()
    );
    assert_eq!(entered.load(Ordering::SeqCst), 1);
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(1), install)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn install_all_minimal_extension_is_a_noop() {
    let mut hub = test_hub().await;
    hub.install_all(vec![Arc::new(NamedExt("solo"))])
        .await
        .unwrap();
    // Registered, but nothing drained: no routine handler, no
    // tools, no commands, no hooks.
    assert!(hub.extension_names().contains(&"solo"));
    assert!(hub.installed_for("solo").is_none());
    assert!(hub.hooks_for("solo").is_empty());
    assert!(hub.tools_for("solo").is_empty());
    assert!(hub.commands_for("solo").is_empty());
}

#[tokio::test]
async fn install_all_is_idempotent() {
    let mut hub = test_hub().await;
    let ext: Arc<dyn Extension> = Arc::new(NamedExt("solo"));
    hub.install_all(vec![ext.clone()]).await.unwrap();
    hub.install_all(vec![ext]).await.unwrap();
    // Re-installing the same Global extension doesn't double its
    // instance.
    assert!(hub.global_instances.contains_key("solo"));
}

#[tokio::test]
async fn install_all_skips_failing_extension_and_installs_the_rest() {
    let mut hub = test_hub().await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // "broken" is configured exactly like a healthy extension — a
    // command and a before_agent_start hook — and fails only at
    // instantiate, so anything of its still present in the hub proves
    // the drain ran on a failed instantiation.
    let broken = TestExt::new("broken")
        .command("boom", Arc::new(DummyCmd))
        .before_agent_start(Arc::new(CountingHook {
            calls: calls.clone(),
        }))
        .fails_instantiate();
    hub.install_all(vec![
        cmd_ext("healthy_first", "one"),
        Arc::new(broken),
        cmd_ext("healthy_second", "two"),
    ])
    .await
    .expect("one failing instantiation must not fail install_all");

    // Healthy extensions installed normally.
    assert!(hub.global_instances.contains_key("healthy_first"));
    assert!(hub.global_instances.contains_key("healthy_second"));
    assert_eq!(hub.command_owner("one"), Some("healthy_first"));
    assert_eq!(hub.command_owner("two"), Some("healthy_second"));

    // The failing one is registered but contributes nothing.
    assert!(hub.extension_names().contains(&"broken"));
    assert!(!hub.global_instances.contains_key("broken"));
    assert!(hub.commands_for("broken").is_empty());
    assert_eq!(hub.command_owner("boom"), None);
    assert!(hub.hooks_for("broken").is_empty());
    let ctx = fixture_ctx_with_active(all_active(&["broken"])).await;
    assert!(hub.fire_before_agent_start(&ctx).await.is_empty());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);

    // The failure is recorded so callers can surface a degraded peer
    // without reading logs (mirrors McpRegistry::insert_failed).
    let failures = hub.install_failures();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].name, "broken");
    assert!(
        failures[0]
            .error
            .contains("simulated instantiation failure")
    );
}

#[tokio::test]
async fn install_all_records_no_failures_when_every_global_extension_installs() {
    let mut hub = test_hub().await;
    hub.install_all(vec![cmd_ext("alpha", "one"), cmd_ext("beta", "two")])
        .await
        .unwrap();
    assert!(hub.install_failures().is_empty());
}

#[tokio::test]
async fn install_all_clears_a_failure_when_a_later_install_succeeds() {
    let mut hub = test_hub().await;
    let broken = Arc::new(TestExt::new("flaky").fails_instantiate()) as Arc<dyn Extension>;
    hub.install_all(vec![broken]).await.unwrap();
    assert_eq!(hub.install_failures().len(), 1);

    let fixed = Arc::new(TestExt::new("flaky")) as Arc<dyn Extension>;
    hub.install_all(vec![fixed]).await.unwrap();
    assert!(hub.install_failures().is_empty());
    assert!(hub.global_instances.contains_key("flaky"));
}

#[tokio::test]
async fn install_all_isolates_panicking_instantiation_and_installs_the_rest() {
    let mut hub = test_hub().await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // "boom" panics during instantiate; "after" must still install.
    let boom = TestExt::new("boom")
        .command("will-not-register", Arc::new(DummyCmd))
        .panics_instantiate();
    hub.install_all(vec![Arc::new(boom), counting_ext("after", calls.clone())])
        .await
        .expect("a panicking instantiation must not fail install_all");

    // The panicking extension is registered but contributes nothing
    // — its drain never ran.
    assert!(hub.extension_names().contains(&"boom"));
    assert!(!hub.global_instances.contains_key("boom"));
    assert!(hub.commands_for("boom").is_empty());
    assert_eq!(hub.command_owner("will-not-register"), None);

    // The healthy extension installed normally.
    assert!(hub.global_instances.contains_key("after"));
    let ctx = fixture_ctx_with_active(all_active(&["after"])).await;
    let injected = hub.fire_before_agent_start(&ctx).await;
    assert_eq!(injected.len(), 1);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // A panic is an install failure too, and is recorded alongside the
    // `Err` case so `/extensions list` flags it.
    let failures = hub.install_failures();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].name, "boom");
    assert!(failures[0].error.contains("panicked"));
}

#[tokio::test]
async fn install_all_instantiates_duplicate_names_once() {
    let mut hub = test_hub().await;
    let instantiations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dup = || {
        Arc::new(
            TestExt::new("dup")
                .before_agent_start(Arc::new(CountingHook {
                    calls: calls.clone(),
                }))
                .instantiation_counter(instantiations.clone()),
        ) as Arc<dyn Extension>
    };
    hub.install_all(vec![dup(), dup()]).await.unwrap();

    // Only the first occurrence of the name instantiates.
    assert_eq!(instantiations.load(std::sync::atomic::Ordering::SeqCst), 1);

    // ...and its hook is registered exactly once, so it fires once
    // per turn.
    assert_eq!(hub.before_agent_start.len(), 1);
    let ctx = fixture_ctx_with_active(all_active(&["dup"])).await;
    let injected = hub.fire_before_agent_start(&ctx).await;
    assert_eq!(injected.len(), 1);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// Global extension that parks in `instantiate` until a shared barrier
/// releases it. Two of these complete only if the hub has both
/// instantiations in flight at the same time.
struct BarrierExt {
    name: &'static str,
    barrier: Arc<tokio::sync::Barrier>,
}
impl Extension for BarrierExt {
    fn name(&self) -> &'static str {
        self.name
    }
    fn supported_hooks(&self) -> &[HookKind] {
        &[]
    }
    fn instantiate<'a>(
        &'a self,
        _scope_ctx: instance::ScopeCtx<'a>,
    ) -> instance::InstantiateFuture<'a> {
        let manifest = self.manifest();
        let barrier = self.barrier.clone();
        Box::pin(async move {
            barrier.wait().await;
            Ok(Arc::new(TestInstance {
                manifest,
                parts: TestParts::default(),
            }) as Arc<dyn instance::ExtensionInstance>)
        })
    }
}

#[tokio::test]
async fn install_all_instantiates_global_extensions_concurrently() {
    let mut hub = test_hub().await;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    // A serial install_all parks "left" on a barrier that "right"
    // never reaches, and the timeout fires.
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        hub.install_all(vec![
            Arc::new(BarrierExt {
                name: "left",
                barrier: barrier.clone(),
            }),
            Arc::new(BarrierExt {
                name: "right",
                barrier: barrier.clone(),
            }),
        ]),
    )
    .await
    .expect("Global instantiations must run concurrently")
    .unwrap();

    assert!(hub.global_instances.contains_key("left"));
    assert!(hub.global_instances.contains_key("right"));
}

// -----------------------------------------------------------------
// dispatch_routine coverage
// -----------------------------------------------------------------

#[tokio::test]
async fn dispatch_routine_invokes_registered_handler() {
    let mut hub = test_hub().await;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    hub.install_all(vec![Arc::new(
        TestExt::new("heartbeat").routine_handler(Arc::new(Recorder { seen: seen.clone() })),
    )])
    .await
    .unwrap();

    hub.dispatch_routine(
        "heartbeat",
        &RoutineScope::Global,
        serde_json::json!({"task": "ping"}),
    )
    .await
    .unwrap();

    let recorded = seen.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0], serde_json::json!({"task": "ping"}));
}

#[tokio::test]
async fn dispatch_routine_unknown_extension_errors() {
    let hub = ExtensionHub::new();
    let err = hub
        .dispatch_routine("ghost", &RoutineScope::Global, serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("ghost"), "got: {err}");
}

#[tokio::test]
async fn dispatch_routine_extension_without_handler_errors() {
    // A minimal extension publishes no routine handler.
    let mut hub = test_hub().await;
    hub.install_all(vec![Arc::new(NamedExt("solo"))])
        .await
        .unwrap();
    let err = hub
        .dispatch_routine("solo", &RoutineScope::Global, serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("routine"), "got: {err}");
}

#[tokio::test]
async fn dispatch_routine_handler_error_propagates() {
    struct AlwaysFails;
    impl handler::RoutineHandler for AlwaysFails {
        fn on_fire<'a>(
            &'a self,
            _payload: serde_json::Value,
        ) -> handler::HandlerFuture<'a, anyhow::Result<()>> {
            Box::pin(async { Err(anyhow::anyhow!("simulated failure")) })
        }
    }
    let mut hub = test_hub().await;
    hub.install_all(vec![Arc::new(
        TestExt::new("broken").routine_handler(Arc::new(AlwaysFails)),
    )])
    .await
    .unwrap();
    let err = hub
        .dispatch_routine("broken", &RoutineScope::Global, serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("simulated"), "got: {err}");
}

/// Publishes a no-op Messenger through the instance `messenger()`
/// endpoint. Used by `cap_resolver_walks_instance_published_caps`.
/// Messenger is not part of `TestExt`'s derived `provides_capabilities`,
/// so this publishes the endpoint without declaring it — same manifest
/// as the one-off double it replaced.
fn instance_messenger_ext() -> Arc<dyn Extension> {
    struct NoopMessenger;
    impl caps::Messenger for NoopMessenger {
        fn send<'a>(
            &'a self,
            _target: String,
            _body: caps::MessageBody,
        ) -> caps::CapFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }
    }
    Arc::new(TestExt::new("inst-messenger").messenger(Arc::new(NoopMessenger)))
}

#[tokio::test]
async fn cap_resolver_walks_instance_published_caps() {
    let mut hub = test_hub().await;
    hub.install_all(vec![instance_messenger_ext()])
        .await
        .unwrap();

    let resolver = hub.cap_resolver_for_turn(None, None).await;
    use instance::CapResolver as _;
    assert!(
        resolver.messenger().is_some(),
        "HubCapResolver should expose an instance-published Messenger"
    );
}

#[tokio::test]
async fn install_all_validates_manifests_before_instantiation() {
    // Manifest with empty name — should reject before any
    // instantiation runs.
    struct BadManifestExt;
    impl Extension for BadManifestExt {
        fn name(&self) -> &'static str {
            "named-in-trait-but-not-in-manifest"
        }
        fn supported_hooks(&self) -> &[HookKind] {
            &[]
        }
        fn manifest(&self) -> manifest::ExtensionManifest {
            manifest::ExtensionManifest {
                name: String::new(), // <-- triggers EmptyName
                extension_ref: ExtensionRef::builtin("x"),
                supported_hooks: Vec::new(),
                required_capabilities: Vec::new(),
                requested_capabilities: Vec::new(),
                provides_capabilities: Vec::new(),
            }
        }
    }
    let mut hub = ExtensionHub::new();
    let err = hub
        .install_all(vec![Arc::new(BadManifestExt)])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("name"), "got: {err}");
}

/// Injected pre-turn messages are not persisted, so a wait in that turn is
/// never checkpointed; a resumed turn never fires `before_agent_start` again.
#[tokio::test]
async fn injected_pre_turn_messages_prevent_wait_checkpoints_and_never_refire() {
    use crate::runtime::{JobWaitIntent, RuntimeRecord, RuntimeRecorder};
    use crate::tool::{ToolDescriptor, ToolError};
    use std::future::Future;
    use std::pin::Pin;

    struct Wait;
    impl Tool for Wait {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "job_wait".into(),
                description: "test wait".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }
        }
        fn execute<'a>(
            &'a self,
            _: serde_json::Value,
            _: &'a crate::tool::ToolContext,
        ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
            Box::pin(async { Ok("observed".into()) })
        }
    }

    #[derive(Default)]
    struct Intents(std::sync::Mutex<usize>, std::sync::atomic::AtomicUsize);
    impl RuntimeRecorder for Intents {
        fn record<'a>(
            &'a self,
            _: RuntimeRecord,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }
        fn checkpoint_job_wait<'a>(
            &'a self,
            _: JobWaitIntent,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            *self.0.lock().unwrap() += 1;
            Box::pin(async { Ok(()) })
        }
        fn finish_job_wait<'a>(
            &'a self,
            _: u64,
            _: usize,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    struct ResultHook {
        recorder: Arc<Intents>,
        observations: Arc<std::sync::Mutex<Vec<usize>>>,
    }
    impl handler::HookHandlerToolResult for ResultHook {
        fn on_tool_result<'a>(
            &'a self,
            _: &'a str,
            output: String,
        ) -> handler::HandlerFuture<'a, String> {
            Box::pin(async move {
                self.observations
                    .lock()
                    .unwrap()
                    .push(self.recorder.1.load(std::sync::atomic::Ordering::SeqCst));
                output
            })
        }
    }

    let mut hub = test_hub().await;
    let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let recorder = Arc::new(Intents::default());
    let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let result_ext = TestExt::new("result").tool_result(Arc::new(ResultHook {
        recorder: recorder.clone(),
        observations: observations.clone(),
    }));
    hub.install_all(vec![
        counting_ext("inject", fired.clone()),
        Arc::new(result_ext),
    ])
    .await
    .unwrap();
    let (_instance, session) = crate::test_support::fresh_session().await;
    let tools = Arc::new(ToolRegistry::new());
    tools.register(Wait);
    let mut ctx = crate::test_support::tool_context(session, tools);
    ctx.active_extensions = all_active(&["inject", "result"]);
    let arguments =
        serde_json::json!({"session_db_id": "child", "timeout_seconds": 30}).to_string();
    let mock = Arc::new(crate::test_support::MockBackend::new());
    mock.push_tool_calls([("w".into(), "job_wait".into(), arguments.clone())]);
    mock.push_text("done");
    mock.push_text("resumed done");
    let backend = crate::backends::BackendManager::with_mock(
        mock.clone(),
        crate::test_support::empty_secrets().await,
    );
    let policies = crate::tool::ToolPolicyRegistry::empty();
    let security = crate::test_support::permissive_security();
    let outcome = crate::runtime::execute_with_recorder(
        None,
        vec![RuntimeMessage::User("go".into())],
        &backend,
        &security,
        &ctx,
        &policies,
        Some(recorder.clone()),
        Some(&hub),
        None,
        crate::runtime::ModelCallScope::default(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.body, "done");
    assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        *recorder.0.lock().unwrap(),
        0,
        "no checkpoint after injection"
    );

    let call = crate::runtime::ToolCallRequest {
        id: "w".into(),
        name: "job_wait".into(),
        arguments,
    };
    let now = chrono::Utc::now();
    let outcome = crate::runtime::resume_with_recorder(
        None,
        vec![RuntimeMessage::User("go".into())],
        &backend,
        &security,
        &ctx,
        &policies,
        Some(recorder),
        Some(&hub),
        None,
        crate::runtime::RuntimeResume {
            prior_model_sequences: vec![0],
            prior_messages: vec![RuntimeMessage::AssistantToolCalls {
                exchange_id: None,
                content: None,
                tool_calls: vec![call.clone()],
                provider_extra: Default::default(),
            }],
            model_sequence: 0,
            call_index: 0,
            call,
            deadline: now + chrono::Duration::seconds(30),
            policy_deadline: now + chrono::Duration::seconds(60),
            policy_timeout_seconds: 60,
            metadata: Vec::new(),
            matrix_send_queued: false,
            rate_limited_calls: Vec::new(),
        },
        crate::runtime::ModelCallScope::default(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.body, "resumed done");
    assert_eq!(
        fired.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "before_agent_start does not run again on resume"
    );
    assert_eq!(
        *observations.lock().unwrap(),
        [1, 2],
        "both live and resumed waits retire before result hooks"
    );
}
