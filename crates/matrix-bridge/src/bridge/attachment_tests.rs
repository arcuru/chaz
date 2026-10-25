//! Disposable session registries and loopback Matrix HTTP only; no models.
use super::*;
use chaz_core::{
    agent::AgentRegistry,
    agent_db::{AgentDbConfig, AgentMeta, create_agent_db},
    session::{BridgeEventRole, EntryRouting, SessionRegistry, TransportRef},
    types::ConversationId,
};
use eidetica::{Database, Instance, NewAccount, backend::database::InMemory, store::DocStore};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path_regex},
};

const LOGIN: &str = "@bot:example.com";
const ROOM: &str = "!one:example.com";
const OTHER_ROOM: &str = "!two:example.com";

struct Fixture {
    _instance: Instance,
    registry: Arc<SessionRegistry>,
    agent: DbEntry,
}
impl Fixture {
    async fn new() -> Self {
        let (instance, mut user) = Instance::create_backend(
            Box::new(InMemory::new()),
            NewAccount::passwordless("attachment"),
        )
        .await
        .unwrap();
        let (agent_db, pubkey) = create_agent_db(
            &mut user,
            "default",
            &AgentDbConfig::default(),
            &AgentMeta {
                display_name: Some("default".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        instance.enable_sync().await.unwrap();
        let registry = Arc::new(
            SessionRegistry::new(
                instance.clone(),
                user,
                Arc::new(AgentRegistry::from_config(
                    &serde_json::from_value(json!({"agents":[{"name":"default"}]})).unwrap(),
                )),
            )
            .await
            .unwrap(),
        );
        Self {
            _instance: instance,
            registry,
            agent: DbEntry {
                db_id: agent_db.id(),
                pubkey,
                display_name: "default".into(),
            },
        }
    }
    async fn bound(&self) -> Database {
        self.registry
            .get_or_create_conversational_channel_session(&self.agent, LOGIN, "matrix", LOGIN, ROOM)
            .await
            .unwrap()
            .1
    }
    async fn server(&self) -> Arc<Server> {
        use chaz_core::{
            backends::BackendManager,
            extension::ExtensionHub,
            hosted_index::HostedIndex,
            security::{LeakDetector, LeakPolicy, SecurityContext},
            tool::{ToolPolicyRegistry, ToolRegistry},
            tool_host::NativeToolHost,
        };
        let index = HostedIndex::empty("agent");
        index.register(self.agent.clone());
        Server::new(
            self.registry.clone(),
            self.registry.agents.clone(),
            index,
            HostedIndex::empty("bank"),
            HostedIndex::empty("skill_bank"),
            Arc::new(ToolRegistry::new()),
            Arc::new(ToolPolicyRegistry::empty()),
            SecurityContext {
                leak_detector: LeakDetector::new(LeakPolicy::default()),
                auto_approved_tools: HashSet::new(),
                approval_callback: None,
            },
            HashMap::new(),
            Default::default(),
            Arc::new(NativeToolHost::new()),
            Arc::new(ExtensionHub::new()),
            BackendManager::new(
                &None,
                SecretStore::new(self.registry.chaz_peer().clone()).await,
            ),
            Arc::new(chaz_core::mcp::McpRegistry::new()),
            None,
        )
    }
    fn route(&self, db: &Database) -> Arc<ChannelRoute> {
        let (stop, _) = tokio::sync::watch::channel(false);
        Arc::new(ChannelRoute {
            registry: Arc::downgrade(&self.registry),
            agent: self.agent.clone(),
            login_id: LOGIN.into(),
            room_id: ROOM.into(),
            session_db_id: db.root_id().to_string(),
            active: Mutex::new(true),
            stop,
        })
    }
    async fn legacy_bind(&self, db: &Database, room: &str) {
        let txn = db.new_transaction().await.unwrap();
        txn.get_store::<DocStore>("transport_bindings")
            .await
            .unwrap()
            .set_string(
                serde_json::to_string(&("matrix", LOGIN, room)).unwrap(),
                "matrix",
            )
            .await
            .unwrap();
        txn.commit().await.unwrap();
    }
}

async fn mock_room() -> (MockServer, Room) {
    let http = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex("/_matrix/client/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"versions": ["v1.11"]})))
        .mount(&http)
        .await;
    Mock::given(method("POST"))
        .and(path_regex("/_matrix/client/v3/keys/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "one_time_key_counts": {}, "device_keys": {}, "failures": {}
        })))
        .mount(&http)
        .await;
    let mut sync = json!({
        "next_batch": "s1", "rooms": {"join": {ROOM: {
            "state": {"events": [
                {"type":"m.room.create", "state_key":"", "sender":LOGIN, "event_id":"$create", "origin_server_ts":1,
                 "content":{"creator":LOGIN,"room_version":"10"}},
                {"type":"m.room.member", "state_key":LOGIN, "sender":LOGIN, "event_id":"$join", "origin_server_ts":2,
                 "content":{"membership":"join"}}
            ]},
            "timeline": {"events":[], "limited":false, "prev_batch":"s0"}
        }}}
    });
    sync["rooms"]["join"][OTHER_ROOM] = sync["rooms"]["join"][ROOM].clone();
    Mock::given(method("GET"))
        .and(path_regex("/_matrix/client/v3/sync"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sync))
        .mount(&http)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(
            "/_matrix/client/v3/rooms/.*/state/m.room.encryption/",
        ))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"errcode":"M_NOT_FOUND", "error":"unencrypted fixture"})),
        )
        .mount(&http)
        .await;
    Mock::given(method("PUT"))
        .and(path_regex(
            "/_matrix/client/v3/rooms/.*/send/m.room.message/.*",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"event_id":"$sent"})))
        .mount(&http)
        .await;
    let client = matrix_sdk::Client::builder()
        .homeserver_url(http.uri())
        .build()
        .await
        .unwrap();
    client
        .restore_session(
            serde_json::from_value::<matrix_sdk::authentication::matrix::MatrixSession>(json!({
                "user_id":LOGIN, "device_id":"DEVICE", "access_token":"disposable"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    client
        .sync_once(matrix_sdk::config::SyncSettings::default())
        .await
        .unwrap();
    let room = client
        .get_room(&matrix_sdk::ruma::RoomId::parse(ROOM).unwrap())
        .unwrap();
    (http, room)
}

fn approval_request() -> chaz_core::session::SessionEntry {
    chaz_core::bridge::approval_request_entry(
        "default",
        &chaz_core::tool::ToolApprovalInfo {
            name: "shell".into(),
            arguments_display: "test".into(),
            risk_level: chaz_core::tool::RiskLevel::High,
        },
        std::time::Duration::from_secs(120),
    )
    .1
}

async fn outbound(db: &Database, room: &str, id: &str) {
    let mut session = Session::new(ConversationId(db.root_id().to_string()), db.clone()).await;
    session
        .add_entry(chaz_core::session::SessionEntry::new_bridge_event(
            "default",
            BridgeEventRole::Outbound,
            id,
            Some(EntryRouting {
                destinations: vec![TransportRef {
                    transport: "matrix".into(),
                    login_id: LOGIN.into(),
                    channel: room.into(),
                    sender: None,
                    sender_display: None,
                    message_id: Some(id.into()),
                }],
                ..Default::default()
            }),
        ))
        .await
        .unwrap();
}

async fn sent(http: &MockServer) -> Vec<serde_json::Value> {
    http.received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method == "PUT" && r.url.path().contains("/send/m.room.message/"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn outbound_route_refuses_both_ambiguities_and_explicit_detach_restores_original() {
    let fx = Fixture::new().await;
    let db = fx.bound().await;
    let route = fx.route(&db);
    drop(route.guard().await.unwrap());
    let (http, room) = mock_room().await;
    let store = DeliveryStore::new(fx.registry.chaz_peer().clone());
    let reconciler = DeliveryReconciler::attach(
        db.clone(),
        store,
        DeliveryBinding::new("matrix", LOGIN, ROOM, db.root_id().to_string()),
        fx.registry.agents.clone(),
        "default".into(),
        |body| vec![body.to_owned()],
        {
            let route = route.clone();
            let room = room.clone();
            move |chunk| Box::pin(send_response_chunk(room.clone(), route.clone(), chunk))
        },
    )
    .await
    .unwrap();
    // Positive control: the exact production send closure really reaches HTTP.
    outbound(&db, ROOM, "before-conflict").await;
    reconciler.run_pass().await;
    assert_eq!(
        sent(&http).await.len(),
        1,
        "{:?}",
        http.received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.url.path())
            .collect::<Vec<_>>()
    );

    fx.legacy_bind(&db, "!two:example.com").await;
    outbound(&db, ROOM, "held-multi-room").await;
    reconciler.run_pass().await;
    assert_eq!(sent(&http).await.len(), 1);
    fx.registry
        .detach_channel_session(&fx.agent, LOGIN, "matrix", LOGIN, "!two:example.com", None)
        .await
        .unwrap();
    reconciler.run_pass().await;
    assert_eq!(sent(&http).await.len(), 2);
    assert_eq!(fx.bound().await.root_id(), db.root_id());

    let (_, duplicate) = fx
        .registry
        .create_session(Some("legacy-duplicate"))
        .await
        .unwrap();
    fx.legacy_bind(&duplicate, ROOM).await;
    fx.registry
        .ensure_session_host(&duplicate.root_id().to_string(), &fx.agent)
        .await
        .unwrap();
    let agent_db = fx
        .registry
        .open_agent_db(&fx.agent.db_id, None)
        .await
        .unwrap()
        .unwrap();
    agent_db
        .expose_session_on(&duplicate.root_id().to_string(), LOGIN)
        .await
        .unwrap();
    outbound(&db, ROOM, "held-multi-session").await;
    reconciler.run_pass().await;
    assert_eq!(sent(&http).await.len(), 2);
    fx.registry
        .detach_channel_session(
            &fx.agent,
            LOGIN,
            "matrix",
            LOGIN,
            ROOM,
            Some(&duplicate.root_id().to_string()),
        )
        .await
        .unwrap();
    reconciler.run_pass().await;
    assert_eq!(sent(&http).await.len(), 3);
    assert_eq!(fx.bound().await.root_id(), db.root_id());
    reconciler.stop();
}

#[tokio::test]
async fn approval_observation_never_blocks_the_committing_writer_on_its_route_gate() {
    let fx = Fixture::new().await;
    let db = fx.bound().await;
    let route = fx.route(&db);
    let (_http, room) = mock_room().await;
    let pending = Arc::new(ApprovalRelay {
        slots: Mutex::new(HashMap::new()),
    });
    let watcher = attach_approval_watcher(&db, room, pending, route.clone())
        .await
        .unwrap();
    let gate = route.guard().await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        outbound(&db, ROOM, "writer-holds-route"),
    )
    .await
    .expect("a callback must only invalidate, not wait on a gate held by the committing writer");
    drop(gate);
    route.stop.send_replace(true);
    drop(watcher);
}

#[tokio::test]
async fn detach_stops_delivery_and_approval_observers_without_history_replay() {
    let fx = Fixture::new().await;
    let db = fx.bound().await;
    let route = fx.route(&db);
    let (http, room) = mock_room().await;
    // Ordinary local history and a previously addressed different room stay local.
    let mut session = Session::new(ConversationId(db.root_id().to_string()), db.clone()).await;
    session
        .add_entry(chaz_core::session::SessionEntry {
            sender: "default".into(),
            content: "old local final".into(),
            timestamp: chrono::Utc::now(),
            entry_type: chaz_core::session::EntryType::Message,
            metadata: None,
            routing: None,
        })
        .await
        .unwrap();
    outbound(&db, "!old:example.com", "old output").await;
    let pending = Arc::new(ApprovalRelay {
        slots: Mutex::new(HashMap::new()),
    });
    let approval = attach_approval_watcher(&db, room.clone(), pending.clone(), route.clone())
        .await
        .unwrap();
    let reconciler = DeliveryReconciler::attach(
        db.clone(),
        DeliveryStore::new(fx.registry.chaz_peer().clone()),
        DeliveryBinding::new("matrix", LOGIN, ROOM, db.root_id().to_string()),
        fx.registry.agents.clone(),
        "default".into(),
        |body| vec![body.to_owned()],
        {
            let route = route.clone();
            let room = room.clone();
            move |chunk| Box::pin(send_response_chunk(room.clone(), route.clone(), chunk))
        },
    )
    .await
    .unwrap();
    assert!(
        sent(&http).await.is_empty(),
        "attach must not replay old local/other-room output"
    );
    session.add_entry(approval_request()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while pending.slots.lock().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        sent(&http).await.len(),
        1,
        "positive approval delivery control"
    );
    assert_eq!(pending.slots.lock().await.len(), 1);
    // A decision holds the route gate while committing to this same session.
    // The deferred watcher must not deadlock that commit.
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        resolve_pending_approval(&pending, &room, "@human:example.com", false),
    )
    .await
    .unwrap();
    assert!(pending.slots.lock().await.is_empty());
    assert_eq!(sent(&http).await.len(), 2);
    let channel = AttachedChannel {
        delivery: reconciler.clone(),
        approval,
        route: route.clone(),
        typing: typing::attach(&db, room, route.stop.subscribe())
            .await
            .unwrap(),
    };
    fx.registry
        .detach_channel_session(&fx.agent, LOGIN, "matrix", LOGIN, ROOM, None)
        .await
        .unwrap();
    channel.finish().await;
    session.add_entry(approval_request()).await.unwrap();
    outbound(&db, ROOM, "after-detach").await;
    // Even reattaching the same persisted binding cannot revive old observers.
    fx.registry
        .attach_conversational_channel_session(&fx.agent, LOGIN, "matrix", LOGIN, ROOM, &db)
        .await
        .unwrap();
    session.add_entry(approval_request()).await.unwrap();
    reconciler.run_pass().await;
    assert_eq!(sent(&http).await.len(), 2);
    assert!(route.guard().await.is_err());
    let history = Session::new(ConversationId(db.root_id().to_string()), db).await;
    assert!(
        history
            .entries()
            .iter()
            .any(|entry| entry.content == "old local final")
    );
}

#[tokio::test]
async fn detach_recovers_delivery_after_startup_refused_legacy_ambiguity() {
    for duplicate_room in [false, true] {
        let fx = Fixture::new().await;
        let original = fx.bound().await;
        let (http, room) = mock_room().await;
        let (detach_room, detach_id) = if duplicate_room {
            let (_, duplicate) = fx.registry.create_session(Some("duplicate")).await.unwrap();
            fx.legacy_bind(&duplicate, ROOM).await;
            let sid = duplicate.root_id().to_string();
            fx.registry
                .ensure_session_host(&sid, &fx.agent)
                .await
                .unwrap();
            fx.registry
                .open_agent_db(&fx.agent.db_id, None)
                .await
                .unwrap()
                .unwrap()
                .expose_session_on(&sid, LOGIN)
                .await
                .unwrap();
            (room.clone(), sid)
        } else {
            fx.legacy_bind(&original, OTHER_ROOM).await;
            (
                room.client()
                    .get_room(&matrix_sdk::ruma::RoomId::parse(OTHER_ROOM).unwrap())
                    .unwrap(),
                String::new(),
            )
        };
        let server = fx.server().await;
        let (_stop, stopped) = tokio::sync::watch::channel(false);
        let attached = Arc::new(Channels {
            bindings: Mutex::new(Some(HashMap::new())),
            stopped,
            observer_tasks: Default::default(),
        });
        let pending = Arc::new(ApprovalRelay {
            slots: Mutex::new(HashMap::new()),
        });
        attach_registered_channels(
            &server,
            &room.client(),
            &attached,
            LOGIN,
            "default",
            pending.clone(),
            &workspace::RoomIndex::default(),
        )
        .await;
        outbound(&original, ROOM, "pending-before-recovery").await;
        assert!(
            sent(&http).await.is_empty(),
            "legacy shape duplicate_room={duplicate_room}"
        );
        handle_detach(
            &detach_id,
            detach_room,
            server.clone(),
            LOGIN,
            "default",
            attached.clone(),
            pending,
            workspace::RoomIndex::default(),
        )
        .await;
        let recovered = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if sent(&http)
                    .await
                    .iter()
                    .any(|body| body["body"] == "pending-before-recovery")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        for channel in attached.bindings.lock().await.take().unwrap().into_values() {
            channel.finish().await;
        }
        server.shutdown().await;
        recovered
            .expect("explicit detach must resume surviving delivery without new input or restart");
        assert_eq!(fx.bound().await.root_id(), original.root_id());
    }
}

#[tokio::test]
async fn detach_cancels_an_inflight_approval_post_before_draining_its_route() {
    let fx = Fixture::new().await;
    let db = fx.bound().await;
    let route = fx.route(&db);
    let (http, room) = mock_room().await;
    Mock::given(method("PUT"))
        .and(path_regex(
            "/_matrix/client/v3/rooms/.*/send/m.room.message/.*",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"event_id":"$delayed"}))
                .set_delay(std::time::Duration::from_secs(60)),
        )
        .with_priority(1)
        .mount(&http)
        .await;
    let pending = Arc::new(ApprovalRelay {
        slots: Mutex::new(HashMap::new()),
    });
    let approval = attach_approval_watcher(&db, room.clone(), pending, route.clone())
        .await
        .unwrap();
    let delivery = DeliveryReconciler::attach(
        db.clone(),
        DeliveryStore::new(fx.registry.chaz_peer().clone()),
        DeliveryBinding::new("matrix", LOGIN, ROOM, db.root_id().to_string()),
        fx.registry.agents.clone(),
        "default".into(),
        |body| vec![body.to_owned()],
        |_| Box::pin(async { anyhow::bail!("no outbound rows in this fixture") }),
    )
    .await
    .unwrap();
    let channel = AttachedChannel {
        delivery,
        approval,
        typing: typing::attach(&db, room, route.stop.subscribe())
            .await
            .unwrap(),
        route,
    };
    let mut session = Session::new(ConversationId(db.root_id().to_string()), db).await;
    session.add_entry(approval_request()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while sent(&http).await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("approval must reach the blocked HTTP send");
    tokio::time::timeout(std::time::Duration::from_secs(2), channel.finish())
        .await
        .expect("detach must cancel the approval post before waiting for its route gate");
}

#[tokio::test]
async fn reattach_waits_for_the_old_observers_to_finish() {
    let fx = Fixture::new().await;
    let db = fx.bound().await;
    let server = fx.server().await;
    let (http, room) = mock_room().await;
    let (_stop, stopped) = tokio::sync::watch::channel(false);
    let attached = Arc::new(Channels {
        bindings: Mutex::new(Some(HashMap::new())),
        stopped,
        observer_tasks: Default::default(),
    });
    let pending = Arc::new(ApprovalRelay {
        slots: Mutex::new(HashMap::new()),
    });
    attach_once(
        &attached,
        &db,
        room.clone(),
        &server,
        LOGIN,
        "default",
        pending.clone(),
    )
    .await
    .unwrap();
    let sid = db.root_id().to_string();
    let old_route = attached
        .bindings
        .lock()
        .await
        .as_ref()
        .unwrap()
        .get(&(ROOM.into(), sid.clone()))
        .unwrap()
        .route
        .clone();
    let inflight = old_route.guard().await.unwrap();
    let detaching = tokio::spawn(handle_detach(
        "",
        room.clone(),
        server.clone(),
        LOGIN,
        "default",
        attached.clone(),
        pending.clone(),
        workspace::RoomIndex::default(),
    ));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while chaz_core::session::is_bound(&db, "matrix", LOGIN, ROOM)
            .await
            .unwrap()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detach must reach observer draining");
    let mut attaching = tokio::spawn({
        let room = room.clone();
        let server = server.clone();
        let attached = attached.clone();
        async move { handle_attach(&sid, room, server, LOGIN, "default", attached, pending).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !chaz_core::session::is_bound(&db, "matrix", LOGIN, ROOM)
            .await
            .unwrap()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reattach must record its binding");
    // Its binding is recorded, but installation must wait for the old route's
    // final release rather than report success against a dying observer.
    let early = tokio::time::timeout(std::time::Duration::from_millis(100), &mut attaching).await;
    drop(inflight);
    tokio::time::timeout(std::time::Duration::from_secs(5), detaching)
        .await
        .unwrap()
        .unwrap();
    if early.is_err() {
        tokio::time::timeout(std::time::Duration::from_secs(5), attaching)
            .await
            .unwrap()
            .unwrap();
    }
    outbound(&db, ROOM, "after-concurrent-reattach").await;
    let delivered = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if sent(&http)
                .await
                .iter()
                .any(|body| body["body"] == "after-concurrent-reattach")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    for channel in attached.bindings.lock().await.take().unwrap().into_values() {
        channel.finish().await;
    }
    server.shutdown().await;
    assert!(
        early.is_err(),
        "reattach acknowledged before the old observers finished"
    );
    delivered.expect("the replacement route must deliver new addressed output");
    assert!(old_route.guard().await.is_err());
}

#[tokio::test]
async fn approval_reaction_cannot_resolve_another_rooms_request() {
    let fx = Fixture::new().await;
    let db = fx.bound().await;
    let prompt = OwnedEventId::try_from("$prompt").unwrap();
    let pending = Arc::new(ApprovalRelay {
        slots: Mutex::new(HashMap::from([(
            prompt.clone(),
            PendingApproval {
                session_db: db.clone(),
                request_id: "request".into(),
                room_id: ROOM.into(),
                route: fx.route(&db),
                seq: 0,
            },
        )])),
    });
    let reaction = || {
        serde_json::from_value::<OriginalSyncReactionEvent>(serde_json::json!({
            "type": "m.reaction", "event_id": "$reaction", "sender": "@human:example.org",
            "origin_server_ts": 1, "content": {"m.relates_to": {
                "rel_type": "m.annotation", "event_id": "$prompt", "key": "✅",
            }},
        }))
        .unwrap()
    };
    handle_approval_reaction(reaction(), pending.clone(), "!sibling:example.org").await;
    let session = Session::new(
        chaz_core::types::ConversationId(db.root_id().to_string()),
        db.clone(),
    )
    .await;
    assert!(
        chaz_core::bridge::existing_decision(session.entries(), "request").is_none(),
        "a sibling reaction must not authorize this request"
    );
    assert!(
        pending.slots.lock().await.contains_key(&prompt),
        "wrong-room reaction must not consume the prompt"
    );
    handle_approval_reaction(reaction(), pending.clone(), ROOM).await;
    let session = Session::new(
        chaz_core::types::ConversationId(db.root_id().to_string()),
        db,
    )
    .await;
    assert_eq!(
        chaz_core::bridge::existing_decision(session.entries(), "request"),
        Some(ApprovalDecision::Approve)
    );
    assert!(pending.slots.lock().await.is_empty());
}
