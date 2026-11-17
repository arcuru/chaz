//! Persisted controls through the real shared command handler, with no provider calls.
use super::super::agent::tests::{cmd_ctx, fixture};
use super::super::{Command, CommandOutcome, dispatch};
use crate::{
    backends::BackendManager,
    config::{Config, ReasoningEffort},
    session::{try_read_meta_from_db, update_meta_on_db},
};

fn text(outcome: CommandOutcome) -> String {
    match outcome {
        CommandOutcome::Text(text) => text,
        CommandOutcome::Error(error) => panic!("{error}"),
        _ => panic!("expected text"),
    }
}

#[tokio::test]
async fn reasoning_commands_persist_isolate_cycle_and_restore_per_model() {
    let (instance, server, registry, secrets, _, sid, db) = fixture().await;
    let config: Config = serde_yaml::from_str(r#"
default_reasoning: high
backends:
  - type: openaicompatible
    name: route
    models:
      - name: first
        reasoning: {effort: medium, supported_efforts: [high, none, low, medium, low], format: openai}
      - name: second
        reasoning: {supported_efforts: [low, high]}
  - type: openaicompatible
    name: other
    models:
      - name: first
        reasoning: {effort: high, supported_efforts: [low, high]}
"#).unwrap();
    let backend = BackendManager::from_config(&config, secrets.clone());
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &db);
    text(dispatch(Command::Model(Some("route:first".into())), &ctx).await);
    assert!(
        text(dispatch(Command::Reasoning(None), &ctx).await).contains("medium (model default)")
    );
    for (arg, expected) in [
        ("high", "high (session)"),
        ("cycle", "provider (session)"),
        ("cycle", "none (session)"),
        ("cycle", "low (session)"),
        ("cycle", "medium (session)"),
    ] {
        assert!(
            text(dispatch(Command::Reasoning(Some(arg.into())), &ctx).await).contains(expected)
        );
    }
    let saved = try_read_meta_from_db(&db).await.unwrap();
    assert_eq!(saved.reasoning_overrides.len(), 1);
    assert!(
        matches!(dispatch(Command::Reasoning(Some("xhigh".into())), &ctx).await, CommandOutcome::Error(e) if e.contains("unsupported"))
    );
    assert_eq!(
        try_read_meta_from_db(&db)
            .await
            .unwrap()
            .reasoning_overrides,
        saved.reasoning_overrides
    );
    assert!(
        matches!(dispatch(Command::Reasoning(Some("bogus".into())), &ctx).await, CommandOutcome::Error(e) if e.contains("Usage:"))
    );

    // Reopening, unrelated metadata writes and another conversation preserve isolation.
    let reopened = instance
        .login_account("test", "test-password")
        .await
        .unwrap()
        .open_database(db.root_id())
        .await
        .unwrap();
    assert_eq!(
        try_read_meta_from_db(&reopened)
            .await
            .unwrap()
            .reasoning_overrides,
        saved.reasoning_overrides
    );
    text(dispatch(Command::NameSession("kept".into()), &ctx).await);
    assert_eq!(
        try_read_meta_from_db(&db)
            .await
            .unwrap()
            .reasoning_overrides,
        saved.reasoning_overrides
    );
    let (id2, db2) = registry
        .create_session(Some("other-session"))
        .await
        .unwrap();
    let sid2 = id2.to_string();
    let ctx2 = cmd_ctx(&server, &secrets, &backend, &sid2, &db2);
    assert!(
        text(dispatch(Command::Reasoning(None), &ctx2).await).contains("medium (model default)")
    );
    assert!(
        try_read_meta_from_db(&db2)
            .await
            .unwrap()
            .reasoning_overrides
            .is_empty()
    );

    text(dispatch(Command::Model(Some("route:second".into())), &ctx).await);
    assert!(text(dispatch(Command::Reasoning(None), &ctx).await).contains("high (global default)"));
    text(dispatch(Command::Reasoning(Some("low".into())), &ctx).await);
    text(dispatch(Command::Model(Some("other:first".into())), &ctx).await);
    assert!(text(dispatch(Command::Reasoning(None), &ctx).await).contains("high (model default)"));
    text(dispatch(Command::Model(Some("route:first".into())), &ctx).await);
    assert!(text(dispatch(Command::Reasoning(None), &ctx).await).contains("medium (session)"));
    assert!(
        text(dispatch(Command::Reasoning(Some("provider".into())), &ctx).await)
            .contains("provider (session)")
    );
    assert!(
        text(dispatch(Command::Reasoning(Some("default".into())), &ctx).await)
            .contains("medium (model default)")
    );
    let reset = try_read_meta_from_db(&db).await.unwrap();
    assert_eq!(reset.reasoning_overrides.len(), 1); // second-model selection survives
    assert_eq!(reset.name.as_deref(), Some("kept"));

    // A capability/config edit never silently sends a now-unsupported selection.
    let key = backend.reasoning_key(Some("route:first")).unwrap();
    update_meta_on_db(&db, |m| {
        m.reasoning_overrides
            .insert(key, Some(ReasoningEffort::Max));
    })
    .await
    .unwrap();
    assert!(matches!(
        dispatch(Command::Reasoning(None), &ctx).await,
        CommandOutcome::Error(_)
    ));
    assert!(
        text(dispatch(Command::Reasoning(Some("provider".into())), &ctx).await)
            .contains("provider (session)")
    );
    text(dispatch(Command::Reasoning(Some("reset".into())), &ctx).await);
    // Per-agent model resolution changes only which model's entry gets edited.
    text(
        dispatch(
            Command::AgentModel {
                agent: "chaz".into(),
                model: Some("route:second".into()),
            },
            &ctx,
        )
        .await,
    );
    assert!(text(dispatch(Command::Reasoning(None), &ctx).await).contains("low (session)"));
    // Legacy metadata omits the new field; it remains readable with no override.
    let old: crate::session::SessionMeta = serde_json::from_str("{}").unwrap();
    assert!(old.reasoning_overrides.is_empty());
}
