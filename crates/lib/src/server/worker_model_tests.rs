//! Spawn-time Worker model / preset role_suffix propagation, proved on the
//! real child execution path: every child LLM request is recorded by
//! `MockBackend` (no provider fallback), so the assertions cover the actual
//! request model and system prompt, not just a resolved struct.

use super::*;
use crate::agent_db::WorkerDbConfig;
use crate::config::AgentPreset;
use crate::tool::Tool;
use std::time::Duration;

struct Spawned {
    server: Arc<Server>,
    mock: Arc<crate::test_support::MockBackend>,
    result: Result<String, String>,
}

/// Seed a "default" agent whose `helper` Worker carries a template model,
/// a preset with its own model + role_suffix, then run one synchronous
/// `spawn_worker` tool call directly and capture the child's recorded call.
async fn spawn_with(
    worker: WorkerDbConfig,
    args: serde_json::Value,
    script: Option<Result<crate::runtime::LLMResponse, crate::error::LlmError>>,
) -> Spawned {
    let (_instance, server, registry) = server_fixture().await;
    let (_, adb) = seed_agent(&server, &registry, "default").await;
    let mut base = adb.read_config().await.unwrap();
    base.workers = vec![worker];
    adb.write_config(&base).await.unwrap();
    server
        .hydrate_agent_from_db(server.agents.default_agent().clone())
        .await;
    let (sid, db) = server
        .registry
        .create_session(Some("model-parent"))
        .await
        .unwrap();
    let slot = crate::instance::ServerSlot::default();
    slot.set(server.clone());
    let mock = Arc::new(crate::test_support::MockBackend::new());
    match script {
        Some(Ok(response)) => mock.push_response(Ok(response)),
        Some(Err(err)) => mock.push_err(err),
        None => mock.push_text("worker response"),
    }
    let backend = BackendManager::with_mock(
        mock.clone(),
        crate::security::SecretStore::new(registry.chaz_peer().clone()).await,
    );
    let tool = crate::tools::SpawnWorker {
        server: slot,
        backend,
        security: crate::test_support::permissive_security(),
    };
    let mut ctx = crate::test_support::tool_context(
        Arc::new(Mutex::new(
            Session::new(ConversationId(sid.to_string()), db).await,
        )),
        Arc::new(ToolRegistry::new()),
    );
    ctx.agent_name = "default".into();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        tool.execute(
            serde_json::json!({"name":"helper","task":"do work", "preset": args.get("preset").cloned().unwrap_or(serde_json::Value::Null), "model": args.get("model").cloned().unwrap_or(serde_json::Value::Null)}),
            &ctx,
        ),
    )
    .await
    .expect("spawn_worker timed out");
    Spawned {
        server,
        mock,
        result: result.map_err(|e| e.to_string()),
    }
}

fn helper_worker() -> WorkerDbConfig {
    let mut presets = std::collections::HashMap::new();
    presets.insert(
        "reviewer".to_string(),
        AgentPreset {
            model: Some("preset-model".into()),
            role_suffix: Some("Act as a terse reviewer.".into()),
            ..Default::default()
        },
    );
    WorkerDbConfig {
        name: "helper".into(),
        system_prompt: "BASE PROMPT".into(),
        model: Some("template-model".into()),
        presets,
        ..Default::default()
    }
}

/// Assert the single recorded child call's model and system prompt.
fn child_call(spawned: &Spawned) -> (String, Vec<String>) {
    let calls = spawned.mock.recorded_calls();
    assert_eq!(calls.len(), 1, "expected exactly one child LLM request");
    let call = &calls[0];
    let system_prompts: Vec<String> = call
        .messages
        .iter()
        .filter_map(|m| match m {
            crate::runtime::RuntimeMessage::System(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    (call.model.clone(), system_prompts)
}

#[tokio::test]
async fn template_model_and_prompt_reach_child_request_without_preset() {
    let spawned = spawn_with(helper_worker(), serde_json::json!({}), None).await;
    assert_eq!(
        spawned
            .result
            .clone()
            .unwrap_or_else(|e| panic!("worker failed: {e}")),
        "worker response"
    );
    let (model, prompts) = child_call(&spawned);
    assert_eq!(model, "template-model");
    assert_eq!(prompts, vec!["BASE PROMPT"]);
    spawned.server.shutdown().await;
}

#[tokio::test]
async fn preset_overrides_model_and_appends_role_suffix_to_child_prompt() {
    let spawned = spawn_with(
        helper_worker(),
        serde_json::json!({"preset":"reviewer"}),
        None,
    )
    .await;
    assert_eq!(
        spawned
            .result
            .clone()
            .unwrap_or_else(|e| panic!("worker failed: {e}")),
        "worker response"
    );
    let (model, prompts) = child_call(&spawned);
    assert_eq!(model, "preset-model");
    assert_eq!(prompts, vec!["BASE PROMPT\n\nAct as a terse reviewer."]);
    spawned.server.shutdown().await;
}

#[tokio::test]
async fn inline_model_wins_over_preset_while_role_suffix_still_applies() {
    let spawned = spawn_with(
        helper_worker(),
        serde_json::json!({"preset":"reviewer","model":"inline-model"}),
        None,
    )
    .await;
    assert_eq!(
        spawned
            .result
            .clone()
            .unwrap_or_else(|e| panic!("worker failed: {e}")),
        "worker response"
    );
    let (model, prompts) = child_call(&spawned);
    assert_eq!(model, "inline-model");
    assert_eq!(prompts, vec!["BASE PROMPT\n\nAct as a terse reviewer."]);
    spawned.server.shutdown().await;
}

#[tokio::test]
async fn omitted_model_keeps_agent_default_and_plain_prompt() {
    let worker = WorkerDbConfig {
        name: "helper".into(),
        system_prompt: "BASE PROMPT".into(),
        ..Default::default()
    };
    // seed_agent pins the agent's model to "test-model"; with no template,
    // preset or inline override the child must use it unchanged.
    let spawned = spawn_with(worker, serde_json::json!({}), None).await;
    assert_eq!(
        spawned
            .result
            .clone()
            .unwrap_or_else(|e| panic!("worker failed: {e}")),
        "worker response"
    );
    let (model, prompts) = child_call(&spawned);
    assert_eq!(model, "test-model");
    assert_eq!(prompts, vec!["BASE PROMPT"]);
    spawned.server.shutdown().await;
}

#[tokio::test]
async fn unsupported_model_surfaces_error_instead_of_silent_fallback() {
    let spawned = spawn_with(
        helper_worker(),
        serde_json::json!({"model":"ghost-model"}),
        Some(Err(crate::error::LlmError::Configuration {
            message: "model ghost-model not found".into(),
        })),
    )
    .await;
    // The request must have carried the requested model, and the failure must
    // reach the caller as an error — never a silent switch to another model.
    let (model, _) = child_call(&spawned);
    assert_eq!(model, "ghost-model");
    let error = match spawned.result {
        Err(error) => error,
        Ok(text) => panic!("child failure must surface to the spawning agent, got {text}"),
    };
    assert!(
        error.to_string().contains("ghost-model"),
        "error should name the requested model: {error}"
    );
    spawned.server.shutdown().await;
}
