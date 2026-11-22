use super::*;

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex as SyncMutex;

use serde_json::{Value, json};

use crate::backends::BackendManager;
use crate::cache::CacheOptions;
use crate::context::{ContextBuilder, estimate_request_tokens};
use crate::extension::{ExtensionHub, ExtensionRef, HookKind, append_event};
use crate::runtime::{
    self, CallPreparation, ModelCallScope, RuntimeMessage, RuntimeRecord, RuntimeRecorder,
};
use crate::test_support::harness::{
    empty_secrets, fresh_session, permissive_security, tool_context,
};
use crate::test_support::mock_backend::{MockBackend, RecordedCall};
use crate::tool::{Tool, ToolDescriptor, ToolError, ToolPolicyRegistry, ToolRegistry};

const PROFILE: &str = r#"
tool_profiles:
  compact:
    default: full
    tools: {todos__get_prompt: hidden}
"#;
const DEFER: &str = "    defer_bundles: [config/todos, ext/task-pane/peek]\n";
const DECLARATIONS: &str = r#"
tool_bundles:
  - id: todos
    tools: [todos__todo_get, todos__todo_update, todos__get_prompt, todos__late]
"#;
const GET: &str = "todos__todo_get";
const UPDATE: &str = "todos__todo_update";
const HIDDEN: &str = "todos__get_prompt";
const LATE: &str = "todos__late";
const SKILL: &str = "test_skill";
const PANE: &str = "test_pane";
const DISCOVERY: &[&str] = &[SKILL, PANE, NO_REPLY_TOOL];
const DETAIL: &[&str] = &[SKILL, PANE, GET, UPDATE, NO_REPLY_TOOL];
const PREVIEW: &[&str] = &[SKILL, PANE, GET, NO_REPLY_TOOL];

struct NativeFixture(&'static str);

impl Extension for NativeFixture {
    fn name(&self) -> &'static str {
        self.0
    }

    fn supported_hooks(&self) -> &[HookKind] {
        &[]
    }

    fn tool_bundles(&self) -> Vec<ToolBundleDeclaration> {
        if self.0 == "task-pane" {
            vec![ToolBundleDeclaration {
                id: "peek".into(),
                tools: vec![GET.into()],
            }]
        } else {
            Vec::new()
        }
    }
}

fn parse(yaml: &str, extensions: &[Arc<dyn Extension>]) -> anyhow::Result<ToolProfile> {
    let cfg = serde_yaml::from_str(yaml)?;
    Ok(build_profiles(&cfg, extensions)?.remove("compact").unwrap())
}

#[test]
fn declarations_validate_before_use_without_requiring_registered_members() {
    let extensions: Vec<Arc<dyn Extension>> = vec![Arc::new(NativeFixture("task-pane"))];
    let fixture = format!("{PROFILE}{DEFER}{DECLARATIONS}");
    assert!(crate::config::check_unknown_config_keys(&fixture).is_empty());
    let profile = parse(&fixture, &extensions).unwrap();
    assert!(profile.deferred_bundles.members.contains(LATE));
    assert!(parse(PROFILE, &[]).unwrap().deferred_bundles.is_empty());
    for (yaml, diagnostic) in [
        (
            fixture.replace("config/todos,", "config/missing,"),
            "unknown bundle 'config/missing'",
        ),
        (
            fixture.replace("config/todos,", "todos,"),
            "malformed bundle ID 'todos'",
        ),
        (
            fixture.replace("id: todos", "id: task/todos"),
            "malformed bundle ID 'config/task/todos'",
        ),
        (
            fixture.replace(
                "tools: [todos__todo_get",
                "tools: [todos__*, todos__todo_get",
            ),
            "malformed or reserved tool name 'todos__*'",
        ),
        (
            fixture.replace("todos__late]", "no_reply]"),
            "reserved tool name 'no_reply'",
        ),
        (
            fixture.replace("todos__late]", "todos__todo_get]"),
            "duplicate tool 'todos__todo_get'",
        ),
        (
            format!("{fixture}  - id: todos\n    tools: [todos__todo_get]\n"),
            "duplicate bundle 'config/todos'",
        ),
    ] {
        let err = parse(&yaml, &extensions).unwrap_err().to_string();
        assert!(err.contains(diagnostic), "{err}");
    }
    let duplicate = [extensions[0].clone(), extensions[0].clone()];
    assert!(
        parse(&fixture, &duplicate)
            .unwrap_err()
            .to_string()
            .contains("duplicate bundle 'ext/task-pane/peek'")
    );
    assert!(parse(&fixture, &extensions).is_ok());
}

struct FixtureTool(&'static str);

type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>>;

impl Tool for FixtureTool {
    fn descriptor(&self) -> ToolDescriptor {
        let parameters = if self.0 == GET || self.0 == UPDATE {
            json!({"type":"object", "required":["task"], "additionalProperties":false, "properties": {
                "task": {"type":"object", "required":["id"], "additionalProperties":false,
                    "properties":{"id":{"type":"string", "description":"Task identifier. ".repeat(180)}}}
            }})
        } else {
            json!({"type":"object", "properties":{}, "required":[], "additionalProperties":false})
        };
        ToolDescriptor {
            name: self.0.into(),
            description: "Fixture tool".into(),
            parameters,
        }
    }

    fn strict_schema(&self) -> bool {
        true
    }

    fn execute<'a>(&'a self, args: Value, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            if self.0 == GET {
                return Ok(format!("read {}", args["task"]["id"].as_str().unwrap()));
            }
            let Some(action) = args["action"].as_str() else {
                return Ok("observed".into());
            };
            let mut handle = ctx
                .bundle_presentation
                .clone()
                .expect("attributed, active native tool");
            if self.0 == SKILL {
                handle = handle
                    .for_resolved_skill("operator-disk/todos".into())
                    .unwrap();
            }
            let wants: &[&str] = match action {
                "detail" => &["config/todos"],
                "preview" => &["ext/task-pane/peek"],
                "invalid" => &["ext/task-pane/peek", "config/missing"],
                "close" => &[],
                _ => panic!("unexpected fixture action"),
            };
            handle
                .set_wanted(wants)
                .await
                .map_err(|err| err.to_string())?;
            Ok("presentation changed".into())
        })
    }
}

struct Fixture {
    hub: ExtensionHub,
    registry: Arc<ToolRegistry>,
    db: Database,
    a: ToolContext,
    b: ToolContext,
    // The databases remain open for the test's lifetime.
    _instances: (eidetica::Instance, eidetica::Instance),
}

impl Fixture {
    async fn new() -> Self {
        let extensions: Vec<Arc<dyn Extension>> = ["skills", "task-pane", "mcp-todos"]
            .into_iter()
            .map(|name| Arc::new(NativeFixture(name)) as _)
            .collect();
        let profile = parse(&format!("{PROFILE}{DEFER}{DECLARATIONS}"), &extensions).unwrap();
        let mut hub = ExtensionHub::new();
        hub.install_all(extensions).await.unwrap();
        let registry = Arc::new(ToolRegistry::new());
        for (name, owner) in [
            (SKILL, "skills"),
            (PANE, "task-pane"),
            (GET, "mcp-todos"),
            (UPDATE, "mcp-todos"),
            (HIDDEN, "mcp-todos"),
        ] {
            registry.register_arc_owned(Arc::new(FixtureTool(name)), Some(owner));
        }
        registry.announce_pending_source("todos");
        let (i, a) = fresh_session().await;
        let (j, b) = fresh_session().await;
        let db = a.lock().await.database().clone();
        for session in [&a, &b] {
            hub.record_active(session.lock().await.database())
                .await
                .unwrap();
        }
        let make_context = |session| {
            let mut ctx = tool_context(session, registry.clone());
            ctx.active_extensions = hub
                .extension_names()
                .into_iter()
                .map(str::to_owned)
                .collect();
            ctx.tools = ctx
                .tools
                .with_active_extensions(Some(ctx.active_extensions.clone()));
            ctx.profile = profile.clone();
            ctx.allow_no_reply = true;
            ctx
        };
        let a = make_context(a);
        let b = make_context(b);
        Self {
            hub,
            registry,
            db,
            a,
            b,
            _instances: (i, j),
        }
    }

    async fn prepare(&self, ctx: &ToolContext) -> PreparedToolPresentation {
        self.hub
            .prepare_tool_presentation(ctx, || true)
            .await
            .unwrap()
            .unwrap()
    }

    async fn run(
        &self,
        ctx: &ToolContext,
        mock: Arc<MockBackend>,
        recorder: Arc<Observations>,
        budget: Option<usize>,
    ) -> Result<String, String> {
        let presentation = self
            .hub
            .prepare_tool_presentation(ctx, || true)
            .await
            .unwrap();
        let tools = presentation
            .as_ref()
            .map_or_else(|| ctx.definitions(), |view| view.initial.advertised.clone());
        let built = ContextBuilder::new(&[], &ctx.agent_name, "Fixture", &Default::default())
            .with_tools(&tools)
            .build()
            .await;
        let backend = BackendManager::with_mock(mock, empty_secrets().await);
        runtime::execute_with_recorder(
            None,
            built.messages,
            &backend,
            &permissive_security(),
            ctx,
            &ToolPolicyRegistry::empty(),
            Some(recorder),
            Some(&self.hub),
            None,
            ModelCallScope {
                request_budget_tokens: budget.or(Some(built.request_budget_tokens)),
                cache: built.cache,
                sources: built.sources,
                tool_presentation: presentation,
                ..Default::default()
            },
        )
        .await
        .map(|result| result.body)
    }

    async fn toggle(&self, name: &str, enabled: bool) {
        let timestamp = latest_events(&self.db)
            .await
            .unwrap()
            .values()
            .map(ExtensionEvent::timestamp)
            .max()
            .unwrap()
            + chrono::Duration::milliseconds(1);
        let event = if enabled {
            ExtensionEvent::Activated {
                name: name.into(),
                extension_ref: ExtensionRef::builtin(name),
                timestamp,
            }
        } else {
            ExtensionEvent::Deactivated {
                name: name.into(),
                timestamp,
            }
        };
        append_event(&self.db, event).await.unwrap();
    }
}

#[derive(Default)]
struct Observations(SyncMutex<Vec<(Vec<ToolDefinition>, u64, CacheOptions)>>);

impl RuntimeRecorder for Observations {
    fn prepare_call<'a>(
        &'a self,
        call: CallPreparation<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        self.0.lock().unwrap().push((
            call.tools.to_vec(),
            call.observation.input_tokens,
            call.cache.clone(),
        ));
        Box::pin(async { Ok(()) })
    }

    fn record(
        &self,
        _: RuntimeRecord,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }
}

fn names(defs: &[ToolDefinition]) -> Vec<&str> {
    defs.iter().map(|def| def.name.as_str()).collect()
}

fn action(mock: &MockBackend, id: &str, tool: &str, action: &str) {
    mock.push_tool_calls([(
        id.into(),
        tool.into(),
        json!({"action": action}).to_string(),
    )]);
}

fn read(mock: &MockBackend, name: &str) {
    mock.push_tool_calls([(
        "read".into(),
        name.into(),
        json!({"task":{"id":"fixture"}}).to_string(),
    )]);
}

fn result_contains(call: &RecordedCall, needle: &str) -> bool {
    call.messages.iter().any(
        |m| matches!(m, RuntimeMessage::ToolResult { content, .. } if content.contains(needle)),
    )
}

#[tokio::test]
async fn next_request_uses_session_slots_and_the_same_tools_for_budgeting() {
    let f = Fixture::new().await;
    let mock = Arc::new(MockBackend::new());
    let recorder = Arc::new(Observations::default());
    let initial = f.prepare(&f.a).await.initial;
    assert_eq!(
        initial.eligible_names, DETAIL,
        "Hidden stays ineligible; deferred Full stays eligible"
    );
    assert_eq!(names(&initial.advertised), DISCOVERY);
    assert!(
        f.a.tools.get(GET).is_some(),
        "deferral cannot change lookup"
    );

    read(&mock, GET); // Known name, not advertised.
    action(&mock, "1", SKILL, "detail");
    action(&mock, "2", SKILL, "detail");
    action(&mock, "3", PANE, "detail");
    action(&mock, "4", PANE, "invalid"); // No partial replacement.
    action(&mock, "5", PANE, "preview");
    action(&mock, "6", SKILL, "close");
    mock.push_text("preview open");
    // Observe while A still has its preview open, not after both are empty.
    mock.push_text("B unaffected");
    action(&mock, "7", PANE, "close");
    mock.push_text("closed");
    assert_eq!(
        f.run(&f.a, mock.clone(), recorder.clone(), None)
            .await
            .unwrap(),
        "preview open"
    );
    assert_eq!(
        f.run(&f.b, mock.clone(), recorder.clone(), None)
            .await
            .unwrap(),
        "B unaffected"
    );
    assert_eq!(
        f.run(&f.a, mock.clone(), recorder.clone(), None)
            .await
            .unwrap(),
        "closed"
    );
    let calls = mock.recorded_calls();
    assert_eq!(
        calls.iter().map(|c| names(&c.tools)).collect::<Vec<_>>(),
        [
            DISCOVERY, DISCOVERY, DETAIL, DETAIL, DETAIL, DETAIL, DETAIL, PREVIEW, DISCOVERY,
            PREVIEW, DISCOVERY
        ]
    );
    assert!(result_contains(&calls[1], "read fixture"));
    assert!(result_contains(
        &calls[5],
        "unknown tool bundle 'config/missing'"
    ));
    assert_eq!(
        calls[2].tools[2].parameters["properties"]["task"]["properties"]["id"]["type"],
        "string"
    );
    assert!(calls[2].tools[2].strict);

    // The old profile advertises exactly the same serialized definitions as
    // a plain Full/Hidden scope, even with declarations present but unused.
    let mut eager = f.a.clone();
    eager.profile = parse(&format!("{PROFILE}{DECLARATIONS}"), &[]).unwrap();
    assert!(
        f.hub
            .prepare_tool_presentation(&eager, || true)
            .await
            .unwrap()
            .is_none()
    );
    mock.push_text("eager");
    f.run(&eager, mock.clone(), recorder.clone(), None)
        .await
        .unwrap();
    let calls = mock.recorded_calls();
    assert_eq!(
        serde_json::to_vec(&calls.last().unwrap().tools).unwrap(),
        serde_json::to_vec(&calls[2].tools).unwrap()
    );
    let observations = recorder.0.lock().unwrap();
    assert_eq!(observations.len(), calls.len());
    for (call, (tools, tokens, cache)) in calls.iter().zip(observations.iter()) {
        assert_eq!(call.tools, *tools);
        assert_eq!(
            *tokens,
            estimate_request_tokens(&call.messages, &call.tools) as u64
        );
        assert_eq!(call.cache, *cache);
    }
}

#[tokio::test]
async fn expansion_refuses_a_second_request_that_no_longer_fits() {
    let f = Fixture::new().await;
    let mock = Arc::new(MockBackend::new());
    let record = Arc::new(Observations::default());
    let initial = f.prepare(&f.a).await.initial;
    let built = ContextBuilder::new(&[], &f.a.agent_name, "Fixture", &Default::default())
        .with_tools(&initial.advertised)
        .build()
        .await;
    let tight = estimate_request_tokens(&built.messages, &initial.advertised) + 100;
    action(&mock, "1", SKILL, "detail");
    mock.push_text("must not be sent");
    let err = f
        .run(&f.a, mock.clone(), record.clone(), Some(tight))
        .await
        .unwrap_err();
    assert!(err.contains("budget"), "{err}");
    assert_eq!(mock.recorded_calls().len(), 1);
    assert_eq!(record.0.lock().unwrap().len(), 1);
    assert_eq!(mock.pending(), 1);
}

#[tokio::test]
async fn revocation_ambiguity_and_reset_win_without_changing_lookup() {
    let f = Fixture::new().await;
    let turn = f.prepare(&f.a).await.turn;
    let pane = turn.handle("task-pane").await.unwrap().unwrap();
    let skill = turn.handle("skills").await.unwrap().unwrap();
    skill.set_wanted(&["ext/task-pane/peek"]).await.unwrap();
    assert_eq!(names(&f.prepare(&f.a).await.initial.advertised), PREVIEW);
    // No snapshot between disable and re-enable: the old activation still dies.
    f.toggle("task-pane", false).await;
    f.toggle("task-pane", true).await;
    assert_eq!(names(&f.prepare(&f.a).await.initial.advertised), DISCOVERY);
    assert!(pane.set_wanted(&["config/todos"]).await.is_err());

    skill.set_wanted(&["config/todos"]).await.unwrap();
    let mut other_agent = f.a.clone();
    other_agent.agent_name = "other-agent".into();
    assert_eq!(
        names(&f.prepare(&other_agent).await.initial.advertised),
        DISCOVERY
    );
    f.registry.register(FixtureTool(GET)); // Same name, different owner; still callable by old routing.
    assert_eq!(
        names(&f.prepare(&f.a).await.initial.advertised),
        &[SKILL, PANE, UPDATE, NO_REPLY_TOOL]
    );
    assert!(f.a.tools.get(GET).is_some());

    let mock = Arc::new(MockBackend::new());
    let run = f.run(&f.a, mock.clone(), Arc::default(), None);
    mock.push_text("before revoke");
    run.await.unwrap();
    assert!(
        mock.recorded_calls()[0]
            .tools
            .iter()
            .any(|def| def.name == UPDATE)
    );
    f.toggle("mcp-todos", false).await;
    read(&mock, UPDATE);
    mock.push_text("after revoke");
    f.run(&f.a, mock.clone(), Arc::default(), None)
        .await
        .unwrap();
    let calls = mock.recorded_calls();
    assert_eq!(names(&calls[1].tools), DISCOVERY);
    assert!(result_contains(
        &calls[2],
        "extension 'mcp-todos' is inactive"
    ));

    let sid = f.db.root_id().to_string();
    f.hub.reset_tool_presentation(&sid).await;
    assert!(skill.set_wanted(&["config/todos"]).await.is_err());
    f.toggle("mcp-todos", true).await;
    let fresh = f
        .prepare(&f.a)
        .await
        .turn
        .handle("skills")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(names(&f.prepare(&f.a).await.initial.advertised), DISCOVERY);
    f.hub.close_tool_presentation(&sid).await;
    assert!(fresh.set_wanted(&["config/todos"]).await.is_err());
    assert_eq!(names(&f.prepare(&f.a).await.initial.advertised), DISCOVERY);
}

#[tokio::test]
async fn retry_reuses_the_envelope_even_if_the_slot_is_released() {
    let f = Fixture::new().await;
    let turn = f.prepare(&f.a).await.turn;
    let pane = turn.handle("task-pane").await.unwrap().unwrap();
    pane.set_wanted(&["config/todos"]).await.unwrap();
    let mock = Arc::new(MockBackend::new());
    let gate = mock.block_next_call();
    mock.push_err(crate::error::LlmError::Timeout);
    mock.push_text("retried");
    let record = Arc::new(Observations::default());
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        tokio::try_join!(f.run(&f.a, mock.clone(), record.clone(), None), async {
            gate.wait_started().await;
            pane.set_wanted(&[]).await.unwrap();
            gate.release();
            Ok(())
        })
        .unwrap();
    })
    .await
    .unwrap();
    let calls = mock.recorded_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(names(&calls[0].tools), DETAIL);
    assert_eq!(calls[0].tools, calls[1].tools);
    assert_eq!(calls[0].messages, calls[1].messages);
    assert_eq!(calls[0].cache, calls[1].cache);
    assert_eq!(record.0.lock().unwrap().len(), 1, "one logical call");
    assert_eq!(names(&f.prepare(&f.a).await.initial.advertised), DISCOVERY);
}

#[tokio::test]
async fn late_registration_cannot_expand_a_durable_child_pin_after_resume() {
    let f = Fixture::new().await;
    f.registry.register(FixtureTool(runtime::JOB_WAIT_TOOL));
    let pin = f.a.tools.permitted_names();
    assert!(
        pin.iter().any(|name| name == GET),
        "pin is authority, not advertisement"
    );
    assert!(!pin.iter().any(|name| name == LATE));
    let parent = f
        .prepare(&f.a)
        .await
        .turn
        .handle("skills")
        .await
        .unwrap()
        .unwrap();
    parent.set_wanted(&["config/todos"]).await.unwrap();
    f.registry
        .register_arc_owned(Arc::new(FixtureTool(LATE)), Some("mcp-todos"));
    f.registry.finish_pending_source("todos");
    let mut child = f.b.clone();
    child.tools = ScopedTools::new(f.registry.clone(), Some(pin));
    assert!(names(&f.prepare(&f.a).await.initial.advertised).contains(&LATE));
    assert!(!names(&f.prepare(&child).await.initial.advertised).contains(&GET));
    assert!(child.tools.get(LATE).is_none());

    // Simulate a cold executor resuming a pending wait: prior activation is
    // replayed as history, not a native call. The persisted pin is unchanged.
    let cold = ExtensionHub::new();
    let mock = Arc::new(MockBackend::new());
    mock.push_text("resumed");
    let call = runtime::ToolCallRequest {
        id: "wait".into(),
        name: runtime::JOB_WAIT_TOOL.into(),
        arguments: "{}".into(),
    };
    let exchange = |calls| RuntimeMessage::AssistantToolCalls {
        exchange_id: None,
        content: None,
        tool_calls: calls,
        provider_extra: Default::default(),
    };
    let now = Utc::now();
    runtime::resume_with_recorder(
        None,
        vec![RuntimeMessage::User("resume fixture".into())],
        &BackendManager::with_mock(mock.clone(), empty_secrets().await),
        &permissive_security(),
        &child,
        &ToolPolicyRegistry::empty(),
        None,
        Some(&cold),
        None,
        runtime::RuntimeResume {
            prior_messages: vec![
                exchange(vec![runtime::ToolCallRequest {
                    id: "open".into(),
                    name: SKILL.into(),
                    arguments: r#"{"action":"detail"}"#.into(),
                }]),
                RuntimeMessage::ToolResult {
                    call_id: "open".into(),
                    content: "presentation changed".into(),
                },
                exchange(vec![call.clone()]),
            ],
            prior_model_sequences: vec![0, 0, 1],
            model_sequence: 1,
            call_index: 0,
            call,
            deadline: now + chrono::Duration::seconds(20),
            policy_deadline: now + chrono::Duration::seconds(20),
            policy_timeout_seconds: 20,
            metadata: Vec::new(),
            matrix_send_queued: false,
            rate_limited_calls: Vec::new(),
        },
        ModelCallScope::default(),
    )
    .await
    .unwrap();
    let calls = mock.recorded_calls();
    assert_eq!(calls.len(), 1);
    assert!(!names(&calls[0].tools).contains(&LATE));
    assert!(!names(&calls[0].tools).contains(&GET));
    // Even an explicit fresh want in the child can only expose pinned names.
    let prepared = cold
        .prepare_tool_presentation(&child, || true)
        .await
        .unwrap()
        .unwrap();
    prepared
        .turn
        .handle("skills")
        .await
        .unwrap()
        .unwrap()
        .set_wanted(&["config/todos"])
        .await
        .unwrap();
    let visible = prepared
        .turn
        .snapshot(&child, true)
        .await
        .unwrap()
        .advertised;
    assert!(names(&visible).contains(&GET));
    assert!(!names(&visible).contains(&LATE));
}
