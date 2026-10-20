use super::*;
use crate::runtime::{ResponseMetadata, TokenUsage};
use chrono::TimeZone;

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap()
}

fn entry(sender: &str, ty: EntryType, secs: i64, ctx: Option<u32>) -> SessionEntry {
    SessionEntry {
        sender: sender.into(),
        content: String::new(),
        timestamp: at(secs),
        entry_type: ty,
        metadata: ctx.map(|t| ResponseMetadata {
            model: "m-1".into(),
            usage: TokenUsage {
                // Summed across the turn; must not be reported.
                prompt_tokens: t * 3,
                ..Default::default()
            },
            context_tokens: Some(t),
            ..Default::default()
        }),
        routing: None,
    }
}

fn budget(
    tokens: usize,
    source: ContextBudgetSource,
    window: Option<usize>,
    cap: Option<usize>,
) -> ContextBudget {
    ContextBudget {
        model: Some("m-1".into()),
        tokens,
        source,
        model_window: window,
        agent_cap: cap,
        reserved_output_tokens: 4096,
    }
}

#[test]
fn measurement_is_the_agents_latest_final_request() {
    let entries = vec![
        entry("chaz", EntryType::Message, 0, Some(1_000)),
        entry("other", EntryType::Message, 10, Some(9_999)),
        entry("chaz", EntryType::Message, 20, Some(2_000)),
        entry("user", EntryType::Message, 30, None),
        entry("chaz", EntryType::ToolCall, 31, None),
        entry("chaz", EntryType::Message, 40, None),
        entry("other", EntryType::Message, 50, Some(7_777)),
    ];
    let m = last_measurement(&entries, "chaz").unwrap();
    assert_eq!(m.input_tokens, 2_000, "context_tokens, not summed prompt");
    assert_eq!(m.measured_at, at(20));
    assert_eq!(m.model, "m-1");
    // user message, chaz reply without metadata, other agent's message;
    // the ToolCall audit entry does not count.
    assert_eq!(m.newer_entries, 3);
    assert!(last_measurement(&entries, "nobody").is_none());
}

#[test]
fn report_states_numerator_denominator_and_provenance() {
    let report = render(&Snapshot {
        agent: "chaz".into(),
        taken_at: at(125),
        last: Some(Measurement {
            input_tokens: 45_210,
            measured_at: at(5),
            model: "m-1".into(),
            newer_entries: 1,
        }),
        budget: budget(
            200_000,
            ContextBudgetSource::ModelWindow,
            Some(200_000),
            None,
        ),
    });
    assert_eq!(
        report,
        "Context for agent 'chaz' (snapshot 2027-01-15T08:02:05Z)\n\
         Last completed request: 45,210 input tokens, measured 2027-01-15T08:00:05Z (2m ago) on m-1\n\
         Next request: not yet measured; 1 transcript entry added since the measurement\n\
         Context budget: 200,000 tokens for m-1 — model catalog window; 4,096 reserved for the reply\n\
         Model window: 200,000 tokens\n\
         Used: 45,210 / 200,000 = 22.6% of budget"
    );
}

#[test]
fn agent_cap_below_window_reports_both_ratios() {
    let report = render(&Snapshot {
        agent: "chaz".into(),
        taken_at: at(0),
        last: Some(Measurement {
            input_tokens: 25_000,
            measured_at: at(0),
            model: "m-0".into(),
            newer_entries: 0,
        }),
        budget: budget(
            50_000,
            ContextBudgetSource::AgentCap,
            Some(200_000),
            Some(50_000),
        ),
    });
    assert!(report.contains("explicit agent cap (max_context_tokens)"));
    assert!(
        report.contains(
            "Used: 25,000 / 50,000 = 50.0% of budget; 25,000 / 200,000 = 12.5% of model window"
        ),
        "{report}"
    );
    assert!(report.contains("Note: the measurement was taken on m-0, the budget is for m-1"));
}

#[test]
fn unknown_values_stay_unknown() {
    let mut b = budget(128_000, ContextBudgetSource::ConfiguredDefault, None, None);
    b.model = None;
    let report = render(&Snapshot {
        agent: "chaz".into(),
        taken_at: at(0),
        last: None,
        budget: b,
    });
    assert!(report.contains(
        "Last completed request: unknown (no measured reply from this agent in this session)"
    ));
    assert!(report.contains("Next request: not yet measured\n"));
    assert!(report.contains(
        "Context budget: 128,000 tokens for unknown — fallback: configured max_context_tokens default"
    ));
    assert!(report.contains("Model window: unknown"));
    assert!(report.ends_with("Used: unknown"), "{report}");
}

#[test]
fn token_grouping() {
    assert_eq!(tokens(0), "0");
    assert_eq!(tokens(999), "999");
    assert_eq!(tokens(1_000), "1,000");
    assert_eq!(tokens(1_234_567), "1,234,567");
}

#[test]
fn omitted_input_usage_is_unknown_not_a_measured_zero() {
    let mut entries = vec![entry("chaz", EntryType::Message, 0, Some(0))];
    crate::session::usage::mark_usage_fields(
        &mut entries[0].metadata.as_mut().unwrap().extra,
        false,
        true,
    );
    assert!(last_measurement(&entries, "chaz").is_none());
    crate::session::usage::mark_usage_fields(
        &mut entries[0].metadata.as_mut().unwrap().extra,
        true,
        true,
    );
    assert_eq!(last_measurement(&entries, "chaz").unwrap().input_tokens, 0);
}

#[tokio::test]
async fn pending_reports_are_persisted_agent_scoped_and_latest_wins() {
    let (_instance, session) = crate::test_support::fresh_session().await;
    let db = session.lock().await.database().clone();
    assert!(take_pending_report(&db, "alpha").await.unwrap().is_none());
    for (agent, id) in [("alpha", "old"), ("beta", "beta"), ("alpha", "latest")] {
        store_report(
            &db,
            agent,
            &ReportRecord {
                id: id.into(),
                created_at: at(0),
                report: id.into(),
            },
        )
        .await
        .unwrap();
    }
    assert!(
        take_pending_report(&db, "unrelated")
            .await
            .unwrap()
            .is_none()
    );
    let latest = take_pending_report(&db, "alpha").await.unwrap().unwrap();
    assert_eq!(latest.report, "latest");
    assert!(take_pending_report(&db, "alpha").await.unwrap().is_none());
    assert_eq!(
        take_pending_report(&db, "beta")
            .await
            .unwrap()
            .unwrap()
            .report,
        "beta"
    );
    // Delivery lives in a different store: an older delivery can't clobber a
    // new report, without relying on in-memory delivery state.
    store_report(
        &db,
        "alpha",
        &ReportRecord {
            id: "new".into(),
            created_at: at(1),
            report: "new snapshot".into(),
        },
    )
    .await
    .unwrap();
    let reopened = db.clone();
    assert_eq!(
        take_pending_report(&reopened, "alpha")
            .await
            .unwrap()
            .unwrap()
            .report,
        "new snapshot"
    );
    assert!(
        take_pending_report(&reopened, "alpha")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn corrupt_report_or_delivery_is_not_marked_delivered() {
    let (_instance, session) = crate::test_support::fresh_session().await;
    let db = session.lock().await.database().clone();
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<DocStore>(REPORTS_STORE)
        .await
        .unwrap()
        .set_string("alpha", "not-json")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let before = db.snapshot().await.unwrap();
    assert!(take_pending_report(&db, "alpha").await.is_err());
    assert_eq!(before, db.snapshot().await.unwrap());
    store_report(
        &db,
        "alpha",
        &ReportRecord {
            id: "valid".into(),
            created_at: at(0),
            report: "valid report".into(),
        },
    )
    .await
    .unwrap();
    let txn = db.new_transaction().await.unwrap();
    txn.get_store::<DocStore>(DELIVERIES_STORE)
        .await
        .unwrap()
        .set("alpha", 123i64)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let before = db.snapshot().await.unwrap();
    assert!(take_pending_report(&db, "alpha").await.is_err());
    assert_eq!(before, db.snapshot().await.unwrap());
}

#[tokio::test]
async fn tail_reopens_authorized_session_after_delegation_advances() {
    let (_instance, registry) = crate::test_support::fresh_session_registry().await;
    let (agent, key) = registry
        .create_new_agent_db(
            "alpha",
            &Default::default(),
            &crate::agent_db::AgentMeta {
                display_name: Some("alpha".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let (_, owner_db) = registry
        .create_session(Some("context-delegation"))
        .await
        .unwrap();
    let sid = owner_db.root_id().to_string();
    registry
        .attach_agent_to_session(
            &sid,
            &crate::hosted_index::DbEntry {
                db_id: agent.id(),
                display_name: "alpha".into(),
                pubkey: key.clone(),
            },
        )
        .await
        .unwrap();
    registry
        .enable_on_commit_sync_with_key(owner_db.root_id(), &key)
        .await
        .unwrap();
    let cached = registry
        .user_lock()
        .await
        .open_database_with_key(owner_db.root_id(), &key)
        .await
        .unwrap();
    assert!(matches!(
        cached.auth_identity(),
        Some(eidetica::auth::types::SigKey::Delegation { .. })
    ));
    store_report(
        &owner_db,
        "alpha",
        &ReportRecord {
            id: "delegated-report".into(),
            created_at: at(0),
            report: "delegated report".into(),
        },
    )
    .await
    .unwrap();
    // A later agent write advances the delegation observed by ordinary turns.
    let txn = agent.database().new_transaction().await.unwrap();
    txn.get_store::<DocStore>("meta")
        .await
        .unwrap()
        .set_string("advanced", "yes")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    registry
        .enable_on_commit_sync_with_key(owner_db.root_id(), &key)
        .await
        .unwrap();
    let fresh = registry
        .user_lock()
        .await
        .open_database_with_key(owner_db.root_id(), &key)
        .await
        .unwrap();
    let txn = fresh.new_transaction().await.unwrap();
    txn.get_store::<DocStore>("ordinary_turn")
        .await
        .unwrap()
        .set_string("probe", "fresh identity")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let error = take_pending_report(&cached, "alpha").await.unwrap_err();
    assert!(
        error.to_string().contains("Entry validation failed"),
        "{error:#}"
    );
    let tail = PendingReportTail {
        registry: registry.clone(),
        session_db_id: sid.clone(),
    };
    let delivered = tail
        .context_tail(&ContextTailCall {
            agent_name: "alpha",
            recent_message_text: &[],
            session_db_id: Some(sid),
            request_id: None,
        })
        .await
        .unwrap();
    assert!(delivered.is_some_and(|report| report.ends_with("delegated report")));
    assert!(
        take_pending_report(&fresh, "alpha")
            .await
            .unwrap()
            .is_none()
    );
}
