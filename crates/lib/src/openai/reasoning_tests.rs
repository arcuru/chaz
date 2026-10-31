//! Stub-server wire tests for per-model reasoning across every request API:
//! what is sent, what is refused before sending, and how rejections read.

use super::*;
use crate::backends::{BackendManager, ChatContext, Message, MessageRole};
use crate::config::{BackendType, Model, ModelApi, ReasoningConfig, ReasoningFormat};
use crate::test_support::empty_secrets;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const KEY: &str = "sk-test-SECRET-0e23";

fn model(name: &str, api: Option<ModelApi>, reasoning: Option<ReasoningConfig>) -> Model {
    Model {
        name: name.into(),
        api,
        reasoning,
        price_input: None,
        price_output: None,
        price_cache_read: None,
        context_window: None,
    }
}

fn effort(effort: ReasoningEffort, format: Option<ReasoningFormat>) -> Option<ReasoningConfig> {
    Some(ReasoningConfig { effort, format })
}

fn backend(server: &MockServer, kind: BackendType, models: Vec<Model>) -> Backend {
    let mut b = Backend::new(kind);
    b.name = Some("stub".into());
    b.api_base = Some(format!("{}/v1", server.uri()));
    b.api_key = Some(KEY.into());
    b.models = Some(models);
    b
}

async fn manager(b: Backend) -> BackendManager {
    BackendManager::new(&Some(vec![b]), empty_secrets().await)
}

fn chat_ok() -> Value {
    json!({"choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "model": "test-model", "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}})
}

fn responses_ok() -> Value {
    json!({"status": "completed", "model": "test-model",
        "output": [{"type": "message", "content": [{"type": "output_text", "text": "ok"}]}],
        "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}})
}

fn anthropic_ok() -> Value {
    json!({"content": [{"type": "text", "text": "ok"}], "model": "test-model",
        "stop_reason": "end_turn", "usage": {"input_tokens": 3, "output_tokens": 1}})
}

async fn stub(server: &MockServer, endpoint: &str, body: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path(endpoint))
        .respond_with(body)
        .mount(server)
        .await;
}

async fn bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "echo".into(),
        description: "echo".into(),
        parameters: json!({"type": "object", "properties": {}}),
        strict: false,
    }
}

fn plain(model: &str) -> ChatContext {
    ChatContext {
        messages: vec![Message::new(MessageRole::User, "hi")],
        model: Some(model.into()),
        system_prompt: None,
    }
}

fn user() -> Vec<RuntimeMessage> {
    vec![RuntimeMessage::User("hi".into())]
}

#[tokio::test]
async fn legacy_chat_request_bytes_are_unchanged() {
    // The exact bytes Chaz sent before `format` existed. Omitted and explicit
    // `openrouter` must both keep them; omitted reasoning sends no key at all.
    let server = MockServer::start().await;
    let secrets = empty_secrets().await;
    for (reasoning, expected) in [
        (
            effort(ReasoningEffort::High, None),
            r#"{"model":"test-model","messages":[{"role":"user","content":"hello"}],"reasoning":{"effort":"high"},"usage":{"include":true}}"#,
        ),
        (
            effort(ReasoningEffort::High, Some(ReasoningFormat::OpenRouter)),
            r#"{"model":"test-model","messages":[{"role":"user","content":"hello"}],"reasoning":{"effort":"high"},"usage":{"include":true}}"#,
        ),
        (
            None,
            r#"{"model":"test-model","messages":[{"role":"user","content":"hello"}],"usage":{"include":true}}"#,
        ),
    ] {
        let b = backend(
            &server,
            BackendType::OpenAICompatible,
            vec![model("test-model", None, reasoning)],
        );
        let client = OpenAI::new(&b, &secrets);
        let control = client.reasoning_control("test-model").unwrap();
        let messages = vec![ChatMessage {
            role: "user".into(),
            content: Some(MessageContent::Text("hello".into())),
            tool_calls: None,
            tool_call_id: None,
            extra: Map::new(),
        }];
        let request = client.chat_request("test-model", messages, None, control);
        assert_eq!(serde_json::to_string(&request).unwrap(), expected);
    }
}

#[tokio::test]
async fn chat_formats_apply_on_tool_and_plain_paths() {
    for (format, sent) in [
        (None, json!({"reasoning": {"effort": "low"}})),
        (
            Some(ReasoningFormat::OpenRouter),
            json!({"reasoning": {"effort": "low"}}),
        ),
        (
            Some(ReasoningFormat::OpenAI),
            json!({"reasoning_effort": "low"}),
        ),
    ] {
        let server = MockServer::start().await;
        stub(
            &server,
            "/v1/chat/completions",
            ResponseTemplate::new(200).set_body_json(chat_ok()),
        )
        .await;
        let mgr = manager(backend(
            &server,
            BackendType::OpenAICompatible,
            vec![model(
                "test-model",
                None,
                effort(ReasoningEffort::Low, format),
            )],
        ))
        .await;
        let response = mgr
            .chat_with_tools_for_model(Some("test-model"), &user(), &[tool()], "test-model")
            .await
            .unwrap();
        // An ignoring provider looks like success; absent accounting stays unknown.
        let LLMResponse::Text { metadata, .. } = response else {
            panic!("expected text");
        };
        assert_eq!(metadata.unwrap().usage.reasoning_tokens, None);
        mgr.execute(&plain("test-model")).await.unwrap();
        let bodies = bodies(&server).await;
        assert_eq!(
            bodies.len(),
            2,
            "tool path then plain path, no extra request"
        );
        assert!(bodies[0].get("tools").is_some());
        for body in &bodies {
            for key in ["reasoning", "reasoning_effort"] {
                assert_eq!(body.get(key), sent.get(key), "{format:?}: {body}");
            }
            assert!(!body.to_string().contains("format"), "{body}");
        }
    }
}

#[tokio::test]
async fn omitted_reasoning_and_unlisted_models_send_no_control() {
    let server = MockServer::start().await;
    stub(
        &server,
        "/v1/chat/completions",
        ResponseTemplate::new(200).set_body_json(chat_ok()),
    )
    .await;
    let mgr = manager(backend(
        &server,
        BackendType::OpenAICompatible,
        vec![
            model("plain", None, None),
            model(
                "tuned",
                None,
                effort(ReasoningEffort::High, Some(ReasoningFormat::OpenAI)),
            ),
        ],
    ))
    .await;
    for name in ["plain", "discovered/model"] {
        mgr.chat_with_tools_for_model(Some(name), &user(), &[], name)
            .await
            .unwrap();
        mgr.execute(&plain(name)).await.unwrap();
    }
    for body in bodies(&server).await {
        assert!(body.get("reasoning").is_none(), "{body}");
        assert!(body.get("reasoning_effort").is_none(), "{body}");
    }
}

#[tokio::test]
async fn native_responses_sends_effort_and_refuses_explicit_chat_format() {
    let server = MockServer::start().await;
    stub(
        &server,
        "/v1/responses",
        ResponseTemplate::new(200).set_body_json(responses_ok()),
    )
    .await;
    let responses = Some(ModelApi::Responses);
    let mgr = manager(backend(
        &server,
        BackendType::OpenAICompatible,
        vec![
            model("native", responses, effort(ReasoningEffort::High, None)),
            model(
                "native-openai",
                responses,
                effort(ReasoningEffort::High, Some(ReasoningFormat::OpenAI)),
            ),
            model(
                "native-router",
                responses,
                effort(ReasoningEffort::High, Some(ReasoningFormat::OpenRouter)),
            ),
        ],
    ))
    .await;
    mgr.chat_with_tools_for_model(Some("native"), &user(), &[tool()], "native")
        .await
        .unwrap();
    mgr.execute(&plain("native")).await.unwrap();
    let sent = bodies(&server).await;
    assert_eq!(sent.len(), 2);
    for body in &sent {
        assert_eq!(body["reasoning"], json!({"effort": "high"}));
        assert!(body.get("reasoning_effort").is_none());
    }
    for name in ["native-openai", "native-router"] {
        for error in [
            mgr.chat_with_tools_for_model(Some(name), &user(), &[], name)
                .await
                .err()
                .expect("expected an error"),
            mgr.execute(&plain(name))
                .await
                .expect_err("expected an error"),
            mgr.complete_bounded(Some(name), &user(), 16)
                .await
                .err()
                .expect("expected an error"),
        ] {
            assert!(matches!(error, LlmError::Configuration { .. }), "{error}");
            let text = error.to_string();
            assert!(
                text.contains("reasoning.format")
                    && text.contains("API is 'responses'")
                    && text.contains("Remove reasoning.format"),
                "{text}"
            );
        }
    }
    assert_eq!(bodies(&server).await.len(), 2, "refusals send nothing");
}

#[tokio::test]
async fn anthropic_effort_is_not_sent_and_explicit_format_is_refused() {
    let server = MockServer::start().await;
    stub(
        &server,
        "/v1/messages",
        ResponseTemplate::new(200).set_body_json(anthropic_ok()),
    )
    .await;
    let mgr = manager(backend(
        &server,
        BackendType::Anthropic,
        vec![
            model("plain", None, None),
            model("omitted", None, effort(ReasoningEffort::Low, None)),
            model(
                "explicit",
                None,
                effort(ReasoningEffort::Low, Some(ReasoningFormat::OpenRouter)),
            ),
        ],
    ))
    .await;
    for name in ["plain", "omitted"] {
        mgr.chat_with_tools_for_model(Some(name), &user(), &[], name)
            .await
            .unwrap();
    }
    let mut sent = bodies(&server).await;
    assert_eq!(sent.len(), 2);
    // The configured effort changes nothing on the wire (and is not claimed as
    // supported): the two bodies differ only by model id.
    sent[1]["model"] = json!("plain");
    assert_eq!(sent[0], sent[1]);
    assert!(!sent[1].to_string().contains("effort"));
    let error = mgr
        .chat_with_tools_for_model(Some("explicit"), &user(), &[], "explicit")
        .await
        .err()
        .expect("expected an error");
    assert!(matches!(error, LlmError::Configuration { .. }));
    assert!(
        error.to_string().contains("API is 'anthropic-messages'"),
        "{error}"
    );
    assert_eq!(bodies(&server).await.len(), 2);
}

/// One rejection per envelope shape: what the error says, and that exactly
/// one request was made (no retry without reasoning, no fallback).
async fn chat_rejection(
    reasoning: Option<ReasoningConfig>,
    status: u16,
    body: &str,
) -> (LlmError, usize) {
    let server = MockServer::start().await;
    stub(
        &server,
        "/v1/chat/completions",
        ResponseTemplate::new(status).set_body_raw(body.to_string(), "application/json"),
    )
    .await;
    let mgr = manager(backend(
        &server,
        BackendType::OpenAICompatible,
        vec![model("test-model", None, reasoning)],
    ))
    .await;
    let error = mgr
        .chat_with_tools_for_model(Some("test-model"), &user(), &[], "test-model")
        .await
        .err()
        .expect("expected an error");
    (error, server.received_requests().await.unwrap().len())
}

#[tokio::test]
async fn chat_rejections_keep_provider_text_and_name_the_sent_control() {
    let openai_none = effort(ReasoningEffort::None, Some(ReasoningFormat::OpenAI));
    let legacy_high = effort(ReasoningEffort::High, None);
    for (reasoning, status, body, provider_text, sent) in [
        (
            openai_none.clone(),
            400,
            r#"{"error":{"message":"Unsupported value: 'reasoning_effort' does not support 'none' with this model.","type":"invalid_request_error","param":"reasoning_effort","code":"unsupported_value"}}"#,
            "does not support 'none'",
            Some("reasoning_effort=none"),
        ),
        (
            legacy_high.clone(),
            400,
            r#"{"error":{"message":"Unrecognized request argument supplied: reasoning","type":"invalid_request_error","param":null,"code":null}}"#,
            "Unrecognized request argument",
            Some("reasoning.effort=high"),
        ),
        (
            legacy_high.clone(),
            400,
            r#"{"error":{"message":"Reasoning is mandatory for this endpoint and cannot be disabled.","code":400}}"#,
            "Reasoning is mandatory",
            Some("reasoning.effort=high"),
        ),
        (
            openai_none.clone(),
            422,
            r#"{"detail":"reasoning_effort: value is not one of low, high, max"}"#,
            "provider body: {\"detail\":\"reasoning_effort: value is not one of low, high, max\"}",
            Some("reasoning_effort=none"),
        ),
        // The same rejection without a configured control gets no attribution.
        (
            None,
            422,
            r#"{"detail":"unprocessable"}"#,
            "provider body: {\"detail\":\"unprocessable\"}",
            None,
        ),
        (
            None,
            400,
            r#"{"error":{"message":"Unrecognized request argument supplied: foo","type":"invalid_request_error","param":null,"code":null}}"#,
            "Unrecognized request argument",
            None,
        ),
    ] {
        let (error, requests) = chat_rejection(reasoning, status, body).await;
        assert_eq!(requests, 1, "{body}");
        assert!(matches!(error, LlmError::InvalidRequest { .. }), "{error}");
        assert!(!error.is_retryable());
        let text = error.to_string();
        assert!(text.contains(provider_text), "{text}");
        match sent {
            Some(sent) => assert!(
                text.contains(&format!(
                    "request to backend 'stub' model 'test-model' included {sent}; the provider did not say whether that field caused the rejection"
                )),
                "{text}"
            ),
            None => assert!(!text.contains("included"), "{text}"),
        }
    }
}

#[tokio::test]
async fn bare_error_bodies_are_bounded_flattened_and_redacted() {
    let body = format!(
        "{{\"detail\":\"bad key {KEY}\",\n\"trace\":\"{}\"}}",
        "x".repeat(2_000)
    );
    let (error, requests) = chat_rejection(effort(ReasoningEffort::Low, None), 422, &body).await;
    assert_eq!(requests, 1);
    let text = error.to_string();
    assert!(!text.contains(KEY), "{text}");
    assert!(text.contains("bad key [redacted]"), "{text}");
    assert!(!text.contains('\n'), "{text}");
    assert!(text.contains("more chars)"), "{text}");
    assert!(text.len() < 1_000, "bounded: {} bytes", text.len());
    assert!(text.contains("included reasoning.effort=low"), "{text}");
}

#[tokio::test]
async fn responses_rejections_are_attributed_and_bounded() {
    for (status, body, provider_text) in [
        (
            400,
            r#"{"error":{"message":"Unsupported value: 'xhigh' is not supported with this model.","code":"unsupported_value"}}"#,
            "unsupported_value: Unsupported value: 'xhigh'",
        ),
        (
            422,
            "upstream says no\n\nreasoning rejected",
            "upstream says no reasoning rejected",
        ),
    ] {
        let server = MockServer::start().await;
        stub(
            &server,
            "/v1/responses",
            ResponseTemplate::new(status).set_body_raw(body, "application/json"),
        )
        .await;
        let mgr = manager(backend(
            &server,
            BackendType::OpenAICompatible,
            vec![model(
                "test-model",
                Some(ModelApi::Responses),
                effort(ReasoningEffort::XHigh, None),
            )],
        ))
        .await;
        let error = mgr
            .chat_with_tools_for_model(Some("test-model"), &user(), &[], "test-model")
            .await
            .err()
            .expect("expected an error");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert!(matches!(error, LlmError::InvalidRequest { .. }), "{error}");
        assert!(!error.is_retryable());
        let text = error.to_string();
        assert!(text.contains(provider_text), "{text}");
        assert!(
            text.contains("model 'test-model' included reasoning.effort=xhigh"),
            "{text}"
        );
    }
}

#[tokio::test]
async fn http_200_error_envelopes_keep_their_classification() {
    // OpenRouter can report an upstream failure inside a 200 body. Decoding the
    // success body ourselves must not turn a retryable 502 into a client error.
    for (body, retryable) in [
        (
            r#"{"error":{"message":"Upstream unavailable","code":502}}"#,
            true,
        ),
        (r#"{"error":{"message":"Bad effort","code":400}}"#, false),
    ] {
        let (error, requests) =
            chat_rejection(effort(ReasoningEffort::High, None), 200, body).await;
        assert_eq!(requests, 1);
        assert_eq!(error.is_retryable(), retryable, "{error}");
        if !retryable {
            assert!(
                error.to_string().contains("included reasoning.effort=high"),
                "{error}"
            );
        }
    }
    // A malformed success body is reported without quoting it.
    let (error, _) = chat_rejection(None, 200, r#"{"id":"secret transcript"}"#).await;
    assert!(
        error
            .to_string()
            .starts_with("Invalid request: Response deserialize error")
    );
    assert!(!error.to_string().contains("secret transcript"), "{error}");
}
