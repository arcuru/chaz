use super::*;
use crate::{
    agent_db::{AgentDb, AgentDbConfig, AgentMeta, create_agent_db},
    session::{
        CallInput, CallMessageBinding,
        conversation::staging::{
            Accounting, Completion, ModelExecution, NativeResponse, Observation, Payload, Staging,
            View, stage_observation,
        },
    },
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
    let cancel = TurnCancel::new();
    let (input, _, recorder) = Box::pin(Session::start_pinned_fixture(
        session.clone(),
        first.clone(),
        message("user", "2 + 2"),
        AGENT.into(),
        cancel.clone(),
    ))
    .await
    .unwrap();
    let accepted = Session::history(&db).await.unwrap();
    assert_eq!(
        accepted
            .accepted_agent_pin(&input)
            .await
            .unwrap()
            .unwrap()
            .0,
        &a
    );
    drop(accepted);
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
    assert_eq!(first_call.tools.len(), 1);
    assert_eq!(first_call.accepted_additions.len(), 2); // the hook and the tool
    assert_eq!(last_call.accepted_additions, first_call.accepted_additions);
    assert_eq!(last_call.tools, first_call.tools);
    let foreign = first_call.clone();
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
    let cancel = TurnCancel::new();
    let (next_input, next, recorder) = Box::pin(Session::start_pinned_fixture(
        current_session.clone(),
        first,
        message("user", "Next request"),
        AGENT.into(),
        cancel.clone(),
    ))
    .await
    .unwrap();
    assert_eq!(
        Session::history(&db)
            .await
            .unwrap()
            .accepted_agent_pin(&next_input)
            .await
            .unwrap()
            .unwrap()
            .0,
        &b
    );
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
    let history = Session::history(&db).await.unwrap();
    let call = history
        .recorded_call(&fx.call(0), &next.attempt_id)
        .await
        .unwrap()
        .unwrap();
    Box::pin(check_readback_bindings(&db, call, &foreign, &next)).await;
}

async fn check_readback_bindings(
    db: &Database,
    call: &CallInput,
    foreign: &CallInput,
    attempt: &TurnAttempt,
) {
    // Separate signed siblings of the valid model result let each readback
    // vary exactly one binding. The foreign attempt belongs to the same Agent.
    let hook = foreign
        .messages
        .iter()
        .find_map(|slot| match slot {
            CallMessageBinding::AcceptedAddition(_) => Some(slot.clone()),
            _ => None,
        })
        .unwrap();
    let at = &call.observed.frontier;
    let mut violations = Vec::new();
    let mut good_model = None;
    for (label, message, tools) in [
        ("valid model", None, Vec::new()),
        ("foreign hook", Some(hook), Vec::new()),
        ("foreign schema", None, foreign.tools.clone()),
    ] {
        let txn = db.new_transaction_at(at).await.unwrap();
        let staged = Staging::at(db, at, &txn, &call.call.branch).await.unwrap();
        let op = staged
            .append(vec![(
                Payload::NativeModelResponse {
                    response: NativeResponse {
                        call: call.call.clone(),
                        attempt: call.attempt.clone(),
                        content: Some(BODY.into()),
                        tool_calls: Vec::new(),
                        provider_extra: Default::default(),
                        metadata: None,
                    },
                },
                View::NativeExchange,
            )])
            .await
            .unwrap();
        let response = op.fact_id(0);
        let mut messages = call.messages.clone();
        if let Some(slot) = message {
            messages.insert(1, slot);
        }
        stage_observation(
            &txn,
            Observation::ModelExecuted {
                result: Box::new(ModelExecution {
                    response: response.clone(),
                    requested_model: call.requested_model.clone(),
                    cache: call.effective_nonsecret_settings["cache"].clone(),
                    messages,
                    tools,
                    accounting: Box::new(Accounting {
                        agent: AGENT.into(),
                        timestamp: Utc::now(),
                        request_budget_tokens: None,
                        input_tokens: 12,
                        output_tokens: 3,
                        prices: None,
                    }),
                }),
            },
        )
        .await
        .unwrap();
        let entry = txn.commit().await.unwrap();
        let at = Snapshot::from([entry.clone()]);
        let read = Session::history_at(db, &at).await.unwrap();
        let result = read.recorded_call_entry(&entry).await;
        if label == "valid model" {
            assert!(result.unwrap().is_some());
            good_model = Some((at, response));
        } else if !result.is_err_and(|err| err.to_string().contains("another attempt")) {
            violations.push(label);
        }
    }
    let (at, response) = good_model.unwrap();
    for sender in [Some(AGENT), None, Some(" \t")] {
        let txn = db.new_transaction_at(&at).await.unwrap();
        let staged = Staging::at(db, &at, &txn, &call.call.branch).await.unwrap();
        let publication = staged.publish(&response, BODY).await.unwrap();
        stage_observation(
            &txn,
            Observation::TurnCompleted {
                completion: Completion {
                    response: response.clone(),
                    publication,
                    attempt: call.attempt.clone(),
                    sender: sender.map(str::to_owned),
                },
            },
        )
        .await
        .unwrap();
        txn.get_store::<Table<TurnAttempt>>(crate::session::TURN_ATTEMPTS_STORE)
            .await
            .unwrap()
            .set(
                &call.attempt,
                TurnAttempt {
                    status: crate::session::TurnAttemptStatus::Completed,
                    completed_at: Some(Utc::now()),
                    ..attempt.clone()
                },
            )
            .await
            .unwrap();
        let cut = Snapshot::from([txn.commit().await.unwrap()]);
        let read = Session::history_at(db, &cut).await.unwrap();
        let result = read.display_rows().await;
        if sender == Some(AGENT) {
            assert_eq!(result.unwrap().last().unwrap().sender, AGENT);
        } else if !result.is_err_and(|err| err.to_string().contains("completion sender")) {
            violations.push(if sender.is_none() {
                "missing sender"
            } else {
                "blank sender"
            });
        }
    }
    assert!(
        violations.is_empty(),
        "readback did not reject {violations:?}"
    );
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
        Box::pin(Session::start_pinned_fixture(
            session,
            first,
            message("user", "No winner"),
            AGENT.into(),
            TurnCancel::new()
        ))
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
        Box::pin(Session::start_pinned_fixture(
            session.clone(),
            first.clone(),
            invalid,
            AGENT.into(),
            TurnCancel::new()
        ))
        .await
        .is_err()
    );

    // A started row is inherited at acceptance, and the receipt Entry writes
    // a different attempt. Checking subtree membership and merged rows alone
    // would accept it. Build a valid fact at these very parents.
    // Build a valid fact in a transaction that is never committed.
    let tip = db.snapshot().await.unwrap();
    let draft = db.new_transaction_at(&tip).await.unwrap();
    let staged = Staging::at(&db, &tip, &draft, "fixture").await.unwrap();
    let (pin, _) = staged.agent_pin(&first.id()).unwrap();
    let (op, accepted, attempt) = staged
        .accept_pinned_input(message("user", "must be atomic"), &first.id(), pin)
        .await
        .unwrap();
    drop(staged);
    drop(draft);
    let txn = db.new_transaction().await.unwrap();
    let store = txn
        .get_store::<Table<TurnAttempt>>(crate::session::TURN_ATTEMPTS_STORE)
        .await
        .unwrap();
    store
        .set(&attempt.attempt_id, attempt.clone())
        .await
        .unwrap();
    let at = Snapshot::from([txn.commit().await.unwrap()]);
    let txn = db.new_transaction_at(&at).await.unwrap();
    let staged = Staging::at(&db, &at, &txn, &accepted.branch).await.unwrap();
    let mut fact = op;
    fact.observed.frontier = at.clone();
    staged.stage_fixture_op(&fact).await.unwrap();
    stage_observation(&txn, Observation::InputAccepted { input: accepted })
        .await
        .unwrap();
    txn.get_store::<Table<TurnAttempt>>(crate::session::TURN_ATTEMPTS_STORE)
        .await
        .unwrap()
        .insert(attempt)
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
async fn pinned_fixture_repeated_wake_and_lost_ack_do_not_dispatch_again() {
    let (instance, _, db) = crate::session::test_helpers::test_session_db().await;
    let first = agent(&instance, "first", A).await;
    let session = session(&db).await;
    Session::fixture_pin_agent(&session, &first, false)
        .await
        .unwrap();
    // With nothing unsettled, the Agent's own reply still owes nothing.
    let pinned = db.snapshot().await.unwrap();
    let own = Box::pin(Session::start_pinned_fixture(
        session.clone(),
        first.clone(),
        message(AGENT, "my own reply"),
        AGENT.into(),
        TurnCancel::new(),
    ))
    .await
    .err()
    .unwrap();
    assert!(own.to_string().contains("own reply"), "{own}");
    assert_eq!(db.snapshot().await.unwrap(), pinned);
    // Two concurrent wakes for the same Session: the executor accepts once.
    let (one, two) = tokio::join!(
        Box::pin(Session::start_pinned_fixture(
            session.clone(),
            first.clone(),
            message("user", "accept once"),
            AGENT.into(),
            TurnCancel::new()
        )),
        Box::pin(Session::start_pinned_fixture(
            session.clone(),
            first.clone(),
            message("user", "accept once"),
            AGENT.into(),
            TurnCancel::new()
        )),
    );
    assert_ne!(one.is_ok(), two.is_ok());
    let refused = one.as_ref().err().or(two.as_ref().err()).unwrap();
    assert!(
        refused.to_string().contains("unsettled attempt"),
        "{refused}"
    );
    let (entry, ..) = one.ok().or(two.ok()).unwrap();
    let accepted = Snapshot::from([entry.clone()]);
    assert_eq!(db.snapshot().await.unwrap(), accepted);

    // The caller lost the acknowledgement and resubmits: the durable Started
    // attempt says the Agent already owes this response, so nothing new runs.
    let retry = Box::pin(Session::start_pinned_fixture(
        session,
        first,
        message("user", "accept once"),
        AGENT.into(),
        TurnCancel::new(),
    ))
    .await
    .err()
    .unwrap();
    assert!(retry.to_string().contains("unsettled attempt"), "{retry}");
    let history = Session::history(&db).await.unwrap();
    assert!(history.accepted_agent_pin(&entry).await.unwrap().is_some());
    assert_eq!(history.snapshot(), &accepted);
}

#[tokio::test]
async fn pinned_fixture_unsettled_attempt_only_holds_its_own_agent() {
    let (instance, _, db) = crate::session::test_helpers::test_session_db().await;
    let first = agent(&instance, "first", A).await;
    let other = agent(&instance, "other", OTHER).await;
    let session = session(&db).await;
    for agent in [&first, &other] {
        Session::fixture_pin_agent(&session, agent, false)
            .await
            .unwrap();
    }
    let start = |agent: &AgentDb, display: &str| {
        Box::pin(Session::start_pinned_fixture(
            session.clone(),
            agent.clone(),
            message("user", "both of you"),
            display.into(),
            TurnCancel::new(),
        ))
    };
    // The first Agent's attempt stays Started; the other Agent still owes
    // its own response, and the first still owes nothing new.
    let (held, ..) = start(&first, AGENT).await.unwrap();
    let (answered, ..) = start(&other, "Other").await.unwrap();
    let refused = start(&first, AGENT).await.err().unwrap();
    assert!(
        refused.to_string().contains("unsettled attempt"),
        "{refused}"
    );
    let history = Session::history(&db).await.unwrap();
    for (input, agent) in [(&held, &first), (&answered, &other)] {
        let (pin, _) = history.agent_pin(&agent.id()).await.unwrap().unwrap();
        assert_eq!(
            history.accepted_agent_pin(input).await.unwrap().unwrap().0,
            pin
        );
    }
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
    let error = Box::pin(Session::start_pinned_fixture(
        session,
        first,
        message("user", "no fallback"),
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
        let cancel = TurnCancel::new();
        let (input, _, recorder) = Box::pin(Session::start_pinned_fixture(
            session.clone(),
            first,
            message("user", "2 + 2"),
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

#[tokio::test]
async fn pinned_fixture_peer_agent_is_named_peer_not_own_history() {
    tokio::task::unconstrained(Box::pin(sequential_agents())).await;
}

/// Run one pinned turn to publication. Returns the fixture for wire evidence.
async fn run_pinned(
    instance: &Instance,
    session: &Arc<Mutex<Session>>,
    agent: &AgentDb,
    display: &str,
    question: &str,
    with_tool: bool,
    responder: impl wiremock::Respond + 'static,
) -> Fixture {
    let cancel = TurnCancel::new();
    let (.., recorder) = Box::pin(Session::start_pinned_fixture(
        session.clone(),
        agent.clone(),
        message("user", question),
        display.into(),
        cancel.clone(),
    ))
    .await
    .unwrap();
    let fx = Box::pin(Fixture::with_recorder(
        instance.clone(),
        session.clone(),
        Arc::new(recorder),
        with_tool,
        None,
        cancel,
        responder,
    ))
    .await;
    let outcome = Box::pin(fx.run()).await.unwrap();
    let mut guard = session.lock().await;
    Box::pin(
        fx.recorder
            .complete(&mut guard, Some(message(display, &outcome.body)), None),
    )
    .await
    .unwrap();
    drop(guard);
    fx
}

async fn sequential_agents() {
    let disk = tempfile::tempdir().unwrap();
    let (instance, db) = sqlite_fixture_db(&disk).await;
    let first = agent(&instance, "first", A).await;
    let other = agent(&instance, "other", OTHER).await;
    let session = session(&db).await;
    Session::fixture_pin_agent(&session, &first, false)
        .await
        .unwrap();
    Session::fixture_pin_agent(&session, &other, false)
        .await
        .unwrap();

    // A uses its tool, then publishes. Its own follow-up still replays the
    // native exchange, so the hiding below is reader-specific.
    let a = Box::pin(run_pinned(
        &instance,
        &session,
        &first,
        AGENT,
        "2 + 2",
        true,
        |req: &Request| stub_response(true, req),
    ))
    .await;
    let a_wire = a.assert_effects(2, 1, 1).await;
    assert!(
        a_wire[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["tool_calls"][0]["id"] == "wire-1")
    );

    let b = Box::pin(run_pinned(
        &instance,
        &session,
        &other,
        "other",
        Q_CAPITAL,
        false,
        two_answer_response,
    ))
    .await;
    let b_wire = b.assert_effects(1, 0, 0).await;
    let messages = b_wire[0]["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], OTHER);
    assert!(
        messages.iter().all(|m| m["role"] != "assistant"
            && m["role"] != "tool"
            && m.get("tool_calls").is_none()),
        "{messages:#?}"
    );
    assert!(!serde_json::to_string(messages).unwrap().contains("wire-1"));
    let users: Vec<_> = messages[1..]
        .iter()
        .map(|m| {
            assert_eq!(m["role"], "user");
            m["content"].as_str().unwrap()
        })
        .collect();
    assert_eq!(users.len(), 3, "{users:?}");
    assert_eq!(users[0], "2 + 2");
    let peer: Value = serde_json::from_str(users[1]).unwrap();
    assert_eq!(
        peer,
        json!({"kind": "local_message", "sender": AGENT, "body": BODY})
    );
    assert_eq!(users[2], Q_CAPITAL);
    let (root, first_id) = (db.root_id().clone(), first.id());
    drop((a, b, session, first, other, db, instance));

    let reopened = reopen(&disk).await;
    let user = reopened
        .login_account("test", "test-password")
        .await
        .unwrap();
    let db = user.open_database(&root).await.unwrap();
    let history = Session::history(&db).await.unwrap();
    let rows: Vec<_> = history
        .display_rows()
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.sender, row.content))
        .collect();
    assert_eq!(
        rows,
        [
            ("user", "2 + 2"),
            (AGENT, BODY),
            ("user", Q_CAPITAL),
            ("other", A_CAPITAL),
        ]
        .map(|(s, c)| (s.to_string(), c.to_string()))
    );
    // The reopened context still renders the same peer view for the next
    // request, and A's own view keeps its native tool exchange.
    let current = super::pinned::session(&db).await;
    let first = AgentDb::from_database(user.open_database(&first_id).await.unwrap());
    let c = Box::pin(run_pinned(
        &reopened,
        &current,
        &first,
        AGENT,
        "Anything else?",
        false,
        |req: &Request| stub_response(false, req),
    ))
    .await;
    let wire = c.assert_effects(1, 0, 0).await;
    let messages = wire[0]["messages"].as_array().unwrap();
    assert_eq!(messages[0]["content"], A);
    assert!(
        messages
            .iter()
            .any(|m| m["tool_calls"][0]["id"] == "wire-1")
    );
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "assistant" && m["content"] == BODY)
    );
    let peer = messages
        .iter()
        .filter_map(|m| serde_json::from_str::<Value>(m["content"].as_str()?).ok())
        .find(|m| m["kind"] == "local_message")
        .unwrap();
    assert_eq!(peer["sender"], "other");
    assert_eq!(peer["body"], A_CAPITAL);
}
