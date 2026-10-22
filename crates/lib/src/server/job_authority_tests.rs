//! Job authority must come from a readable AgentDb, not a runtime snapshot.

use super::*;
use crate::session::jobs::{AcceptedAgentJob, JobState, read_accepted_job, read_job_rejection};
use eidetica::store::DocStore;

#[derive(Clone, Copy, Debug)]
enum ConfigPayload {
    Healthy,
    WrongType,
    BadJson,
    Missing,
}

struct AuthorityFixture {
    _dir: tempfile::TempDir,
    settings: crate::config::EideticaConfig,
    yaml: crate::config::Config,
    entries: Vec<DbEntry>,
    dbs: Vec<crate::agent_db::AgentDb>,
    healthy: Vec<AgentDbConfig>,
    shutdown: tokio::sync::watch::Sender<()>,
    service: tokio::task::JoinHandle<eidetica::Result<()>>,
}

impl AuthorityFixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("authority.sock");
        let (owner, mut user) = Instance::create_backend(
            Box::new(InMemory::new()),
            NewAccount::passwordless("job-authority"),
        )
        .await
        .unwrap();
        let yaml: crate::config::Config = serde_yaml::from_str(concat!(
            "agents:\n  - name: default\n    max_spawn_depth: 2\n",
            "  - name: target\n    max_spawn_depth: 2\n",
        ))
        .unwrap();
        let denied: crate::grants::Grants = serde_json::from_value(serde_json::json!({
            "shell": {"allow": []}, "network": {"endpoints": []},
            "fs": {"allow_read": [], "allow_write": []}
        }))
        .unwrap();
        let mut entries = Vec::new();
        let mut dbs = Vec::new();
        let mut healthy = Vec::new();
        for (cfg, depth) in yaml.agents.as_ref().unwrap().iter().zip([4, 3]) {
            let mut persisted = AgentDbConfig::from_agent_config(cfg);
            persisted.max_spawn_depth = Some(depth);
            persisted.tools = Some(Vec::new());
            persisted.capabilities = denied.clone();
            let (db, pubkey) = create_agent_db(
                &mut user,
                &cfg.name,
                &persisted,
                &AgentMeta {
                    display_name: Some(cfg.name.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            entries.push(DbEntry {
                db_id: db.id(),
                display_name: cfg.name.clone(),
                pubkey,
            });
            dbs.push(db);
            healthy.push(persisted);
        }
        let server = eidetica::service::ServiceServer::bind(owner, &socket)
            .await
            .unwrap();
        let (shutdown, receiver) = tokio::sync::watch::channel(());
        let service = tokio::spawn(server.run(receiver));
        Self {
            _dir: dir,
            settings: crate::config::EideticaConfig {
                connection: format!("unix://{}", socket.display()),
                login: crate::config::EideticaLoginConfig {
                    username: "job-authority".into(),
                    password: None,
                    passwordless: true,
                },
                sync: None,
            },
            yaml,
            entries,
            dbs,
            healthy,
            shutdown,
            service,
        }
    }

    async fn payload(&self, index: usize, mode: ConfigPayload) {
        use ConfigPayload::*;
        if matches!(mode, Healthy) {
            self.dbs[index]
                .write_config(&self.healthy[index])
                .await
                .unwrap();
        } else {
            let txn = self.dbs[index].database().new_transaction().await.unwrap();
            let store = txn.get_store::<DocStore>("config").await.unwrap();
            match mode {
                WrongType => store.set("value", 42i64).await.unwrap(),
                BadJson => store.set_string("value", "{").await.unwrap(),
                Missing => assert!(store.delete("value").await.unwrap()),
                Healthy => unreachable!(),
            }
            txn.commit().await.unwrap();
        }
        // Positive fault controls: absence alone is allowed to default.
        let read = self.dbs[index].read_config().await;
        match mode {
            Healthy => assert_eq!(read.unwrap(), self.healthy[index]),
            WrongType => {
                let error = read.unwrap_err();
                assert!(
                    !error
                        .downcast_ref::<eidetica::Error>()
                        .unwrap()
                        .is_not_found()
                );
            }
            BadJson => assert!(read.unwrap_err().is::<serde_json::Error>()),
            Missing => assert_eq!(read.unwrap(), AgentDbConfig::default()),
        }
    }

    async fn executor(
        &self,
    ) -> (
        Arc<Server>,
        Arc<crate::test_support::MockBackend>,
        tokio::sync::OwnedSemaphorePermit,
    ) {
        let (server, mock) = published_executor_with_mock(
            &self.settings,
            Arc::new(AgentRegistry::from_config(&self.yaml)),
            self.entries[0].clone(),
        )
        .await;
        server.agent_index().register(self.entries[1].clone());
        // Admission/recovery are real; model and tool execution stay inert.
        let permits = server
            .semaphore
            .clone()
            .try_acquire_many_owned(MAX_CONCURRENT_LLM_CALLS as u32)
            .unwrap();
        (server, mock, permits)
    }

    async fn warm(&self, server: &Server) {
        for (i, entry) in self.entries.iter().enumerate() {
            let hydrated = server
                .hydrate_agent_from_db(server.agents.get(&entry.display_name).unwrap())
                .await;
            assert_eq!(
                hydrated.max_spawn_depth,
                self.healthy[i].max_spawn_depth.unwrap()
            );
            assert_eq!(hydrated.allowed_tools, Some(Vec::new()));
            assert_eq!(hydrated.capabilities, self.healthy[i].capabilities);
        }
    }

    async fn publish(&self, server: &Server) -> (String, eidetica::Database) {
        let registry = server.registry();
        let (parent_id, _) = registry.create_session(Some("cli")).await.unwrap();
        registry
            .attach_agent_to_session(&parent_id.0, &self.entries[0])
            .await
            .unwrap();
        let id = server
            .submit_agent_job(&parent_id.0, "target", "held authority job")
            .await
            .unwrap();
        let (_, child) = registry.open_session(&id).await.unwrap();
        (id, child)
    }

    async fn assert_inert(
        &self,
        server: &Server,
        mock: &crate::test_support::MockBackend,
        child: &eidetica::Database,
        accepted: Option<&AcceptedAgentJob>,
    ) {
        assert_eq!(read_accepted_job(child).await.unwrap().as_ref(), accepted);
        assert!(read_job_rejection(child).await.unwrap().is_none());
        assert_eq!(
            server
                .registry()
                .list_sessions()
                .await
                .unwrap()
                .iter()
                .filter(|row| row
                    .source
                    .as_deref()
                    .is_some_and(|s| s.starts_with("job-stage:")))
                .count(),
            1
        );
        assert!(mock.recorded_calls().is_empty());
        let session =
            Session::new(ConversationId(child.root_id().to_string()), child.clone()).await;
        assert!(
            session.attempts_for_test().await.is_empty(),
            "no model/tool attempt may start"
        );
        assert!(!session.entries().iter().any(|entry| matches!(
            entry.entry_type,
            EntryType::ToolCall | EntryType::ToolResult
        )));
    }

    async fn finish(self) {
        drop(self.shutdown);
        self.service.await.unwrap().unwrap();
    }
}

fn assert_retryable_authority_error(error: anyhow::Error, agent: &str) {
    let message = error.to_string();
    assert!(
        message.contains("job authority unavailable") && message.contains(agent),
        "{error:#}"
    );
    assert!(
        !message.starts_with("invalid published job:"),
        "must hold, not reject: {error:#}"
    );
}

#[tokio::test]
async fn job_authority_admission_holds_on_unreadable_parent_and_target() {
    use ConfigPayload::*;
    tokio::time::timeout(
        ASYNC_TEST_TIMEOUT,
        Box::pin(async {
            for warm in [false, true] {
                for (index, fault) in [(0, WrongType), (0, BadJson), (1, WrongType), (1, BadJson)] {
                    let fixture = AuthorityFixture::new().await;
                    let (server, mock, _permits) = fixture.executor().await;
                    if warm {
                        fixture.warm(&server).await;
                    }
                    fixture.payload(index, fault).await;
                    let (id, child) = fixture.publish(&server).await;
                    assert_retryable_authority_error(
                        server
                            .register_accepted_job(&child)
                            .await
                            .expect_err("unreadable authority must hold admission"),
                        &fixture.entries[index].display_name,
                    );
                    assert_eq!(
                        server.job_status(&id).await.unwrap().state,
                        JobState::Pending
                    );
                    fixture.assert_inert(&server, &mock, &child, None).await;
                    fixture.payload(index, Healthy).await;
                    assert!(server.register_accepted_job(&child).await.unwrap());
                    let accepted = read_accepted_job(&child).await.unwrap().unwrap();
                    assert_eq!(accepted.definition.max_call_depth, 3);
                    assert!(accepted.definition.allowed_tools.is_empty());
                    assert_eq!(
                        accepted.definition.capability_ceiling,
                        fixture.healthy[0].capabilities
                    );
                    assert!(server.register_accepted_job(&child).await.unwrap());
                    fixture
                        .assert_inert(&server, &mock, &child, Some(&accepted))
                        .await;
                    server.shutdown().await;
                    fixture.finish().await;
                }
            }
        }),
    )
    .await
    .expect("authority admission matrix stalled");
}

#[tokio::test]
async fn job_authority_healthy_and_absent_payload_defaults() {
    use ConfigPayload::*;
    tokio::time::timeout(
        ASYNC_TEST_TIMEOUT,
        Box::pin(async {
            for (modes, depth, broad) in [
                ([Healthy, Healthy], 3, false),
                ([Missing, Healthy], 3, true),
                ([Healthy, Missing], 4, false),
                ([Missing, Missing], 10, true),
            ] {
                let fixture = AuthorityFixture::new().await;
                for (i, mode) in modes.into_iter().enumerate() {
                    fixture.payload(i, mode).await;
                }
                let (server, mock, permits) = fixture.executor().await;
                let (_, child) = fixture.publish(&server).await;
                assert!(server.register_accepted_job(&child).await.unwrap());
                let accepted = read_accepted_job(&child).await.unwrap().unwrap();
                assert_eq!(accepted.definition.max_call_depth, depth);
                assert_eq!(
                    accepted.definition.allowed_tools,
                    if broad {
                        vec!["calculate".to_string()]
                    } else {
                        vec![]
                    }
                );
                assert_eq!(
                    accepted.definition.capability_ceiling,
                    if broad {
                        crate::grants::Grants::default()
                    } else {
                        fixture.healthy[0].capabilities.clone()
                    }
                );
                fixture
                    .assert_inert(&server, &mock, &child, Some(&accepted))
                    .await;
                server.shutdown().await;
                drop(permits);
                let (fresh, next_mock, _permits) = fixture.executor().await;
                assert_eq!(
                    fresh.validate_accepted_job(&child).await.unwrap(),
                    Some(accepted.clone())
                );
                assert!(fresh.register_accepted_job(&child).await.unwrap());
                fixture
                    .assert_inert(&fresh, &next_mock, &child, Some(&accepted))
                    .await;
                fresh.shutdown().await;
                fixture.finish().await;
            }
        }),
    )
    .await
    .expect("authority defaults matrix stalled");
}

#[tokio::test]
async fn job_authority_holds_cached_and_restart_acceptance_until_readable() {
    use ConfigPayload::*;
    tokio::time::timeout(
        ASYNC_TEST_TIMEOUT,
        Box::pin(async {
            for (index, fault) in [(0, WrongType), (1, BadJson)] {
                let fixture = AuthorityFixture::new().await;
                let (server, mock, permits) = fixture.executor().await;
                let (id, child) = fixture.publish(&server).await;
                assert!(server.register_accepted_job(&child).await.unwrap());
                let accepted = read_accepted_job(&child).await.unwrap().unwrap();
                fixture.warm(&server).await;
                fixture.payload(index, fault).await;
                for result in [
                    server.validate_accepted_job(&child).await.map(|_| ()),
                    server.register_accepted_job(&child).await.map(|_| ()),
                    server.process_session(&id).await.map(|_| ()),
                ] {
                    assert_retryable_authority_error(
                        result.unwrap_err(),
                        &fixture.entries[index].display_name,
                    );
                }
                fixture
                    .assert_inert(&server, &mock, &child, Some(&accepted))
                    .await;
                server.shutdown().await;
                drop(permits);
                // The service retains storage, but this executor starts from YAML.
                let (fresh, next_mock, _permits) = fixture.executor().await;
                for result in [
                    fresh.validate_accepted_job(&child).await.map(|_| ()),
                    fresh.register_accepted_job(&child).await.map(|_| ()),
                    fresh.process_session(&id).await.map(|_| ()),
                ] {
                    assert_retryable_authority_error(
                        result.unwrap_err(),
                        &fixture.entries[index].display_name,
                    );
                }
                let row = fresh
                    .registry()
                    .list_sessions()
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|row| row.session_db_id == id)
                    .unwrap();
                assert!(
                    !fresh.adopt_local_client_session(row.clone()).await,
                    "a hold must remain eligible for retry"
                );
                fixture
                    .assert_inert(&fresh, &next_mock, &child, Some(&accepted))
                    .await;
                fixture.payload(index, Healthy).await;
                // Readability alone cannot replace accepted grants or depth.
                let mut changed = fixture.healthy[1].clone();
                for (depth, reason) in [
                    (0, "spawn depth exceeds Agent ceiling"),
                    (5, "accepted job authority differs from parent"),
                ] {
                    changed.max_spawn_depth = Some(depth);
                    fixture.dbs[1].write_config(&changed).await.unwrap();
                    assert!(
                        fresh
                            .register_accepted_job(&child)
                            .await
                            .unwrap_err()
                            .to_string()
                            .contains(reason)
                    );
                    fixture
                        .assert_inert(&fresh, &next_mock, &child, Some(&accepted))
                        .await;
                }
                fixture.payload(1, Healthy).await;
                let mut broadened = fixture.healthy[0].clone();
                broadened.tools = None;
                broadened.capabilities = crate::grants::Grants::default();
                fixture.dbs[0].write_config(&broadened).await.unwrap();
                assert!(
                    fresh
                        .register_accepted_job(&child)
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("accepted job authority differs from parent")
                );
                fixture.payload(0, Healthy).await;
                assert!(fresh.adopt_local_client_session(row).await);
                assert_eq!(
                    fresh.validate_accepted_job(&child).await.unwrap(),
                    Some(accepted.clone())
                );
                fixture
                    .assert_inert(&fresh, &next_mock, &child, Some(&accepted))
                    .await;
                fresh.shutdown().await;
                fixture.finish().await;
            }
        }),
    )
    .await
    .expect("authority recovery matrix stalled");
}

#[tokio::test]
async fn job_authority_open_failure_holds_instead_of_defaulting() {
    tokio::time::timeout(
        ASYNC_TEST_TIMEOUT,
        Box::pin(async {
            let fixture = AuthorityFixture::new().await;
            let (server, mock, _permits) = fixture.executor().await;
            fixture.warm(&server).await;
            // A hosted entry with a key that cannot open this DB is not an absent
            // config payload. Keep the actual DB and both runtime Agents intact.
            let mut broken = fixture.entries[1].clone();
            broken.pubkey = fixture.entries[0].pubkey.clone();
            assert!(
                server
                    .registry()
                    .open_agent_db(&broken.db_id, Some(&broken.pubkey))
                    .await
                    .is_err()
            );
            server.agent_index().register(broken);
            let (_, child) = fixture.publish(&server).await;
            assert_retryable_authority_error(
                server.register_accepted_job(&child).await.unwrap_err(),
                "target",
            );
            fixture.assert_inert(&server, &mock, &child, None).await;
            server.agent_index().register(fixture.entries[1].clone());
            assert!(server.register_accepted_job(&child).await.unwrap());
            let accepted = read_accepted_job(&child).await.unwrap().unwrap();
            assert_eq!(accepted.definition.max_call_depth, 3);
            fixture
                .assert_inert(&server, &mock, &child, Some(&accepted))
                .await;
            server.shutdown().await;
            fixture.finish().await;
        }),
    )
    .await
    .expect("AgentDb open failure control stalled");
}

#[tokio::test]
async fn job_authority_missing_hosted_agent_is_not_absent_config() {
    tokio::time::timeout(
        ASYNC_TEST_TIMEOUT,
        Box::pin(async {
            for index in [0, 1] {
                let fixture = AuthorityFixture::new().await;
                let (server, mock, _permits) = fixture.executor().await;
                server
                    .agent_index()
                    .unregister(&fixture.entries[index].db_id);
                let (_, child) = fixture.publish(&server).await;
                assert!(server.register_accepted_job(&child).await.unwrap());
                let rejection = read_job_rejection(&child).await.unwrap().unwrap();
                assert!(
                    rejection.contains(if index == 0 {
                        "parent Agent not hosted"
                    } else {
                        "target Agent not hosted on executor"
                    }),
                    "{rejection}"
                );
                assert!(read_accepted_job(&child).await.unwrap().is_none());
                assert!(mock.recorded_calls().is_empty());
                server.shutdown().await;
                fixture.finish().await;
            }
        }),
    )
    .await
    .expect("missing Agent distinction stalled");
}
