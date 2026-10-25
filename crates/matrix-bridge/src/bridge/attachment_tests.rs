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
    Mock::given(method("GET")).and(path_regex("/_matrix/client/v3/sync"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "next_batch": "s1", "rooms": {"join": {ROOM: {
                "state": {"events": [
                    {"type":"m.room.create", "state_key":"", "sender":LOGIN, "event_id":"$create", "origin_server_ts":1,
                     "content":{"creator":LOGIN,"room_version":"10"}},
                    {"type":"m.room.member", "state_key":LOGIN, "sender":LOGIN, "event_id":"$join", "origin_server_ts":2,
                     "content":{"membership":"join"}}
                ]},
                "timeline": {"events":[], "limited":false, "prev_batch":"s0"}
            }}}
        }))).mount(&http).await;
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
    let (_http, room) = mock_room().await;
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
        sent(&_http).await.len(),
        1,
        "{:?}",
        _http
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.url.path())
            .collect::<Vec<_>>()
    );

    fx.legacy_bind(&db, "!two:example.com").await;
    outbound(&db, ROOM, "held-multi-room").await;
    reconciler.run_pass().await;
    assert_eq!(sent(&_http).await.len(), 1);
    fx.registry
        .detach_channel_session(&fx.agent, LOGIN, "matrix", LOGIN, "!two:example.com", None)
        .await
        .unwrap();
    reconciler.run_pass().await;
    assert_eq!(sent(&_http).await.len(), 2);
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
    assert_eq!(sent(&_http).await.len(), 2);
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
    assert_eq!(sent(&_http).await.len(), 3);
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
    let request = |id: &str| {
        chaz_core::session::SessionEntry {
        sender:"default".into(), content:json!({"request_id":id,"tool_name":"shell","risk_level":"High","arguments_display":"test","timeout_secs":60}).to_string(),
        timestamp:chrono::Utc::now(), entry_type:chaz_core::session::EntryType::ApprovalRequest,
        metadata:None, routing:None,
    }
    };
    session.add_entry(request("before-detach")).await.unwrap();
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
    session.add_entry(request("after-detach")).await.unwrap();
    outbound(&db, ROOM, "after-detach").await;
    // Even reattaching the same persisted binding cannot revive old observers.
    fx.registry
        .attach_conversational_channel_session(&fx.agent, LOGIN, "matrix", LOGIN, ROOM, &db)
        .await
        .unwrap();
    session.add_entry(request("after-rebind")).await.unwrap();
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
