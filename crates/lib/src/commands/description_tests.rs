//! Browse-only session descriptions through the shared command path.

use super::super::agent::tests::{cmd_ctx, fixture};
use super::super::{Command, CommandOutcome, dispatch};
use super::collect_session_infos;
use crate::session::{
    MAX_SESSION_DESCRIPTION_CHARS, set_session_description, try_read_meta_from_db,
};

fn text(outcome: CommandOutcome) -> String {
    match outcome {
        CommandOutcome::Text(t) => t,
        CommandOutcome::Error(e) => panic!("expected text, got error: {e}"),
        _ => panic!("expected text outcome"),
    }
}

fn error(outcome: CommandOutcome) -> String {
    match outcome {
        CommandOutcome::Error(e) => e,
        CommandOutcome::Text(t) => panic!("expected error, got text: {t}"),
        _ => panic!("expected error outcome"),
    }
}

#[tokio::test]
async fn session_without_description_reads_as_absent() {
    let (_i, server, registry, secrets, backend, sid, sdb) = fixture().await;
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

    let meta = try_read_meta_from_db(&sdb).await.unwrap();
    assert_eq!(meta.description, None);
    assert!(!text(dispatch(Command::Info, &ctx).await).contains("Description:"));
    let rows = collect_session_infos(&registry).await.unwrap();
    let row = rows.iter().find(|r| r.session_db_id == sid).unwrap();
    assert_eq!(row.description, None);
    // Clearing an absent description is an ordinary no-op success.
    let cleared = text(dispatch(Command::ClearSessionDescription, &ctx).await);
    assert_eq!(cleared, "Session description cleared.");
    // A legacy serialized meta blob without the field still deserializes.
    let legacy: crate::session::SessionMeta =
        serde_json::from_str(r#"{"name":"old","agent_name":null,"host_agent_db_id":null,"model":null,"role_name":null,"role_prompt":null,"backend_name":null,"backend_url":null,"backend_key_ref":null,"burst_budget_override":null,"ambient_participation":null}"#)
            .unwrap();
    assert_eq!(legacy.description, None);
    assert_eq!(legacy.name.as_deref(), Some("old"));
}

#[tokio::test]
async fn description_persists_and_clears_without_touching_the_name() {
    let (instance, server, registry, secrets, backend, sid, sdb) = fixture().await;
    registry
        .set_session_name(&sid, "planning".into())
        .await
        .unwrap();
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);

    let set = text(
        dispatch(
            Command::DescribeSession("  Q3 budget with finance  ".into()),
            &ctx,
        )
        .await,
    );
    assert_eq!(set, "Session description set: Q3 budget with finance");

    // Persisted canonically: a separate login opens a fresh handle.
    let reloaded_user = instance.login_account("test", None).await.unwrap();
    let reopened = reloaded_user.open_database(sdb.root_id()).await.unwrap();
    let meta = try_read_meta_from_db(&reopened).await.unwrap();
    assert_eq!(meta.description.as_deref(), Some("Q3 budget with finance"));
    assert_eq!(meta.name.as_deref(), Some("planning"));

    // Name index and identifier resolution are unchanged.
    assert_eq!(
        registry.find_by_name("planning").await.unwrap(),
        Some(sid.clone())
    );
    let (_, resolved) = registry.resolve_session("planning").await.unwrap();
    assert_eq!(resolved.root_id().to_string(), sid);

    let info_ctx = super::super::CommandContext {
        session_name: Some("planning"),
        ..cmd_ctx(&server, &secrets, &backend, &sid, &sdb)
    };
    let info = text(dispatch(Command::Info, &info_ctx).await);
    assert!(
        info.contains("\nName: planning\nDescription: Q3 budget with finance\n"),
        "{info}"
    );
    let rows = collect_session_infos(&registry).await.unwrap();
    let row = rows.iter().find(|r| r.session_db_id == sid).unwrap();
    assert_eq!(row.description.as_deref(), Some("Q3 budget with finance"));
    assert_eq!(row.name.as_deref(), Some("planning"));

    let cleared = text(dispatch(Command::ClearSessionDescription, &ctx).await);
    assert_eq!(cleared, "Session description cleared.");
    let meta = try_read_meta_from_db(&sdb).await.unwrap();
    assert_eq!(meta.description, None);
    assert_eq!(meta.name.as_deref(), Some("planning"));
    assert!(!text(dispatch(Command::Info, &info_ctx).await).contains("Description:"));
}

#[tokio::test]
async fn invalid_descriptions_are_refused_and_leave_the_stored_value() {
    let (_i, server, _registry, secrets, backend, sid, sdb) = fixture().await;
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
    text(dispatch(Command::DescribeSession("kept".into()), &ctx).await);

    let too_long = "x".repeat(MAX_SESSION_DESCRIPTION_CHARS + 1);
    let refusals = [
        (too_long.as_str(), "limit is 200"),
        ("first\nsecond", "single line"),
        ("tab\there", "single line"),
        ("bell\u{7}", "single line"),
    ];
    for (input, expected) in refusals {
        let e = error(dispatch(Command::DescribeSession(input.into()), &ctx).await);
        assert!(e.contains(expected), "{input:?}: {e}");
        let meta = try_read_meta_from_db(&sdb).await.unwrap();
        assert_eq!(meta.description.as_deref(), Some("kept"), "{input:?}");
    }
    // Extension callers share the validator: whitespace-only is not a clear.
    let e = set_session_description(&sdb, Some("   "))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("empty"), "{e}");
    assert_eq!(
        try_read_meta_from_db(&sdb)
            .await
            .unwrap()
            .description
            .as_deref(),
        Some("kept")
    );

    // The bound counts characters, not bytes.
    let wide = "界".repeat(MAX_SESSION_DESCRIPTION_CHARS);
    text(dispatch(Command::DescribeSession(wide.clone()), &ctx).await);
    assert_eq!(
        try_read_meta_from_db(&sdb).await.unwrap().description,
        Some(wide)
    );
}

#[tokio::test]
async fn read_only_participant_cannot_write_a_description() {
    let (instance, server, _registry, secrets, backend, sid, sdb) = fixture().await;
    let ctx = cmd_ctx(&server, &secrets, &backend, &sid, &sdb);
    text(dispatch(Command::DescribeSession("owner text".into()), &ctx).await);

    // A separate user holding only a Read key on this session.
    let owner = instance.login_account("test", None).await.unwrap();
    owner
        .admin()
        .await
        .unwrap()
        .create_account(eidetica::NewAccount::passwordless("reader"))
        .await
        .unwrap();
    let mut reader = instance.login_account("reader", None).await.unwrap();
    let read_key = reader.add_private_key(Some("reader")).await.unwrap();
    let txn = sdb.new_transaction().await.unwrap();
    txn.get_settings()
        .unwrap()
        .set_auth_key(
            &read_key,
            eidetica::auth::AuthKey::active(
                Some("reader"),
                eidetica::auth::types::Permission::Read,
            ),
        )
        .await
        .unwrap();
    txn.commit().await.unwrap();
    reader
        .map_key(
            &read_key,
            sdb.root_id(),
            eidetica::auth::SigKey::from_pubkey(&read_key),
        )
        .await
        .unwrap();
    let read_db = reader
        .open_database_with_key(sdb.root_id(), &read_key)
        .await
        .unwrap();
    assert_eq!(
        read_db.current_permission().await.unwrap(),
        eidetica::auth::types::Permission::Read
    );

    let reader_ctx = cmd_ctx(&server, &secrets, &backend, &sid, &read_db);
    for cmd in [
        Command::DescribeSession("reader text".into()),
        Command::ClearSessionDescription,
    ] {
        let e = error(dispatch(cmd, &reader_ctx).await);
        assert!(e.starts_with("Failed to update description"), "{e}");
    }
    assert!(
        set_session_description(&read_db, Some("direct"))
            .await
            .is_err()
    );
    assert_eq!(
        try_read_meta_from_db(&sdb)
            .await
            .unwrap()
            .description
            .as_deref(),
        Some("owner text")
    );
}
