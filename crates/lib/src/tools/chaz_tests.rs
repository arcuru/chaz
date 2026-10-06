use super::*;
use crate::grants::Grants;
use crate::runtime::{ResponseMetadata, TokenUsage};
use crate::session::{EntryType, Session, SessionEntry};
use crate::test_support::{
    MockHost, fresh_session, fresh_session_registry, tool_context_with_host,
};
use crate::tool::{PresentationMode, ScopedTools, ToolPolicy, ToolProfile, ToolRegistry};
use chrono::TimeZone;
use std::collections::HashMap;
use tokio::sync::Mutex;

fn tool() -> ChazTool {
    ChazTool {
        policies: Arc::new(ToolPolicyRegistry::empty()),
    }
}

fn keys(value: &Value, expected: &[&str]) {
    let mut expected: Vec<_> = expected.to_vec();
    expected.sort();
    let mut actual: Vec<_> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    actual.sort();
    assert_eq!(actual, expected);
}

async fn query(tool: &ChazTool, ctx: &ToolContext, name: &str, period: Value) -> Value {
    serde_json::from_str(
        &tool
            .execute(json!({"query": name, "period": period}), ctx)
            .await
            .unwrap(),
    )
    .unwrap()
}

fn message(ts: DateTime<Utc>, sender: &str, model: &str, cost: Option<f64>) -> SessionEntry {
    SessionEntry {
        sender: sender.into(),
        content: "TRANSCRIPT_SECRET".into(),
        timestamp: ts,
        entry_type: EntryType::Message,
        metadata: Some(ResponseMetadata {
            model: model.into(),
            usage: TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
                cost_usd: cost,
                ..Default::default()
            },
            ..Default::default()
        }),
        routing: None,
    }
}

#[test]
fn descriptor_is_the_frozen_closed_strict_schema() {
    let t = tool();
    assert!(t.strict_schema());
    let d = t.descriptor();
    assert_eq!(d.name, "chaz");
    assert_eq!(
        d.description,
        "Query your current session, caller identity, callable tools and policies, or recorded session usage. Read-only; no other sessions or agents. Set period only for usage."
    );
    assert_eq!(
        d.parameters,
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "enum": ["session", "agent", "tools", "usage"]},
                "period": {"type": ["string", "null"], "enum": ["all", "today", "week", "month", null],
                    "description": "For usage: all, or the current UTC calendar day/week/month. For other queries: null."}
            },
            "required": ["query", "period"], "additionalProperties": false
        })
    );
    // Registration also exercises the registry's strict-schema sanity check.
    ToolRegistry::new().register(t);
    let policy = tool().default_policy();
    assert_eq!(
        serde_json::to_value(policy).unwrap(),
        serde_json::to_value(ToolPolicy::default()).unwrap()
    );
}

#[tokio::test]
async fn rejects_invalid_inputs_before_session_reads() {
    let (_instance, session) = fresh_session().await;
    let ctx = tool_context_with_host(
        session.clone(),
        Arc::new(ToolRegistry::new()),
        Arc::new(MockHost::new()),
    );
    let _locked = session.lock().await;
    let inputs = [
        json!(null),
        json!([]),
        json!("session"),
        json!({}),
        json!({"query": "session"}),
        json!({"period": null}),
        json!({"query": null, "period": null}),
        json!({"query": 1, "period": null}),
        json!({"query": "usage", "period": 7}),
        json!({"query": "usage", "period": []}),
        json!({"query": "usage", "period": {}}),
        json!({"query": "usage", "period": true}),
        json!({"query": "usage", "period": null}),
        json!({"query": "usage", "period": "year"}),
        json!({"query": "session", "period": "all"}),
        json!({"query": "agent", "period": "today"}),
        json!({"query": "tools", "period": "week"}),
        json!({"query": "sessions", "period": null}),
        json!({"query": "rename", "period": null}),
        json!({"query": "attach", "period": null}),
        json!({"query": "session", "period": null, "session_db_id": "foreign"}),
        json!({"query": "usage", "period": "all", "scope": "all"}),
        json!({"query": "agent", "period": null, "agent": "foreign"}),
        json!({"query": "tools", "period": null, "args": {}}),
    ];
    for input in inputs {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            tool().execute(input.clone(), &ctx),
        )
        .await
        .expect("invalid input must not wait for the session");
        assert!(
            matches!(result, Err(ToolError::InvalidArgument(_))),
            "{input}: {result:?}"
        );
    }
    // Valid input really does try to acquire the held session (positive control).
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            tool().execute(json!({"query": "session", "period": null}), &ctx)
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn seven_valid_calls_are_read_only_and_need_no_host_capabilities() {
    let (_instance, session) = fresh_session().await;
    let host = Arc::new(MockHost::new()); // empty script fails any host request
    let mut ctx =
        tool_context_with_host(session.clone(), Arc::new(ToolRegistry::new()), host.clone());
    let denied: Grants = serde_json::from_value(json!({"shell": {"allow": []}, "network": {"endpoints": []}, "fs": {"allow_read": [], "allow_write": []}})).unwrap();
    ctx.session_capabilities = denied.clone();
    ctx.agent_capabilities = denied.clone();
    ctx.grants = denied;
    ctx.agent_name = "inherited-worker-identity".into();
    ctx.call_depth = 2;
    ctx.max_call_depth = 3;
    let (id, before) = {
        let s = session.lock().await;
        (
            s.database().root_id().to_string(),
            s.database().snapshot().await.unwrap().into_tips(),
        )
    };
    let out = query(&tool(), &ctx, "session", Value::Null).await;
    assert_eq!(out, json!({"session_db_id": id, "name": null}));
    assert_eq!(
        query(&tool(), &ctx, "agent", Value::Null).await,
        json!({"name": "inherited-worker-identity", "call_depth": 2, "max_call_depth": 3})
    );
    assert_eq!(
        query(&tool(), &ctx, "tools", Value::Null).await,
        json!({"tools": []})
    );
    for period in ["all", "today", "week", "month"] {
        let out = query(&tool(), &ctx, "usage", json!(period)).await;
        keys(
            &out,
            &["session_db_id", "period", "since", "total", "per_model"],
        );
        assert_eq!(out["session_db_id"], id);
        assert_eq!(out["period"], period);
        assert_eq!(out["since"].is_null(), period == "all");
        assert_eq!(
            out["total"],
            json!({"calls": 0, "prompt_tokens": 0, "completion_tokens": 0, "cached_tokens": 0, "cache_creation_tokens": 0, "reasoning_tokens": 0, "cost_usd": 0.0, "cost_reported": false})
        );
        assert_eq!(out["per_model"], json!({}));
    }
    assert!(host.recorded_calls().is_empty());
    let s = session.lock().await;
    assert!(s.entries().is_empty());
    assert_eq!(s.database().snapshot().await.unwrap().into_tips(), before);
}

#[tokio::test]
async fn same_peer_other_sessions_and_metadata_are_not_disclosed() {
    let (_instance, registry) = fresh_session_registry().await;
    let (aid, adb) = registry.create_session(Some("cli")).await.unwrap();
    let (bid, bdb) = registry.create_session(Some("cli")).await.unwrap();
    crate::agent_db::create_agent_db(
        &mut *registry.user_for_tests().await,
        "UNRELATED_AGENT_SECRET",
        &crate::agent_db::AgentDbConfig {
            system_prompt: "UNRELATED_PROMPT_SECRET".into(),
            model: Some("UNRELATED_MODEL_SECRET".into()),
            ..Default::default()
        },
        &Default::default(),
    )
    .await
    .unwrap();
    let mut a = Session::new(aid.clone(), adb.clone()).await;
    let mut b = Session::new(bid.clone(), bdb).await;
    a.update_meta(|m| {
        m.name = Some("session-A".into());
        m.agent_name = Some("NOT_THE_CALLER".into());
        m.backend_url = Some("BACKEND_SECRET".into());
        m.backend_key_ref = Some("KEY_REF_SECRET".into());
        m.role_prompt = Some("ROLE_PROMPT_SECRET".into());
        m.role_name = Some("ROLE_NAME_SECRET".into());
    })
    .await
    .unwrap();
    b.update_meta(|m| {
        m.name = Some("SESSION_B_SECRET".into());
        m.agent_name = Some("AGENT_B_SECRET".into());
    })
    .await
    .unwrap();
    let ts = Utc::now();
    a.add_entry(message(ts, "caller-A", "model-A", Some(0.25)))
        .await
        .unwrap();
    a.add_entry(message(ts, "other-participant-A", "model-A2", None))
        .await
        .unwrap();
    b.add_entry(message(ts, "AGENT_B_SECRET", "MODEL_B_SECRET", Some(900.0)))
        .await
        .unwrap();
    assert!(
        registry.open_session(&bid.0).await.is_ok(),
        "the peer can open B"
    );
    let a = Arc::new(Mutex::new(a));
    let mut ctx = tool_context_with_host(
        a.clone(),
        Arc::new(ToolRegistry::new()),
        Arc::new(MockHost::new()),
    );
    ctx.agent_name = "caller-A".into();
    let before = adb.snapshot().await.unwrap().into_tips();
    assert_eq!(
        query(&tool(), &ctx, "session", Value::Null).await,
        json!({"session_db_id": aid.0, "name": "session-A"})
    );
    let identity = query(&tool(), &ctx, "agent", Value::Null).await;
    assert_eq!(identity["name"], "caller-A");
    let usage = query(&tool(), &ctx, "usage", json!("all")).await;
    assert_eq!(usage["total"]["calls"], 2);
    assert_eq!(usage["total"]["cost_usd"], 0.25);
    keys(&usage["per_model"], &["model-A", "model-A2"]);
    for row in usage["per_model"].as_object().unwrap().values() {
        keys(
            row,
            &[
                "calls",
                "prompt_tokens",
                "completion_tokens",
                "cost_usd",
                "cost_reported",
            ],
        );
    }
    // Exact equality to this session's contribution, not a filtered peer scan.
    let rollup = crate::session::usage::collect_usage(&registry, &Default::default())
        .await
        .unwrap();
    let contribution = rollup
        .per_session
        .iter()
        .find(|s| s.session_db_id == aid.0)
        .unwrap();
    assert_eq!(
        usage["total"],
        serde_json::to_value(&contribution.totals).unwrap()
    );
    assert_eq!(rollup.total.calls, 3);
    for name in ["model-A", "model-A2"] {
        assert_eq!(
            usage["per_model"][name],
            serde_json::to_value(&rollup.per_model[name]).unwrap()
        );
    }
    for (name, period) in [
        ("session", Value::Null),
        ("agent", Value::Null),
        ("tools", Value::Null),
        ("usage", json!("all")),
    ] {
        let output = query(&tool(), &ctx, name, period).await.to_string();
        for forbidden in ["SECRET", "NOT_THE_CALLER", &bid.0] {
            assert!(!output.contains(forbidden), "{output}");
        }
    }
    assert_eq!(adb.snapshot().await.unwrap().into_tips(), before);
    assert_eq!(a.lock().await.entries().len(), 2);
}

#[tokio::test]
async fn tools_use_child_scope_hidden_presentation_and_effective_policy_grants() {
    let (_instance, session) = fresh_session().await;
    let registry = Arc::new(ToolRegistry::new());
    registry.register_arc_owned(Arc::new(crate::tools::ShellExec), Some("core"));
    registry.register_arc_owned(Arc::new(crate::tools::ReadFile), Some("fs"));
    registry.register(crate::tools::Calculate);
    registry.register(crate::tools::GetTime);
    registry.announce_pending_source("pending");
    let grants: Grants = serde_json::from_value(json!({"shell": {"allow": ["git", "ls", "cat"]}, "network": {"endpoints": [{"host": "example.com"}]}, "fs": {"allow_read": ["/work"], "allow_write": ["/work"]}})).unwrap();
    let policies = Arc::new(ToolPolicyRegistry::new(HashMap::from([
        (
            "shell".into(),
            ToolPolicy {
                risk: crate::tool::RiskLevel::Medium,
                approval: crate::tool::ApprovalRequirement::UnlessAutoApproved,
                timeout: 17,
                rate_limit: Some(3),
                grants,
                ..Default::default()
            },
        ),
        (
            "get_time".into(),
            ToolPolicy {
                timeout: 999,
                ..Default::default()
            },
        ),
    ])));
    let t = ChazTool {
        policies: policies.clone(),
    };
    let mut ctx = tool_context_with_host(session, registry.clone(), Arc::new(MockHost::new()));
    let parent = ScopedTools::new(
        registry,
        Some(vec![
            "shell".into(),
            "calculate".into(),
            "read_file".into(),
            "pending__*".into(),
        ]),
    );
    ctx.tools = parent
        .narrow(Some(&[
            "shell".into(),
            "calculate".into(),
            "read_file".into(),
            "get_time".into(),
            "pending__*".into(),
        ]))
        .with_active_extensions(Some(["core".into()].into()));
    ctx.profile = ToolProfile {
        default_mode: PresentationMode::Brief,
        tool_modes: HashMap::from([("shell".into(), PresentationMode::Hidden)]),
    };
    ctx.session_capabilities = serde_json::from_value(
        json!({"shell": {"allow": ["git", "ls"]}, "fs": {"allow_read": ["/work/project"]}}),
    )
    .unwrap();
    ctx.agent_capabilities =
        serde_json::from_value(json!({"shell": {"allow": ["git"]}, "network": {"endpoints": []}}))
            .unwrap();
    ctx.agent_grants.insert(
        "shell".into(),
        serde_json::from_value(json!({"fs": {"allow_write": []}})).unwrap(),
    );
    ctx.grants = Grants::default(); // must not report this invocation's grants for shell
    assert!(
        !ctx.tools
            .definitions(&ctx.profile)
            .iter()
            .any(|d| d.name == "shell")
    );
    assert!(ctx.tools.get("shell").is_some(), "Hidden is callable");
    let out = query(&t, &ctx, "tools", Value::Null).await;
    keys(&out, &["tools"]);
    let rows = out["tools"].as_array().unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["calculate", "shell"]
    );
    for row in rows {
        keys(
            row,
            &[
                "name",
                "presentation",
                "risk",
                "approval",
                "timeout_seconds",
                "rate_limit",
                "effective_grants",
            ],
        );
    }
    assert_eq!(rows[0]["presentation"], "brief");
    assert_eq!(rows[0]["risk"], "low");
    assert_eq!(rows[0]["approval"], "never");
    assert_eq!(rows[0]["timeout_seconds"], 60);
    assert!(rows[0]["rate_limit"].is_null());
    assert_eq!(rows[1]["presentation"], "hidden");
    assert_eq!(rows[1]["risk"], "medium");
    assert_eq!(rows[1]["approval"], "unless_auto_approved");
    assert_eq!(rows[1]["timeout_seconds"], 17);
    assert_eq!(rows[1]["rate_limit"], 3);
    let shell = ctx.tools.get("shell").unwrap();
    let effective = ctx.resolve_call_grants(&policies.resolve(shell.as_ref()).grants, "shell");
    assert_eq!(
        rows[1]["effective_grants"],
        serde_json::to_value(&effective).unwrap()
    );
    assert_eq!(
        rows[1]["effective_grants"]["shell"]["allow"],
        json!(["git"])
    );
    assert_eq!(
        rows[1]["effective_grants"]["network"]["endpoints"],
        json!([])
    );
    assert_eq!(
        rows[1]["effective_grants"]["fs"]["allow_read"],
        json!(["/work/project"])
    );
    assert_eq!(rows[1]["effective_grants"]["fs"]["allow_write"], json!([]));
    ctx.tools = ctx.tools.narrow(Some(&["calculate".into()]));
    let out = query(&t, &ctx, "tools", Value::Null).await;
    assert_eq!(out["tools"].as_array().unwrap().len(), 1);
    assert_eq!(out["tools"][0]["name"], "calculate");
    ctx.tools = ctx.tools.narrow(Some(&[]));
    assert_eq!(
        query(&t, &ctx, "tools", Value::Null).await,
        json!({"tools": []})
    );
}

#[test]
fn utc_calendar_boundaries_are_not_rolling_windows() {
    let now = Utc.with_ymd_and_hms(2026, 10, 3, 23, 59, 59).unwrap();
    assert_eq!(Period::All.since(now), None);
    assert_eq!(
        Period::Today.since(now),
        Some(Utc.with_ymd_and_hms(2026, 10, 3, 0, 0, 0).unwrap())
    );
    assert_eq!(
        Period::Week.since(now),
        Some(Utc.with_ymd_and_hms(2026, 9, 28, 0, 0, 0).unwrap())
    );
    assert_eq!(
        Period::Month.since(now),
        Some(Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap())
    );
    let monday = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
    for p in [Period::Today, Period::Week, Period::Month] {
        assert_eq!(p.since(monday), Some(monday));
    }
    let sunday = Utc.with_ymd_and_hms(2026, 1, 4, 0, 0, 0).unwrap();
    assert_eq!(
        Period::Week.since(sunday),
        Some(Utc.with_ymd_and_hms(2025, 12, 29, 0, 0, 0).unwrap())
    );
}

#[tokio::test]
async fn runtime_enforces_scope_extension_and_policy_for_chaz() {
    use crate::backends::BackendManager;
    use crate::bridge::ApprovalDecision;
    use crate::runtime::{self, RuntimeMessage};
    use crate::test_support::{
        MockBackend, empty_secrets, permissive_security, security_with_decision,
    };
    for mode in [
        "allowed-hidden",
        "removed",
        "extension-disabled",
        "approval-denied",
    ] {
        let (_instance, session) = fresh_session().await;
        let registry = Arc::new(ToolRegistry::new());
        let policies = Arc::new(ToolPolicyRegistry::new(if mode == "approval-denied" {
            HashMap::from([(
                "chaz".into(),
                ToolPolicy {
                    approval: crate::tool::ApprovalRequirement::Always,
                    ..Default::default()
                },
            )])
        } else {
            HashMap::new()
        }));
        registry.register_arc_owned(
            Arc::new(ChazTool {
                policies: policies.clone(),
            }),
            Some("core"),
        );
        registry.register(crate::tools::Calculate); // keep denied scopes nonempty
        let mut ctx = tool_context_with_host(session, registry, Arc::new(MockHost::new()));
        ctx.profile
            .tool_modes
            .insert("chaz".into(), PresentationMode::Hidden);
        if mode == "removed" {
            ctx.tools = ctx.tools.narrow(Some(&["calculate".into()]));
        }
        if mode == "extension-disabled" {
            ctx.tools = ctx.tools.with_active_extensions(Some(Default::default()));
        }
        let mock = Arc::new(MockBackend::new());
        mock.push_tool_calls(vec![(
            "call".into(),
            "chaz".into(),
            json!({"query": "agent", "period": null}).to_string(),
        )]);
        mock.push_text("done");
        let backend = BackendManager::with_mock(mock.clone(), empty_secrets().await);
        let (security, task) = if mode == "approval-denied" {
            let (security, task) = security_with_decision(ApprovalDecision::Deny);
            (security, Some(task))
        } else {
            (permissive_security(), None)
        };
        runtime::execute(
            Some("mock-model"),
            vec![RuntimeMessage::User("who am I".into())],
            &backend,
            &security,
            &ctx,
            &policies,
            None,
            None,
        )
        .await
        .unwrap();
        if let Some(task) = task {
            task.abort();
        }
        let calls = mock.recorded_calls();
        assert_eq!(calls.len(), 2);
        assert!(!calls[0].tools.iter().any(|t| t.name == "chaz"));
        let output = calls[1]
            .messages
            .iter()
            .find_map(|m| match m {
                RuntimeMessage::ToolResult { content, .. } => Some(content),
                _ => None,
            })
            .unwrap();
        match mode {
            "allowed-hidden" => assert!(output.contains("test-agent"), "{output}"),
            "approval-denied" => assert!(
                output.contains("denied") || output.contains("not approved"),
                "{output}"
            ),
            _ => assert!(output.contains("Unknown tool: chaz"), "{output}"),
        }
    }
}
