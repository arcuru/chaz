//! Workflow execution engine.
//!
//! Walks a normalized [`FlowSpec`] tree, delegating each `spawn` node
//! through a [`Delegate`] (the production impl wraps `spawn_agent`; tests
//! inject a canned one). The tree walk is the orchestration logic — task
//! templating (`{previous}`), sequence chaining, fork concurrency, and
//! join reduction — and is deliberately decoupled from the spawn
//! machinery so it is testable without a running `Server`.

use super::spec::{
    AgentReducer, FlowSpec, ForkSpec, JoinMode, JoinReducer, JoinSpec, OnFailure, OutputMode,
    SequenceSpec, SpawnSpec,
};
use crate::tool::{ToolContext, ToolError};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Default fork concurrency cap when a fork doesn't specify one.
const DEFAULT_CONCURRENCY: usize = 4;

/// One-shot delegation to a child agent. Abstracts `spawn_agent` so the
/// executor can run against a mock in tests.
pub(crate) trait Delegate: Send + Sync {
    fn spawn<'a>(
        &'a self,
        agent: &'a str,
        task: &'a str,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>>;
}

/// Per-run scratch state. Holds completed fork outputs keyed by fork id so
/// a later join in the same sequence can collect them.
struct RunState {
    forks: HashMap<String, Value>,
    spawn_limit: Arc<Semaphore>,
}

impl Default for RunState {
    fn default() -> Self {
        Self {
            forks: HashMap::new(),
            spawn_limit: Arc::new(Semaphore::new(16)),
        }
    }
}

/// Substitute the `{previous}` placeholder in a task template with the
/// prior step's output text. If the template has no placeholder the task
/// is returned unchanged (callers that want the prior context regardless
/// can inspect `previous` separately).
fn apply_previous(task: &str, previous: Option<&str>) -> String {
    match previous {
        Some(prev) if task.contains("{previous}") => task.replace("{previous}", prev),
        _ => task.to_string(),
    }
}

/// Parse a `json`-mode agent reply into a `Value`, tolerating a markdown
/// code fence around the JSON (agents love fencing).
fn parse_json_text(text: &str) -> Result<Value, ToolError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(ToolError::Execution(
            "expected JSON output but agent returned empty text".into(),
        ));
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return Ok(v);
    }
    // Strip a ```json ... ``` (or bare ```) fence and retry.
    let stripped = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```"))
        .map(str::trim);
    if let Some(inner) = stripped
        && let Ok(v) = serde_json::from_str::<Value>(inner)
    {
        return Ok(v);
    }
    Err(ToolError::Execution(
        "expected valid JSON output from delegated agent".into(),
    ))
}

/// Render a node output back to a plain string for forwarding as
/// `{previous}` context to the next sequence step.
fn output_to_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Execute one spawn node.
async fn run_spawn(
    spec: &SpawnSpec,
    previous: Option<&str>,
    delegate: &dyn Delegate,
    ctx: &ToolContext,
    spawn_limit: &Semaphore,
) -> Result<Value, ToolError> {
    let _permit = spawn_limit.acquire().await.expect("semaphore not closed");
    let task = apply_previous(&spec.task, previous);
    let text = delegate.spawn(&spec.agent, &task, ctx).await?;
    match spec.output {
        Some(OutputMode::Json) => parse_json_text(&text),
        _ => Ok(Value::String(text)),
    }
}

/// Execute a sequence, threading each step's output into the next as
/// `{previous}`. The sequence's output is its last step's output.
async fn run_sequence(
    spec: &SequenceSpec,
    previous: Option<&str>,
    state: &mut RunState,
    delegate: &dyn Delegate,
    ctx: &ToolContext,
) -> Result<Value, ToolError> {
    let mut prev_text: Option<String> = previous.map(str::to_string);
    let mut last = Value::Null;
    for step in &spec.steps {
        let out = run_flow(step, prev_text.as_deref(), state, delegate, ctx).await?;
        prev_text = Some(output_to_text(&out));
        last = out;
    }
    Ok(last)
}

/// Execute a fork: run all branches concurrently under a `Semaphore` cap,
/// collect into `{branches, errors}`, and record the result under the
/// fork id so a sibling join can find it.
async fn run_fork(
    spec: &ForkSpec,
    previous: Option<&str>,
    state: &mut RunState,
    delegate: &dyn Delegate,
    ctx: &ToolContext,
) -> Result<Value, ToolError> {
    let limit = spec.concurrency.unwrap_or(DEFAULT_CONCURRENCY).max(1);
    let sem = Arc::new(Semaphore::new(limit));

    let spawn_limit = state.spawn_limit.clone();
    let futures = spec.branches.iter().map(|(key, branch)| {
        let spawn_limit = spawn_limit.clone();
        let sem = sem.clone();
        async move {
            // Each branch gets its own RunState — cross-branch fork
            // references aren't in scope, so no shared mutation is needed.
            let mut branch_state = RunState {
                forks: HashMap::new(),
                spawn_limit,
            };
            let _permit = sem.acquire().await.expect("semaphore not closed");
            let result = run_flow(branch, previous, &mut branch_state, delegate, ctx).await;
            (key.clone(), result)
        }
    });

    let results = futures::future::join_all(futures).await;

    let mut branches = Map::new();
    let mut errors = Map::new();
    for (key, result) in results {
        match result {
            Ok(value) => {
                branches.insert(key, value);
            }
            Err(e) => {
                errors.insert(key, Value::from(e.to_string()));
            }
        }
    }

    let output = serde_json::json!({ "branches": branches, "errors": errors });
    state.forks.insert(spec.id.clone(), output.clone());
    Ok(output)
}

/// Execute a join over a previously-completed fork's results.
async fn run_join(
    spec: &JoinSpec,
    state: &mut RunState,
    delegate: &dyn Delegate,
    ctx: &ToolContext,
) -> Result<Value, ToolError> {
    let fork_output = state.forks.get(&spec.from).cloned().ok_or_else(|| {
        ToolError::Execution(format!(
            "join references fork \"{}\" which has not completed (must run after the fork in the same sequence)",
            spec.from
        ))
    })?;

    let branches = fork_output
        .get("branches")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let errors = fork_output
        .get("errors")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let successes = branches.len();
    let on_failure = spec.on_failure.unwrap_or(OnFailure::FailFast);

    let satisfied = match spec.mode {
        JoinMode::All => errors.is_empty(),
        JoinMode::Any => successes > 0,
        JoinMode::Quorum => successes >= spec.quorum.unwrap_or(1),
    };

    if !satisfied {
        // collectErrors proceeds with whatever succeeded; failFast aborts.
        if !(on_failure == OnFailure::CollectErrors && successes > 0) {
            return Err(ToolError::Execution(format!(
                "join \"{}\" not satisfied (mode={:?}, {} succeeded, {} failed)",
                spec.from,
                spec.mode,
                successes,
                errors.len()
            )));
        }
    }

    match &spec.reducer {
        None | Some(JoinReducer::Collect) => Ok(fork_output),
        Some(JoinReducer::Agent(reducer)) => {
            run_reducer(reducer, &fork_output, delegate, ctx, &state.spawn_limit).await
        }
    }
}

/// Route collected fork results through a reducer agent.
async fn run_reducer(
    reducer: &AgentReducer,
    collected: &Value,
    delegate: &dyn Delegate,
    ctx: &ToolContext,
    spawn_limit: &Semaphore,
) -> Result<Value, ToolError> {
    let collected_text = serde_json::to_string_pretty(collected).unwrap_or_default();
    // `{previous}` is replaced with the collected JSON; otherwise the JSON
    // is appended as an explicit context block.
    let task = if reducer.task.contains("{previous}") {
        reducer.task.replace("{previous}", &collected_text)
    } else {
        format!(
            "{}\n\nCollected branch results (JSON):\n```json\n{}\n```",
            reducer.task, collected_text
        )
    };
    let _permit = spawn_limit.acquire().await.expect("semaphore not closed");
    let text = delegate.spawn(&reducer.agent, &task, ctx).await?;
    match reducer.output {
        Some(OutputMode::Json) => parse_json_text(&text),
        _ => Ok(Value::String(text)),
    }
}

/// Dispatch a single flow node. Boxed because the walk is recursive.
fn run_flow<'a>(
    flow: &'a FlowSpec,
    previous: Option<&'a str>,
    state: &'a mut RunState,
    delegate: &'a dyn Delegate,
    ctx: &'a ToolContext,
) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + 'a>> {
    Box::pin(async move {
        match flow {
            FlowSpec::Spawn(s) => run_spawn(s, previous, delegate, ctx, &state.spawn_limit).await,
            FlowSpec::Sequence(s) => run_sequence(s, previous, state, delegate, ctx).await,
            FlowSpec::Fork(f) => run_fork(f, previous, state, delegate, ctx).await,
            FlowSpec::Join(j) => run_join(j, state, delegate, ctx).await,
        }
    })
}

/// Public entry point — execute a whole flow and return its root output.
pub(crate) async fn execute_flow(
    flow: &FlowSpec,
    delegate: &dyn Delegate,
    ctx: &ToolContext,
) -> Result<Value, ToolError> {
    let mut state = RunState::default();
    run_flow(flow, None, &mut state, delegate, ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::orchestrator::spec::parse_flow;
    use crate::test_support::{fresh_session, tool_context};
    use crate::tool::ToolRegistry;
    use std::sync::Mutex;

    /// Records every (agent, task) it is asked to run and replies with a
    /// scripted response. Default reply echoes the task.
    struct MockDelegate {
        calls: Mutex<Vec<(String, String)>>,
        replies: HashMap<String, String>,
    }

    impl MockDelegate {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                replies: HashMap::new(),
            }
        }
        fn with_reply(mut self, agent: &str, reply: &str) -> Self {
            self.replies.insert(agent.to_string(), reply.to_string());
            self
        }
        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Delegate for MockDelegate {
        fn spawn<'a>(
            &'a self,
            agent: &'a str,
            task: &'a str,
            _ctx: &'a ToolContext,
        ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
            self.calls
                .lock()
                .unwrap()
                .push((agent.to_string(), task.to_string()));
            let reply = self
                .replies
                .get(agent)
                .cloned()
                .unwrap_or_else(|| format!("done: {task}"));
            Box::pin(async move { Ok(reply) })
        }
    }

    async fn ctx() -> (eidetica::Instance, ToolContext) {
        let (instance, session) = fresh_session().await;
        let c = tool_context(session, Arc::new(ToolRegistry::new()));
        (instance, c)
    }

    #[tokio::test]
    async fn single_delegation_runs_one_spawn() {
        let (_i, ctx) = ctx().await;
        let delegate = MockDelegate::new().with_reply("researcher", "the answer");
        let flow =
            parse_flow(&serde_json::json!({"agent": "researcher", "task": "look it up"})).unwrap();
        let out = execute_flow(&flow, &delegate, &ctx).await.unwrap();
        assert_eq!(out, Value::from("the answer"));
        assert_eq!(
            delegate.calls(),
            vec![("researcher".into(), "look it up".into())]
        );
    }

    #[tokio::test]
    async fn sequence_threads_previous_into_next_task() {
        let (_i, ctx) = ctx().await;
        let delegate = MockDelegate::new()
            .with_reply("a", "first-output")
            .with_reply("b", "second-output");
        let flow = parse_flow(&serde_json::json!({
            "kind": "sequence",
            "steps": [
                { "agent": "a", "task": "step one" },
                { "agent": "b", "task": "use {previous}" }
            ]
        }))
        .unwrap();
        let out = execute_flow(&flow, &delegate, &ctx).await.unwrap();
        assert_eq!(out, Value::from("second-output"));
        let calls = delegate.calls();
        assert_eq!(calls[1], ("b".into(), "use first-output".into()));
    }

    #[tokio::test]
    async fn parallel_tasks_fork_collects_all_branches() {
        let (_i, ctx) = ctx().await;
        let delegate = MockDelegate::new();
        let flow = parse_flow(&serde_json::json!({
            "kind": "fork",
            "id": "tasks",
            "concurrency": 2,
            "branches": {
                "task-0": { "agent": "researcher", "task": "topic a" },
                "task-1": { "agent": "researcher", "task": "topic b" }
            }
        }))
        .unwrap();
        let out = execute_flow(&flow, &delegate, &ctx).await.unwrap();
        assert_eq!(out["branches"]["task-0"], Value::from("done: topic a"));
        assert_eq!(out["branches"]["task-1"], Value::from("done: topic b"));
        assert!(out["errors"].as_object().unwrap().is_empty());
        assert_eq!(delegate.calls().len(), 2);
    }

    #[tokio::test]
    async fn fork_join_with_reducer_consolidates() {
        let (_i, ctx) = ctx().await;
        let delegate = MockDelegate::new()
            .with_reply("researcher", "finding")
            .with_reply("consolidator", "merged report");
        let flow = parse_flow(&serde_json::json!({
            "kind": "sequence",
            "steps": [
                {
                    "kind": "fork",
                    "id": "research",
                    "branches": {
                        "a": { "agent": "researcher", "task": "topic a" },
                        "b": { "agent": "researcher", "task": "topic b" }
                    }
                },
                {
                    "kind": "join",
                    "from": "research",
                    "mode": "all",
                    "reducer": { "kind": "agent", "agent": "consolidator", "task": "merge these" }
                }
            ]
        }))
        .unwrap();
        let out = execute_flow(&flow, &delegate, &ctx).await.unwrap();
        assert_eq!(out, Value::from("merged report"));
        // The reducer was handed the collected branch JSON.
        let calls = delegate.calls();
        let (agent, task) = calls.last().unwrap();
        assert_eq!(agent, "consolidator");
        assert!(
            task.contains("finding"),
            "reducer task should carry branch results: {task}"
        );
    }

    #[tokio::test]
    async fn join_collect_returns_branch_structure() {
        let (_i, ctx) = ctx().await;
        let delegate = MockDelegate::new();
        let flow = parse_flow(&serde_json::json!({
            "kind": "sequence",
            "steps": [
                { "kind": "fork", "id": "f", "branches": { "a": "researcher", "b": "scout" } },
                { "kind": "join", "from": "f", "mode": "all" }
            ]
        }))
        .unwrap();
        let out = execute_flow(&flow, &delegate, &ctx).await.unwrap();
        assert!(out.get("branches").is_some());
        assert_eq!(out["branches"]["a"], Value::from("done: a"));
    }

    #[tokio::test]
    async fn json_output_mode_parses_reply() {
        let (_i, ctx) = ctx().await;
        let delegate = MockDelegate::new().with_reply("researcher", "```json\n{\"score\": 7}\n```");
        let flow = parse_flow(&serde_json::json!({
            "agent": "researcher", "task": "rate it", "output": "json"
        }))
        .unwrap();
        let out = execute_flow(&flow, &delegate, &ctx).await.unwrap();
        assert_eq!(out["score"], Value::from(7));
    }

    #[tokio::test]
    async fn json_output_mode_rejects_non_json() {
        let (_i, ctx) = ctx().await;
        let delegate = MockDelegate::new().with_reply("researcher", "not json at all");
        let flow = parse_flow(&serde_json::json!({
            "agent": "researcher", "task": "rate it", "output": "json"
        }))
        .unwrap();
        let err = execute_flow(&flow, &delegate, &ctx).await.unwrap_err();
        assert!(err.to_string().contains("JSON"), "got: {err}");
    }

    #[tokio::test]
    async fn disposable_fanout_caps_identity_and_collects_partial_failure() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct LiveDelegate {
            active: AtomicUsize,
            peak: AtomicUsize,
            calls: Mutex<Vec<(String, String)>>,
        }
        impl Delegate for LiveDelegate {
            fn spawn<'a>(
                &'a self,
                agent: &'a str,
                task: &'a str,
                _ctx: &'a ToolContext,
            ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
                Box::pin(async move {
                    self.calls.lock().unwrap().push((agent.into(), task.into()));
                    let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                    self.peak.fetch_max(active, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    self.active.fetch_sub(1, Ordering::SeqCst);
                    if agent == "broken" {
                        return Err(ToolError::Execution("child failed".into()));
                    }
                    Ok(format!("{agent}: {task}"))
                })
            }
        }
        let (_i, ctx) = ctx().await;
        let delegate = LiveDelegate {
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            calls: Mutex::new(Vec::new()),
        };
        let flow = parse_flow(&serde_json::json!({
            "kind":"sequence", "steps":[
                {"kind":"fork", "id":"research", "concurrency":2, "branches":{
                    "one":{"agent":"alice","task":"alpha"},
                    "two":{"agent":"bob","task":"beta"},
                    "three":{"agent":"broken","task":"gamma"},
                    "four":{"agent":"alice","task":"delta"}
                }},
                {"kind":"join","from":"research","mode":"any","onFailure":"collectErrors"}
            ]
        }))
        .unwrap();
        let out = execute_flow(&flow, &delegate, &ctx).await.unwrap();
        assert_eq!(out["branches"]["one"], "alice: alpha");
        assert_eq!(out["branches"]["two"], "bob: beta");
        assert_eq!(out["branches"]["four"], "alice: delta");
        assert!(
            out["errors"]["three"]
                .as_str()
                .unwrap()
                .contains("child failed")
        );
        assert_eq!(delegate.calls.lock().unwrap().len(), 4);
        assert_eq!(delegate.peak.load(Ordering::SeqCst), 2);
        assert_eq!(delegate.active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn nested_forks_share_a_global_spawn_ceiling() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountingDelegate {
            active: AtomicUsize,
            peak: AtomicUsize,
        }
        impl Delegate for CountingDelegate {
            fn spawn<'a>(
                &'a self,
                _agent: &'a str,
                task: &'a str,
                _ctx: &'a ToolContext,
            ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
                Box::pin(async move {
                    let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                    self.peak.fetch_max(n, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                    self.active.fetch_sub(1, Ordering::SeqCst);
                    Ok(task.into())
                })
            }
        }
        let mut outer = Map::new();
        for i in 0..4 {
            let mut inner = Map::new();
            for j in 0..8 {
                inner.insert(
                    format!("child-{j}"),
                    serde_json::json!({"agent":"researcher","task":format!("{i}-{j}")}),
                );
            }
            outer.insert(format!("group-{i}"), serde_json::json!({"kind":"fork","id":format!("inner-{i}"),"concurrency":8,"branches":inner}));
        }
        let flow = parse_flow(
            &serde_json::json!({"kind":"fork","id":"outer","concurrency":4,"branches":outer}),
        )
        .unwrap();
        let (_i, ctx) = ctx().await;
        let delegate = CountingDelegate {
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        };
        let out = execute_flow(&flow, &delegate, &ctx).await.unwrap();
        assert_eq!(out["branches"].as_object().unwrap().len(), 4);
        assert_eq!(
            out["branches"]["group-0"]["branches"]
                .as_object()
                .unwrap()
                .len(),
            8
        );
        assert_eq!(delegate.peak.load(Ordering::SeqCst), 16);
        assert_eq!(delegate.active.load(Ordering::SeqCst), 0);
    }
}
