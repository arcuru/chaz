use super::*;
use crate::config::{BackendType, Model};
use crate::test_support::empty_secrets;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

pub(super) fn backend(base: &str) -> Backend {
    let mut backend = Backend::new(BackendType::OpenAICompatible);
    backend.api_base = Some(base.into());
    backend.api_key = Some("test-only-token".into());
    backend.max_retries = Some(0);
    backend.models = Some(vec![Model {
        name: "test-model".into(),
        api: None,
        reasoning: None,
        price_input: None,
        price_output: None,
        price_cache_read: None,
        context_window: None,
    }]);
    backend
}

pub(super) fn output() -> Vec<Value> {
    vec![
        json!({"type":"reasoning", "id":"r1", "summary":[], "encrypted_content":"opaque1"}),
        json!({"type":"message", "id":"m1", "role":"assistant", "status":"completed",
            "content":[{"type":"output_text", "text":"Working", "annotations":[]}]}),
        json!({"type":"function_call", "id":"fc1", "call_id":"call1", "name":"echo",
            "arguments":"{\"text\":\"first\",\"optional\":null}"}),
        json!({"type":"reasoning", "id":"r2", "summary":[], "encrypted_content":"opaque2"}),
        json!({"type":"function_call", "id":"fc2", "call_id":"call2", "name":"echo",
            "arguments":"{\"text\":\"second\",\"optional\":\"\"}"}),
    ]
}

pub(super) fn final_output() -> Vec<Value> {
    vec![
        json!({"type":"reasoning", "id":"rf", "summary":[], "encrypted_content":"opaque-final"}),
        json!({"type":"message", "id":"mf", "role":"assistant", "status":"completed",
            "content":[{"type":"output_text", "text":"Done", "annotations":[]}]}),
    ]
}

pub(super) fn response(items: Vec<Value>) -> Value {
    json!({"id":"resp1", "model":"actual-model", "status":"completed", "output":items,
        "usage":{"input_tokens":100, "output_tokens":12, "total_tokens":112,
            "input_tokens_details":{"cached_tokens":60},
            "output_tokens_details":{"reasoning_tokens":8}}})
}

fn parse(value: Value) -> Result<LLMResponse, LlmError> {
    parse_response(serde_json::from_value(value).unwrap(), "test-model")
}

#[tokio::test]
async fn ordered_wire_replay_keeps_every_item_and_call_id() {
    let server = MockServer::start().await;
    let secrets = empty_secrets().await;
    let mut config = backend(&format!("{}/v1/", server.uri()));
    config.max_tokens = Some(1234);
    config.models.as_mut().unwrap()[0].reasoning = Some(ReasoningConfig {
        effort: ReasoningEffort::XHigh,
    });
    let client = Responses::new(&config, &secrets);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer test-only-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response(output())))
        .expect(1)
        .mount(&server)
        .await;
    let mut messages = vec![
        RuntimeMessage::System("system1".into()),
        RuntimeMessage::System("system2".into()),
        RuntimeMessage::User("question".into()),
        RuntimeMessage::Assistant("legacy answer".into()),
    ];
    let tools = vec![ToolDefinition {
        name: "echo".into(),
        description: "Echo".into(),
        parameters: json!({"type":"object", "properties":{"text":{"type":"string"}}}),
        strict: false,
    }];
    let result = client
        .chat_with_tools(&messages, &tools, "test-model", Some("opaque-key"))
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    let request: Value = requests[0].body_json().unwrap();
    assert_eq!(request["instructions"], "system1\n\nsystem2");
    // Legacy history uses the easy-message string form, not a partial output item.
    assert_eq!(
        request["input"][1],
        json!({"type":"message", "role":"assistant", "content":"legacy answer"})
    );
    assert_eq!(request["model"], "test-model");
    assert_eq!(request["prompt_cache_key"], "opaque-key");
    assert_eq!(request["max_output_tokens"], 1234);
    assert_eq!(request["reasoning"], json!({"effort":"xhigh"}));
    assert_eq!(
        request["tools"][0],
        json!({"type":"function", "name":"echo", "description":"Echo",
        "parameters": tools[0].parameters, "strict": false})
    );
    assert_eq!(request["store"], false);
    assert_eq!(request["stream"], false);
    assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));
    assert!(request.get("previous_response_id").is_none());
    let LLMResponse::ToolCalls {
        content,
        tool_calls,
        provider_extra,
        metadata,
    } = result
    else {
        panic!("expected calls")
    };
    assert_eq!(content.as_deref(), Some("Working"));
    assert_eq!(
        tool_calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ["call1", "call2"]
    );
    assert_eq!(provider_extra[OUTPUT_ITEMS], json!(output()));
    assert_eq!(metadata.unwrap().usage.cached_tokens, Some(60));
    messages.push(RuntimeMessage::AssistantToolCalls {
        exchange_id: None,
        content,
        tool_calls,
        provider_extra,
    });
    messages.extend([
        RuntimeMessage::ToolResult {
            call_id: "call1".into(),
            content: "first result".into(),
        },
        RuntimeMessage::ToolResult {
            call_id: "call2".into(),
            content: "second result".into(),
        },
    ]);
    let next = serde_json::to_value(
        client
            .request(&messages, &tools, "test-model", Some("opaque-key"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(&next["input"].as_array().unwrap()[2..7], output());
    assert_eq!(
        next["input"][7],
        json!({"type":"function_call_output", "call_id":"call1", "output":"first result"})
    );
    assert_eq!(
        next["input"][8],
        json!({"type":"function_call_output", "call_id":"call2", "output":"second result"})
    );
    let LLMResponse::Text {
        content,
        provider_extra,
        metadata,
    } = parse(response(final_output())).unwrap()
    else {
        panic!("expected text")
    };
    assert_eq!(metadata.unwrap().extra[OUTPUT_ITEMS], json!(final_output()));
    messages.push(RuntimeMessage::AssistantWithExtra {
        content: Some(content),
        provider_extra,
    });
    messages.push(RuntimeMessage::User("next question".into()));
    let resumed = client
        .request(&messages, &[], "test-model", Some("opaque-key"))
        .unwrap();
    assert_eq!(&resumed.input[9..11], final_output());
    assert_eq!(resumed.input[11]["role"], "user");
}

#[test]
fn usage_hit_zero_missing_and_malformed_are_distinct() {
    for (details, expected) in [
        (json!({"cached_tokens":60}), Some(60)),
        (json!({"cached_tokens":0}), Some(0)),
        (json!({}), None),
        (Value::Null, None),
        (json!({"cached_tokens":null}), None),
        (json!({"cached_tokens":"0"}), None),
        (json!({"cached_tokens":-1}), None),
        (json!({"cached_tokens":4294967296u64}), None),
        (json!({"cached_tokens":1.5}), None),
    ] {
        let mut value = response(final_output());
        value["usage"]["input_tokens_details"] = details;
        let LLMResponse::Text {
            metadata: Some(metadata),
            ..
        } = parse(value).unwrap()
        else {
            panic!("metadata missing")
        };
        assert_eq!(metadata.usage.prompt_tokens, 100);
        assert_eq!(metadata.usage.cached_tokens, expected);
        assert_eq!(metadata.usage.reasoning_tokens, Some(8));
    }
    for usage in [
        Value::Null,
        json!("invalid"),
        json!({"input_tokens":"invalid"}),
    ] {
        let mut value = response(final_output());
        value["usage"] = usage;
        let LLMResponse::Text {
            metadata: Some(metadata),
            ..
        } = parse(value).unwrap()
        else {
            panic!("metadata missing")
        };
        assert_eq!(metadata.usage.cached_tokens, None);
    }
}

#[test]
fn refusals_errors_truncation_and_malformed_calls() {
    let refusal = json!({"type":"message", "role":"assistant", "content":[{"type":"refusal", "refusal":"Cannot help"}]});
    let LLMResponse::Text {
        content,
        provider_extra,
        ..
    } = parse(response(vec![refusal.clone()])).unwrap()
    else {
        panic!("expected refusal")
    };
    assert_eq!(content, "Cannot help");
    assert_eq!(provider_extra[OUTPUT_ITEMS], json!([refusal]));
    assert!(matches!(
        parse(
            json!({"status":"failed", "error":{"code":"rate_limit_exceeded", "message":"slow down"}})
        ),
        Err(LlmError::RateLimited { .. })
    ));
    assert!(matches!(
        parse(json!({"status":"incomplete", "incomplete_details":{"reason":"max_output_tokens"}})),
        Err(LlmError::TruncatedOutput { .. })
    ));
    assert!(parse(json!({"status":"queued"})).is_err());
    assert!(matches!(
        parse(response(vec![])),
        Err(LlmError::EmptyResponse { .. })
    ));
    for field in ["arguments", "call_id", "name"] {
        let mut call = output()[2].clone();
        call.as_object_mut().unwrap().remove(field);
        assert!(parse(response(vec![call])).is_err(), "missing {field}");
    }
    let mut call = output()[2].clone();
    call["arguments"] = "[1]".into();
    assert!(parse(response(vec![call])).is_err());
}

#[tokio::test]
async fn http_errors_keep_endpoint_and_do_not_fall_back_to_chat() {
    let server = MockServer::start().await;
    let client = Responses::new(&backend(&server.uri()), &empty_secrets().await);
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0.25")
                .set_body_json(json!({"error":{"message":"provider says wait"}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let error = client
        .chat_with_tools(&[], &[], "test-model", None)
        .await
        .err()
        .unwrap();
    assert_eq!(error.status(), Some(429));
    assert!(error.to_string().contains("provider says wait"));
    assert_eq!(
        error.retry_after(),
        Some(std::time::Duration::from_millis(250))
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/responses");
}

#[tokio::test]
async fn scoped_identity_reuses_persisted_roots_and_isolates_callers_sessions_connections() {
    let secrets = empty_secrets().await;
    let config = backend("http://localhost/v1");
    let first = secrets.session_cache_key("session-root-1", "caller-1", &config);
    assert_eq!(first.len(), 64);
    assert_ne!(
        first,
        secrets.session_cache_key("session-root-1", "caller-2", &config)
    );
    assert_eq!(
        first,
        secrets
            .clone()
            .session_cache_key("session-root-1", "caller-1", &config)
    );
    assert_ne!(
        first,
        secrets.session_cache_key("session-root-2", "caller-1", &config)
    );
    assert_ne!(
        first,
        empty_secrets()
            .await
            .session_cache_key("session-root-1", "caller-1", &config)
    );
    let mut other = config.clone();
    other.api_base = Some("http://localhost/other".into());
    assert_ne!(
        first,
        secrets.session_cache_key("session-root-1", "caller-1", &other)
    );
    other = config.clone();
    other.api_key_ref = Some("other-credential".into());
    assert_ne!(
        first,
        secrets.session_cache_key("session-root-1", "caller-1", &other)
    );
    other = config.clone();
    other.api_key = Some("another-test-token".into());
    assert_ne!(
        first,
        secrets.session_cache_key("session-root-1", "caller-1", &other)
    );
    assert!(!first.contains("session-root"));
}

#[tokio::test]
async fn unsupported_reasoning_and_corrupt_replay_fail_before_http() {
    let server = MockServer::start().await;
    let mut config = backend(&server.uri());
    config.models.as_mut().unwrap()[0].reasoning = Some(ReasoningConfig {
        effort: ReasoningEffort::Max,
    });
    let client = Responses::new(&config, &empty_secrets().await);
    assert!(matches!(
        client.chat_with_tools(&[], &[], "test-model", None).await,
        Err(LlmError::Configuration { .. })
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
    config.models.as_mut().unwrap()[0].reasoning = None;
    let client = Responses::new(&config, &empty_secrets().await);
    let messages = [RuntimeMessage::AssistantWithExtra {
        content: None,
        provider_extra: Map::from_iter([(OUTPUT_ITEMS.into(), json!("corrupt"))]),
    }];
    assert!(client.request(&messages, &[], "test-model", None).is_err());
}

#[tokio::test]
async fn later_system_turns_keep_their_position() {
    let client = Responses::new(&backend("http://localhost/v1"), &empty_secrets().await);
    let messages = [
        RuntimeMessage::System("initial".into()),
        RuntimeMessage::User("question".into()),
        RuntimeMessage::System("later policy".into()),
        RuntimeMessage::Assistant("answer".into()),
    ];
    let request = client.request(&messages, &[], "test-model", None).unwrap();
    assert_eq!(request.instructions, "initial");
    assert_eq!(
        request
            .input
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["user", "system", "assistant"]
    );
    assert_eq!(request.input[1]["content"][0]["text"], "later policy");
}
