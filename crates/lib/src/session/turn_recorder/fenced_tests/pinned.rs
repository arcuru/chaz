use super::*;
use crate::{
    agent_db::{AgentDb, AgentDbConfig, AgentMeta, create_agent_db},
    session::conversation::staging::{Observation, Staging, stage_observation},
};

const A: &str = "First Agent's original instructions";
const B: &str = "First Agent's updated instructions";
const OTHER: &str = "Second Agent's instructions";
const DURING_TOOL: &str = "Steering accepted while the tool runs";

fn config(prompt: &str) -> AgentDbConfig {
    AgentDbConfig {
        system_prompt: prompt.into(),
        ..Default::default()
    }
}

async fn session(db: &Database) -> Arc<Mutex<Session>> {
    // Normal Session construction intentionally refuses v1 writers. The
    // fixture opens only its own database, with no legacy in-memory cache.
    Arc::new(Mutex::new(Session {
        conversation_id: crate::types::ConversationId("pinned".into()),
        database: db.clone(),
        signing_key: None,
        entries: Vec::new(),
        entry_ids: Vec::new(),
        store_name: "entries".into(),
    }))
}

async fn agent(instance: &Instance, name: &str, prompt: &str) -> AgentDb {
    let mut account = instance
        .login_account("test", "test-password")
        .await
        .unwrap();
    create_agent_db(&mut account, name, &config(prompt), &AgentMeta::default())
        .await
        .unwrap()
        .0
}

async fn reopen(disk: &tempfile::TempDir) -> Instance {
    Instance::connect(&format!(
        "sqlite://{}?mode=rwc",
        disk.path().join("fenced.db").display()
    ))
    .await
    .unwrap()
}

#[tokio::test]
async fn pinned_fixture_sqlite_uses_historical_config_and_fresh_causal_context() {
    tokio::task::unconstrained(Box::pin(sqlite_pins_and_steering())).await;
}

async fn sqlite_pins_and_steering() {
    let disk = tempfile::tempdir().unwrap();
    let (instance, db) = sqlite_fixture_db(&disk).await;
    let first = agent(&instance, "first", A).await;
    let other = agent(&instance, "other", OTHER).await;
    let initial = session(&db).await;
    let a = Session::fixture_pin_agent(&initial, &first, false)
        .await
        .unwrap();
    let other_pin = Session::fixture_pin_agent(&initial, &other, false)
        .await
        .unwrap();
    first.write_config(&config(B)).await.unwrap();
    let (root, first_id, other_id) = (db.root_id().clone(), first.id(), other.id());
    drop((initial, first, other, db, instance));

    let instance = reopen(&disk).await;
    let account = instance
        .login_account("test", "test-password")
        .await
        .unwrap();
    let db = account.open_database(&root).await.unwrap();
    let first = AgentDb::from_database(account.open_database(&first_id).await.unwrap());
    let other = AgentDb::from_database(account.open_database(&other_id).await.unwrap());
    let session = session(&db).await;
    let prepared = Session::prepare_pinned_fixture(&session, &first, message("user", "2 + 2"))
        .await
        .unwrap();
    assert_eq!(prepared.accepted.pin.as_ref(), Some(&a));
    let cancel = TurnCancel::new();
    let (input, recorder) = Box::pin(Session::start_pinned_fixture(
        session.clone(),
        &prepared,
        first.clone(),
        AGENT.into(),
        cancel.clone(),
    ))
    .await
    .unwrap();
    let fx = Arc::new(
        Box::pin(Fixture::with_recorder(
            instance.clone(),
            session.clone(),
            Arc::new(recorder),
            true,
            None,
            cancel,
            |req: &Request| stub_response(true, req),
        ))
        .await,
    );

    let (seen, release) = pause_first_call(&fx, true).await;
    let (tool_entered, wait_for_tool) = tokio::sync::oneshot::channel();
    let (finish_tool, tool_release) = tokio::sync::oneshot::channel();
    *fx.tool_pause.lock().unwrap() = Some((tool_entered, tool_release));
    let worker = tokio::spawn({
        let fx = fx.clone();
        async move { Box::pin(fx.run()).await }
    });
    let (result, (correction, b, tool_cut, tool_steering)) = tokio::join!(
        worker,
        Box::pin(async {
            let sent = tokio::time::timeout(Duration::from_secs(30), seen)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(sent["messages"][0]["content"], A);
            assert!(
                Box::pin(fx.run())
                    .await
                    .unwrap_err()
                    .contains("cannot execute twice")
            );
            // Nothing from the model execution, not even dispatch intent or
            // repeated instructions, is written before the recorded return.
            assert_eq!(
                db.snapshot().await.unwrap(),
                Snapshot::from([input.clone()])
            );
            let correction = append_correction(&db, &Snapshot::from([input.clone()])).await;
            let b = Session::fixture_pin_agent(&session, &first, true)
                .await
                .unwrap();
            release.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(30), wait_for_tool)
                .await
                .unwrap()
                .unwrap();
            let tool_cut = db.snapshot().await.unwrap();
            let tool_steering = append_steering(&db, &tool_cut, DURING_TOOL).await;
            finish_tool.send(()).unwrap();
            (correction, b, tool_cut, tool_steering)
        })
    );
    let outcome = result.unwrap().unwrap();
    fx.complete(&outcome.body).await.unwrap();
    let wire = fx.assert_effects(2, 1, 1).await;
    let first_wire = wire[0]["messages"].as_array().unwrap();
    let second_wire = wire[1]["messages"].as_array().unwrap();
    assert_eq!(second_wire[0]["content"], A);
    assert!(!first_wire.iter().any(|m| m["content"] == CORRECTION));
    assert!(second_wire.iter().any(|m| m["content"] == CORRECTION));
    assert!(second_wire.iter().any(|m| m["content"] == DURING_TOOL));
    let call = second_wire
        .iter()
        .position(|m| m["tool_calls"][0]["id"] == "wire-1")
        .unwrap();
    assert_eq!(second_wire[call + 1]["tool_call_id"], "wire-1");

    let current = Session::history(&db).await.unwrap();
    let attempt = &fx.recorder.attempt.attempt_id;
    let first_call = current
        .recorded_call(&fx.call(0), attempt)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        first_call.observed.frontier,
        Snapshot::from([input.clone()])
    );
    assert!(first_call.sources.is_empty());
    assert_eq!(
        first_call.messages[0],
        crate::session::CallMessageBinding::PinnedInstructions
    );
    let last_call = current
        .recorded_call(&fx.call(1), attempt)
        .await
        .unwrap()
        .unwrap();
    let second_parents = last_call.observed.frontier.tips();
    assert_eq!(second_parents.len(), 2);
    let tool_result = second_parents
        .iter()
        .find(|id| **id != tool_steering)
        .unwrap();
    assert!(second_parents.contains(&tool_steering));
    for sibling in [tool_result, &tool_steering] {
        let entry = db.get_entry(sibling).await.unwrap();
        assert_eq!(Snapshot::from(entry.parents().unwrap()), tool_cut);
        assert!(entry.in_subtree("conversation_v1"));
    }
    let execution_entries = crate::session::execution::store_entries_at(
        &db,
        "conversation_provenance_v1",
        current.snapshot(),
    )
    .await
    .unwrap();
    let mut model_entries = Vec::new();
    for entry in execution_entries {
        if let Some(call) = current.recorded_call_entry(entry.id_ref()).await.unwrap() {
            assert_eq!(
                Snapshot::from(entry.parents().unwrap()),
                call.observed.frontier
            );
            model_entries.push((call.call.model_sequence, entry.id()));
        }
    }
    model_entries.sort();
    assert_eq!(model_entries.len(), 2);
    let first_result = &model_entries[0].1;
    assert_ne!(first_result, &correction);
    assert_eq!(
        db.get_entry(&correction).await.unwrap().parents().unwrap(),
        std::slice::from_ref(&input)
    );
    let second_cut = Session::history_at(&db, &last_call.observed.frontier)
        .await
        .unwrap();
    assert!(
        second_cut
            .recorded_call_entry(first_result)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        second_cut.agent_pin(&first_id).await.unwrap().unwrap().0,
        &b
    );
    assert_eq!(
        current.accepted_agent_pin(&input).await.unwrap().unwrap().0,
        &a
    );
    assert_eq!(
        current.agent_pin(&other_id).await.unwrap().unwrap().0,
        &other_pin
    );

    // Lost acknowledgement is not a second start; exact durable receipt
    // remains readable even when current pins and execution history change.
    assert_eq!(
        prepared.readback(&db, current.snapshot()).await.unwrap(),
        Some(input.clone())
    );
    assert!(
        Box::pin(Session::start_pinned_fixture(
            session.clone(),
            &prepared,
            first.clone(),
            AGENT.into(),
            TurnCancel::new()
        ))
        .await
        .err()
        .unwrap()
        .to_string()
        .contains("already accepted")
    );
    assert!(
        Box::pin(fx.run())
            .await
            .unwrap_err()
            .contains("cannot execute twice")
    );
    fx.assert_effects(2, 1, 1).await;
    let first_result = first_result.clone();
    drop((current, second_cut));
    drop((fx, session, first, other, db, account, instance));

    let reopened = reopen(&disk).await;
    let user = reopened
        .login_account("test", "test-password")
        .await
        .unwrap();
    let db = user.open_database(&root).await.unwrap();
    let first = AgentDb::from_database(user.open_database(&first_id).await.unwrap());
    let history = Session::history(&db).await.unwrap();
    assert!(
        history
            .recorded_call_entry(&first_result)
            .await
            .unwrap()
            .is_some()
    );
    let (_, old) = history.accepted_agent_pin(&input).await.unwrap().unwrap();
    assert_eq!(first.read_config_ref(old).await.unwrap().system_prompt, A);
    let (latest, reference) = history.agent_pin(&first_id).await.unwrap().unwrap();
    assert_eq!(latest, &b);
    assert_eq!(
        first
            .read_config_ref(reference)
            .await
            .unwrap()
            .system_prompt,
        B
    );
    let (_, reference) = history.agent_pin(&other_id).await.unwrap().unwrap();
    let other = AgentDb::from_database(user.open_database(&other_id).await.unwrap());
    assert_eq!(
        other
            .read_config_ref(reference)
            .await
            .unwrap()
            .system_prompt,
        OTHER
    );
    let current_session = super::pinned::session(&db).await;
    let next =
        Session::prepare_pinned_fixture(&current_session, &first, message("user", "Next request"))
            .await
            .unwrap();
    assert_eq!(next.accepted.pin.as_ref(), Some(&b));
    let cancel = TurnCancel::new();
    let (_, recorder) = Box::pin(Session::start_pinned_fixture(
        current_session.clone(),
        &next,
        first,
        AGENT.into(),
        cancel.clone(),
    ))
    .await
    .unwrap();
    let fx = Box::pin(Fixture::with_recorder(
        reopened,
        current_session,
        Arc::new(recorder),
        false,
        None,
        cancel,
        |req: &Request| stub_response(false, req),
    ))
    .await;
    assert!(Box::pin(fx.run()).await.is_ok());
    let wire = fx.assert_effects(1, 0, 0).await;
    assert_eq!(wire[0]["messages"][0]["content"], B);
}

#[tokio::test]
async fn pinned_fixture_conflicting_updates_do_not_choose_a_winner() {
    let (instance, _, db) = crate::session::test_helpers::test_session_db().await;
    let first = agent(&instance, "first", A).await;
    let other = agent(&instance, "other", OTHER).await;
    let session = session(&db).await;
    Session::fixture_pin_agent(&session, &first, false)
        .await
        .unwrap();
    let other_pin = Session::fixture_pin_agent(&session, &other, false)
        .await
        .unwrap();
    let base = db.snapshot().await.unwrap();
    for prompt in ["one sibling", "other sibling"] {
        first.write_config(&config(prompt)).await.unwrap();
        let (_, reference) = first
            .read_config_bound(&first.database().snapshot().await.unwrap())
            .await
            .unwrap();
        let txn = db.new_transaction_at(&base).await.unwrap();
        crate::session::conversation::pins::stage(&txn, reference, true)
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }
    let joined = Session::history(&db).await.unwrap();
    assert_eq!(joined.snapshot().tips().len(), 2);
    assert!(
        joined
            .agent_pin(&first.id())
            .await
            .unwrap_err()
            .to_string()
            .contains("concurrent pins")
    );
    assert_eq!(
        joined.agent_pin(&other.id()).await.unwrap().unwrap().0,
        &other_pin
    );
    assert!(
        Session::prepare_pinned_fixture(&session, &first, message("user", "No winner"))
            .await
            .is_err()
    );
    assert_eq!(db.snapshot().await.unwrap(), *joined.snapshot());
}

#[tokio::test]
async fn pinned_fixture_refuses_inherited_attempt_row_and_nonlocal_input() {
    let (instance, _, db) = crate::session::test_helpers::test_session_db().await;
    let first = agent(&instance, "first", A).await;
    let session = session(&db).await;
    Session::fixture_pin_agent(&session, &first, false)
        .await
        .unwrap();
    let mut invalid = message("user", "not local");
    invalid.entry_type = EntryType::Cancel;
    assert!(
        Session::prepare_pinned_fixture(&session, &first, invalid)
            .await
            .is_err()
    );

    // A started row is inherited at acceptance, and the receipt Entry writes
    // a different attempt. Checking subtree membership and merged rows alone
    // would accept it. Build a valid fact at these very parents.
    let plan = Session::prepare_pinned_fixture(&session, &first, message("user", "must be atomic"))
        .await
        .unwrap();
    let txn = db.new_transaction().await.unwrap();
    let store = txn
        .get_store::<Table<TurnAttempt>>(crate::session::TURN_ATTEMPTS_STORE)
        .await
        .unwrap();
    store
        .set(&plan.attempt.attempt_id, plan.attempt.clone())
        .await
        .unwrap();
    let at = Snapshot::from([txn.commit().await.unwrap()]);
    let txn = db.new_transaction_at(&at).await.unwrap();
    let staged = Staging::at(&db, &at, &txn, &plan.accepted.branch)
        .await
        .unwrap();
    let mut fact = plan.op.clone();
    fact.observed.frontier = at.clone();
    staged.stage_fixture_op(&fact).await.unwrap();
    stage_observation(
        &txn,
        Observation::InputAccepted {
            input: plan.accepted.clone(),
        },
    )
    .await
    .unwrap();
    txn.get_store::<Table<TurnAttempt>>(crate::session::TURN_ATTEMPTS_STORE)
        .await
        .unwrap()
        .insert(plan.attempt.clone())
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let issues = Session::history(&db)
        .await
        .unwrap()
        .inspect_conversation()
        .await
        .unwrap()
        .unwrap()
        .issues
        .clone();
    assert!(
        issues.iter().any(|issue| issue
            .to_string()
            .contains("did not write its exact attempt row")),
        "{issues:?}"
    );
}

#[tokio::test]
async fn pinned_fixture_only_one_concurrent_admission_across_session_handles() {
    let (instance, _, db) = crate::session::test_helpers::test_session_db().await;
    let first = agent(&instance, "first", A).await;
    let one = session(&db).await;
    let two = session(&db).await;
    Session::fixture_pin_agent(&one, &first, false)
        .await
        .unwrap();
    let plan = Session::prepare_pinned_fixture(&one, &first, message("user", "accept once"))
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        Box::pin(Session::start_pinned_fixture(
            one,
            &plan,
            first.clone(),
            AGENT.into(),
            TurnCancel::new()
        )),
        Box::pin(Session::start_pinned_fixture(
            two,
            &plan,
            first,
            AGENT.into(),
            TurnCancel::new()
        )),
    );
    assert_ne!(a.is_ok(), b.is_ok());
    let entry = a.ok().or_else(|| b.ok()).unwrap().0;
    assert_eq!(
        db.snapshot().await.unwrap(),
        Snapshot::from([entry.clone()])
    );
    assert_eq!(
        plan.readback(&db, &db.snapshot().await.unwrap())
            .await
            .unwrap(),
        Some(entry)
    );
}

#[tokio::test]
async fn pinned_fixture_unavailable_revision_never_accepts_input() {
    let (instance, _, db) = crate::session::test_helpers::test_session_db().await;
    let first = agent(&instance, "first", A).await;
    let session = session(&db).await;
    Session::fixture_pin_agent(&session, &first, false)
        .await
        .unwrap();
    let history = Session::history(&db).await.unwrap();
    let (_, mut missing) = history
        .agent_pin(&first.id())
        .await
        .unwrap()
        .map(|(id, config)| (id, config.clone()))
        .unwrap();
    missing.frontier = Snapshot::from([ID::from_bytes(b"revision not present in any AgentDB")]);
    let txn = db.new_transaction_at(history.snapshot()).await.unwrap();
    crate::session::conversation::pins::stage(&txn, missing, true)
        .await
        .unwrap();
    let at = Snapshot::from([txn.commit().await.unwrap()]);
    let prepared =
        Session::prepare_pinned_fixture(&session, &first, message("user", "no fallback"))
            .await
            .unwrap();
    let error = Box::pin(Session::start_pinned_fixture(
        session,
        &prepared,
        first,
        AGENT.into(),
        TurnCancel::new(),
    ))
    .await
    .err()
    .unwrap();
    assert!(matches!(
        error.downcast_ref(),
        Some(crate::agent_db::BoundConfigError::Unavailable(_))
    ));
    assert_eq!(prepared.readback(&db, &at).await.unwrap(), None);
    assert_eq!(db.snapshot().await.unwrap(), at);
}

#[tokio::test]
async fn pinned_fixture_refuses_result_and_tools_after_live_revocation() {
    tokio::task::unconstrained(Box::pin(async {
        let (instance, _, db) = crate::session::test_helpers::test_session_db().await;
        let first = agent(&instance, "first", A).await;
        let session = session(&db).await;
        Session::fixture_pin_agent(&session, &first, false)
            .await
            .unwrap();
        let prepared = Session::prepare_pinned_fixture(&session, &first, message("user", "2 + 2"))
            .await
            .unwrap();
        let cancel = TurnCancel::new();
        let (input, recorder) = Box::pin(Session::start_pinned_fixture(
            session.clone(),
            &prepared,
            first,
            AGENT.into(),
            cancel.clone(),
        ))
        .await
        .unwrap();
        let fx = Box::pin(Fixture::with_recorder(
            instance,
            session,
            Arc::new(recorder),
            true,
            None,
            cancel,
            |req: &Request| stub_response(true, req),
        ))
        .await;
        let (seen, release) = pause_first_call(&fx, true).await;
        let (result, revoked) = tokio::join!(
            Box::pin(fx.run()),
            Box::pin(async {
                tokio::time::timeout(Duration::from_secs(30), seen)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(db.snapshot().await.unwrap(), Snapshot::from([input]));
                let txn = db.new_transaction().await.unwrap();
                let key = db.auth_identity().unwrap().hint().pubkey.as_ref().unwrap();
                txn.get_settings()
                    .unwrap()
                    .revoke_auth_key(key)
                    .await
                    .unwrap();
                let revoked = txn.commit().await.unwrap();
                release.send(()).unwrap();
                revoked
            })
        );
        assert!(result.unwrap_err().contains("fenced writer"));
        assert_eq!(db.snapshot().await.unwrap(), Snapshot::from([revoked]));
        Box::pin(fx.assert_effects(1, 0, 1)).await;
    }))
    .await;
}
