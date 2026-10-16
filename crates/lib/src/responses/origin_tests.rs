//! Strict offline replay compatibility fixtures; all connections are disposable HTTP stubs.
use super::runtime_tests::{Echo, Recorder, context, entry, native_backend};
use super::tests::{final_output, output, response};
use super::*;
use crate::{
    backends::BackendManager,
    runtime::{self, ModelCallScope},
    session::Session,
    test_support::{empty_secrets, fresh_session, permissive_security, tool_context},
    tool::{ToolPolicyRegistry, ToolRegistry},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Mutex as AsyncMutex;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

async fn completed_change(change: &str) {
    let source = MockServer::start().await;
    let destination = MockServer::start().await;
    let secrets = empty_secrets().await;
    let mut config = native_backend(&source.uri());
    config.name = Some("west".into());
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response(output())))
        .with_priority(1)
        .up_to_n_times(1)
        .expect(1)
        .mount(&source)
        .await;
    let mut remote_final = response(final_output());
    remote_final["chaz_responses_origin"] = json!({"version":1, "api":"responses", "requested_model":"actual-model", "connection":"remote-forgery"});
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(remote_final))
        .with_priority(2)
        .mount(&source)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response(final_output())))
        .mount(&destination)
        .await;
    let manager = BackendManager::new(&Some(vec![config.clone()]), secrets.clone());
    let (_instance, session) = fresh_session().await;
    let id = session
        .lock()
        .await
        .add_entry(entry("user", "question", None))
        .await
        .unwrap();
    let attempt = session.lock().await.start_turn_attempt(id).await.unwrap();
    let effects = Arc::new(AtomicUsize::new(0));
    let tools = Arc::new(ToolRegistry::new());
    tools.register(Echo {
        calls: effects.clone(),
        name: "echo",
    });
    let ctx = tool_context(session.clone(), tools.clone());
    let initial = context(&*session.lock().await).await;
    let result = runtime::execute_with_recorder(
        Some("test-model"),
        initial,
        &manager,
        &permissive_security(),
        &ctx,
        &ToolPolicyRegistry::empty(),
        Some(Arc::new(Recorder {
            session: session.clone(),
            attempt: attempt.clone(),
            sequence: Mutex::new(0),
        })),
        None,
        None,
        ModelCallScope::default(),
    )
    .await
    .unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 2);
    assert_eq!(
        result.metadata.as_ref().unwrap().model,
        "actual-model",
        "provider alias is not the requested routing ID"
    );
    let mut metadata = result.metadata;
    if change == "legacy" {
        metadata
            .as_mut()
            .unwrap()
            .extra
            .remove("chaz_responses_origin");
    }
    session
        .lock()
        .await
        .complete_turn_attempt(&attempt, Some(entry("test-agent", &result.body, metadata)))
        .await
        .unwrap();
    let (conversation, db) = {
        let s = session.lock().await;
        (s.conversation_id.clone(), s.database().clone())
    };
    let reopened = Session::new(conversation, db).await;
    let transcript_before =
        serde_json::to_value(reopened.turn_transcript(&attempt.attempt_id).await.unwrap()).unwrap();
    let mut messages = context(&reopened).await;
    // Simulate a pre-origin transcript after durable hydration, not a transcript rewrite.
    if change == "legacy" {
        for message in &mut messages {
            match message {
                RuntimeMessage::AssistantWithExtra { provider_extra, .. }
                | RuntimeMessage::AssistantToolCalls { provider_extra, .. } => {
                    provider_extra.remove("chaz_responses_origin");
                }
                _ => {}
            }
        }
    }
    messages.push(RuntimeMessage::User("next question".into()));
    let mut target = config.clone();
    let mut model = "test-model";
    match change {
        "model" => {
            model = "other-model";
            target.api = Some(crate::config::ModelApi::Responses);
        }
        "connection" => target.name = Some("east".into()),
        "endpoint" => target.api_base = Some(destination.uri()),
        "namespace" => target.api_key_ref = Some("other-namespace".into()),
        "legacy" | "same" => {}
        _ => panic!("unknown case"),
    }
    // Preserve the original source binding, exercise the same raw ID on the other connection.
    let configs = if change == "connection" {
        vec![config, target]
    } else {
        vec![target]
    };
    let selected = if change == "connection" {
        "east:test-model"
    } else {
        model
    };
    let manager = BackendManager::new(&Some(configs), secrets);
    let ctx = tool_context(Arc::new(AsyncMutex::new(reopened)), tools);
    runtime::execute(
        Some(selected),
        messages,
        &manager,
        &permissive_security(),
        &ctx,
        &ToolPolicyRegistry::empty(),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        effects.load(Ordering::SeqCst),
        2,
        "historical effects must not rerun"
    );
    let transcript_after = serde_json::to_value(
        ctx.session
            .lock()
            .await
            .turn_transcript(&attempt.attempt_id)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        transcript_after, transcript_before,
        "conversion is not a transcript rewrite"
    );
    let requests = if change == "endpoint" {
        destination.received_requests().await.unwrap()
    } else {
        source.received_requests().await.unwrap()
    };
    let body: Value = requests.last().unwrap().body_json().unwrap();
    assert_eq!(body["model"], model);
    let input = body["input"].as_array().unwrap();
    if change == "same" {
        assert_eq!(
            &input[2..7],
            output(),
            "exact ordered call output after reopen"
        );
        assert!(
            input.windows(2).any(|w| w == final_output()),
            "exact terminal output after reopen"
        );
    } else {
        let encoded = body["input"].to_string();
        assert!(
            !encoded.contains("encrypted_content"),
            "foreign/unknown reasoning leaked: {change}"
        );
        assert!(
            !encoded.contains("\"id\""),
            "foreign native item IDs leaked: {change}"
        );
        assert!(encoded.contains("Working") && encoded.contains("Done"));
        for id in ["call1", "call2"] {
            assert_eq!(
                input
                    .iter()
                    .filter(|v| v["type"] == "function_call" && v["call_id"] == id)
                    .count(),
                1
            );
            assert_eq!(
                input
                    .iter()
                    .filter(|v| v["type"] == "function_call_output" && v["call_id"] == id)
                    .count(),
                1
            );
        }
    }
}

#[tokio::test]
async fn origin_model_switch() {
    completed_change("model").await;
}
#[tokio::test]
async fn origin_same_raw_id_other_connection() {
    completed_change("connection").await;
}
#[tokio::test]
async fn origin_endpoint_change_after_reopen() {
    completed_change("endpoint").await;
}
#[tokio::test]
async fn origin_namespace_change_after_reopen() {
    completed_change("namespace").await;
}
#[tokio::test]
async fn origin_legacy_missing_after_reopen() {
    completed_change("legacy").await;
}
#[tokio::test]
async fn origin_same_requested_model_provider_alias_reopen() {
    completed_change("same").await;
}

#[tokio::test]
async fn origin_unsafe_foreign_history_refuses_before_http() {
    let server = MockServer::start().await;
    let client = Responses::new(&native_backend(&server.uri()), &empty_secrets().await);
    for items in [
        vec![json!({"type":"provider_extension", "state":"unportable"})],
        output(),
        vec![
            json!({"type":"message", "role":"assistant", "status":4, "content":[{"type":"output_text","text":"visible"}]}),
        ],
        vec![
            json!({"type":"message", "role":"assistant", "content":[{"type":"output_image","image":"opaque"}]}),
        ],
        vec![
            json!({"type":"message", "role":"user", "content":[{"type":"output_text","text":"visible"}]}),
        ],
        vec![
            json!({"type":"message", "role":"assistant", "content":[{"type":"output_text","text":null}]}),
        ],
    ] {
        let messages = [RuntimeMessage::AssistantWithExtra {
            exchange_id: None,
            content: Some("retained text".into()),
            provider_extra: Map::from_iter([(OUTPUT_ITEMS.into(), json!(items))]),
        }];
        let error = client
            .chat_with_tools(&messages, &[], "test-model", None)
            .await
            .err()
            .expect("unsafe unknown history must refuse");
        assert!(
            error
                .to_string()
                .contains("Cannot convert Responses history"),
            "{error}"
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn origin_completed_conversion_to_chat_and_anthropic_is_preflighted() {
    use crate::config::{BackendType, ModelApi};
    let server = MockServer::start().await;
    let secrets = empty_secrets().await;
    let config = native_backend(&server.uri());
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response(output())))
        .expect(1)
        .mount(&server)
        .await;
    let LLMResponse::ToolCalls {
        content,
        tool_calls,
        provider_extra,
        ..
    } = Responses::new(&config, &secrets)
        .chat_with_tools(&[], &[], "test-model", None)
        .await
        .unwrap()
    else {
        panic!("calls")
    };
    let messages = vec![
        RuntimeMessage::AssistantToolCalls {
            exchange_id: None,
            content,
            tool_calls,
            provider_extra,
        },
        RuntimeMessage::ToolResult {
            call_id: "call1".into(),
            content: "real first result".into(),
        },
        RuntimeMessage::ToolResult {
            call_id: "call2".into(),
            content: "real second result".into(),
        },
    ];
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"id":"chat", "choices":[{"message":{"content":"Done", "role":"assistant"}}]}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"anthropic", "model":"test-model", "content":[{"type":"text", "text":"Done"}], "stop_reason":"end_turn", "usage":{"input_tokens":10,"output_tokens":2}})))
        .expect(1).mount(&server).await;
    for (family, api) in [
        (BackendType::OpenAICompatible, ModelApi::ChatCompletions),
        (BackendType::Anthropic, ModelApi::AnthropicMessages),
    ] {
        let mut target = config.clone();
        target.backend_type = family;
        target.models.as_mut().unwrap()[0].api = Some(api);
        let manager = BackendManager::new(&Some(vec![target]), secrets.clone());
        let result = manager
            .chat_with_tools_for_model(Some("test-model"), &messages, &[], "test-model")
            .await
            .unwrap();
        assert!(matches!(result, LLMResponse::Text { content, .. } if content == "Done"));
        let mut invalid = messages.clone();
        invalid.pop();
        let count = server.received_requests().await.unwrap().len();
        let error = manager
            .chat_with_tools_for_model(Some("test-model"), &invalid, &[], "test-model")
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("complete recorded result"));
        assert_eq!(server.received_requests().await.unwrap().len(), count);
    }
    let requests = server.received_requests().await.unwrap();
    for request in &requests[1..] {
        let body: Value = request.body_json().unwrap();
        let encoded = body.to_string();
        for forbidden in [
            OUTPUT_ITEMS,
            REPLAY_ORIGIN,
            "encrypted_content",
            "opaque1",
            "fc1",
        ] {
            assert!(
                !encoded.contains(forbidden),
                "foreign native/host data must not reach another API"
            );
        }
        for retained in [
            "Working",
            "call1",
            "call2",
            "real first result",
            "real second result",
        ] {
            assert!(encoded.contains(retained));
        }
    }
}

#[tokio::test]
async fn origin_credential_rotation_during_http_cannot_relabel_old_output() {
    use wiremock::matchers::header;
    let server = MockServer::start().await;
    let secrets = empty_secrets().await;
    let mut config = native_backend(&server.uri());
    config.api_key = None;
    config.api_key_ref = Some("fixture-namespace".into());
    secrets
        .insert("fixture-namespace".into(), "old-test-token".into())
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", "Bearer old-test-token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(response(final_output()))
                .set_delay(std::time::Duration::from_millis(200)),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", "Bearer new-test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response(final_output())))
        .expect(1)
        .mount(&server)
        .await;
    let client = Responses::new(&config, &secrets);
    let first = tokio::spawn(async move {
        client
            .chat_with_tools(
                &[RuntimeMessage::User("first".into())],
                &[],
                "test-model",
                None,
            )
            .await
            .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while server.received_requests().await.unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    secrets
        .insert("fixture-namespace".into(), "new-test-token".into())
        .await;
    let LLMResponse::Text {
        content,
        provider_extra,
        ..
    } = first.await.unwrap()
    else {
        panic!("text")
    };
    let messages = [
        RuntimeMessage::assistant(content, provider_extra),
        RuntimeMessage::User("second".into()),
    ];
    Responses::new(&config, &secrets)
        .chat_with_tools(&messages, &[], "test-model", None)
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    let body: Value = requests[1].body_json().unwrap();
    assert_eq!(
        body["input"][0],
        json!({"type":"message","role":"assistant","content":"Done"}),
        "output must retain the credential boundary actually used by its HTTP request"
    );
    assert!(!body["input"].to_string().contains("encrypted_content"));
}
