//! Orchestrator extension — the `workflow` tool.
//!
//! Gives an agent compositional delegation over the Peer's other agents:
//! single delegation, parallel fan-out with a concurrency cap, sequential
//! pipelines that thread each step's output into the next, and fork+join
//! with an optional reducer agent. The tool schema mirrors
//! pi-orchestrator's `workflow` so supported inline flow definitions are portable between
//! the two (see [`spec`]).
//!
//! Durable `spawn_agent` returns a session DB handle, not a child reply.
//! Until the parent coordinator exists, the tool refuses execution rather
//! than returning handles as if fork/join nodes had completed.

#[cfg(test)]
mod executor;
mod spec;

use crate::backends::BackendManager;
use crate::extension::instance::{ExtensionInstance, InstantiateFuture, ScopeCtx};
use crate::extension::manifest::ExtensionManifest;
use crate::extension::{Extension, ExtensionRef, HookKind};
use crate::instance::ServerSlot;
use crate::security::SecurityContext;
use crate::tool::{
    ApprovalRequirement, RiskLevel, Tool, ToolContext, ToolDescriptor, ToolError, ToolPolicy,
};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// The `workflow` tool.
pub struct WorkflowTool;

impl WorkflowTool {
    /// Build the canonical flow value from the tool's top-level params,
    /// resolving the three compact entry forms (single `{agent, task}`,
    /// parallel `{tasks: [...]}`, and explicit `{flow}`) into one flow.
    fn resolve_flow(arguments: &Value) -> Result<Value, String> {
        if arguments.get("concurrency").is_some() && arguments.get("tasks").is_none() {
            return Err("'concurrency' requires 'tasks'.".into());
        }
        if arguments.get("output").is_some() && arguments.get("agent").is_none() {
            return Err("'output' requires single-agent mode.".into());
        }
        // Parallel tasks → auto-wrapped fork (pi parity).
        let modes = ["tasks", "flow", "agent", "task"]
            .iter()
            .filter(|k| arguments.get(**k).is_some())
            .count();
        if arguments.get("tasks").is_some() && (modes != 1 || !arguments["tasks"].is_array()) {
            return Err(
                "'tasks' must be an array and cannot be combined with another mode.".into(),
            );
        }
        if arguments.get("flow").is_some() && modes != 1 {
            return Err("'flow' cannot be combined with another mode.".into());
        }
        if let Some(tasks) = arguments.get("tasks").and_then(|v| v.as_array()) {
            if tasks.is_empty() {
                return Err("'tasks' was empty — provide at least one {agent, task}.".into());
            }
            let mut branches = serde_json::Map::new();
            for (i, t) in tasks.iter().enumerate() {
                let agent = t.get("agent").and_then(|v| v.as_str());
                let task = t.get("task").and_then(|v| v.as_str());
                let (Some(agent), Some(task)) = (agent, task) else {
                    return Err(format!("tasks[{i}] needs both 'agent' and 'task'."));
                };
                let mut spawn = serde_json::Map::new();
                spawn.insert("kind".into(), Value::from("spawn"));
                spawn.insert("agent".into(), Value::from(agent));
                spawn.insert("task".into(), Value::from(task));
                if agent.is_empty() || task.is_empty() {
                    return Err(format!("tasks[{i}] needs non-empty agent and task."));
                }
                if let Some(output) = t.get("output") {
                    spawn.insert("output".into(), output.clone());
                }
                branches.insert(format!("task-{i}"), Value::Object(spawn));
            }
            let mut fork = serde_json::Map::new();
            fork.insert("kind".into(), Value::from("fork"));
            fork.insert("id".into(), Value::from("tasks"));
            if let Some(c) = arguments.get("concurrency") {
                fork.insert("concurrency".into(), c.clone());
            }
            fork.insert("branches".into(), Value::Object(branches));
            return Ok(Value::Object(fork));
        }

        // Explicit graph mode.
        if let Some(flow) = arguments.get("flow") {
            if flow.is_string() {
                return Err(
                    "named flows (flow: \"<name>\") are a Phase 2 feature; pass an inline flow object."
                        .into(),
                );
            }
            return Ok(flow.clone());
        }

        // Single-agent mode.
        let agent = arguments.get("agent").and_then(|v| v.as_str());
        let task = arguments.get("task").and_then(|v| v.as_str());
        match (agent, task) {
            (Some(agent), Some(task)) => {
                let mut spawn = serde_json::Map::new();
                spawn.insert("kind".into(), Value::from("spawn"));
                spawn.insert("agent".into(), Value::from(agent));
                spawn.insert("task".into(), Value::from(task));
                if let Some(output) = arguments.get("output") {
                    spawn.insert("output".into(), output.clone());
                }
                Ok(Value::Object(spawn))
            }
            _ => Err("Either 'flow', 'tasks', or both 'agent' and 'task' are required.".into()),
        }
    }
}

impl Tool for WorkflowTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "workflow".to_string(),
            description: "Unavailable until a durable workflow parent coordinator is installed. Modes: \
                single ({agent, task}); parallel ({tasks: [{agent, task}, ...], concurrency: N}); \
                or an inline flow graph ({flow: {kind: \"sequence\"|\"fork\"|\"join\", ...}}). A \
                sequence threads each step's output into the next via the {previous} placeholder; \
                a fork fans out its branches concurrently (Semaphore-capped) and collects them into \
                {branches, errors}; a join folds a fork's results, optionally through a reducer \
                agent. Per-spawn output: \"text\" (default) or \"json\" (reply parsed + validated). \
                Supports pi-orchestrator-style inline flows, excluding loops and cwd."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "description": "Single mode: the agent (display name or DB ID) to delegate to. Pair with 'task'."
                    },
                    "task": {
                        "type": "string",
                        "description": "Single mode: what the agent should accomplish."
                    },
                    "tasks": {
                        "type": "array",
                        "description": "Parallel mode: tasks run concurrently, auto-wrapped into a fork.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "agent": { "type": "string", "description": "Agent name." },
                                "task": { "type": "string", "description": "Task description." },
                                "output": { "type": "string", "enum": ["text", "json"], "description": "Output typing." }
                            },
                            "required": ["agent", "task"]
                        }
                    },
                    "flow": {
                        "type": "object",
                        "description": "Graph mode: an inline flow spec. Node kinds: spawn {agent, task, output?}; sequence {steps:[...]}; fork {id, branches:{...}, concurrency?}; join {from, mode:\"all\"|\"any\"|\"quorum\", quorum?, reducer?, onFailure?}."
                    },
                    "concurrency": {
                        "type": "integer",
                        "description": "Max concurrent branches for parallel/fork mode (default 4)."
                    },
                    "output": {
                        "type": "string",
                        "enum": ["text", "json"],
                        "description": "Single mode: output typing for the one spawn. 'json' validates the reply as JSON."
                    }
                }
            }),
        }
    }

    fn default_policy(&self) -> ToolPolicy {
        // Same envelope as spawn_agent — delegation is medium-risk and the
        // fan-out can run long, so allow a generous timeout.
        ToolPolicy {
            risk: RiskLevel::Medium,
            approval: ApprovalRequirement::UnlessAutoApproved,
            timeout: 600,
            ..ToolPolicy::default()
        }
    }

    fn execute<'a>(
        &'a self,
        arguments: Value,
        _ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<String, ToolError>> + Send + 'a>> {
        Box::pin(async move {
            let raw_flow = Self::resolve_flow(&arguments).map_err(ToolError::InvalidArgument)?;
            let _flow = spec::parse_flow(&raw_flow).map_err(ToolError::InvalidArgument)?;
            // A durable spawn returns a handle, not a reply. Never feed that
            // handle into the old synchronous graph or submit partial fan-out.
            Err(ToolError::Execution(
                "workflow is unavailable until durable parent coordination is installed".into(),
            ))
        })
    }
}

/// The orchestrator extension. The constructor retains the existing wiring
/// while the durable parent replaces synchronous delegation.
pub struct OrchestratorExtension;

impl OrchestratorExtension {
    pub fn new(
        _spawn_server_cell: ServerSlot,
        _backend: BackendManager,
        _security: SecurityContext,
    ) -> Self {
        Self
    }
}

impl Extension for OrchestratorExtension {
    fn name(&self) -> &'static str {
        "orchestrator"
    }

    fn supported_hooks(&self) -> &[HookKind] {
        &[HookKind::Tool]
    }

    fn manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: self.name().to_string(),
            extension_ref: ExtensionRef::builtin(self.name()),
            supported_hooks: vec![HookKind::Tool],
            required_capabilities: Vec::new(),
            requested_capabilities: Vec::new(),
            provides_capabilities: Vec::new(),
        }
    }

    fn instantiate<'a>(&'a self, _scope_ctx: ScopeCtx<'a>) -> InstantiateFuture<'a> {
        let manifest = self.manifest();
        Box::pin(async move {
            Ok(Arc::new(OrchestratorInstance { manifest }) as Arc<dyn ExtensionInstance>)
        })
    }
}

struct OrchestratorInstance {
    manifest: ExtensionManifest,
}

impl ExtensionInstance for OrchestratorInstance {
    fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(WorkflowTool)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_flow_single_mode() {
        let args = serde_json::json!({ "agent": "researcher", "task": "look it up" });
        let flow = WorkflowTool::resolve_flow(&args).unwrap();
        assert_eq!(flow["kind"], "spawn");
        assert_eq!(flow["agent"], "researcher");
        assert_eq!(flow["task"], "look it up");
    }

    #[test]
    fn resolve_flow_parallel_tasks_wrap_into_fork() {
        let args = serde_json::json!({
            "tasks": [
                { "agent": "researcher", "task": "a" },
                { "agent": "scout", "task": "b", "output": "json" }
            ],
            "concurrency": 3
        });
        let flow = WorkflowTool::resolve_flow(&args).unwrap();
        assert_eq!(flow["kind"], "fork");
        assert_eq!(flow["concurrency"], 3);
        assert_eq!(flow["branches"]["task-0"]["agent"], "researcher");
        assert_eq!(flow["branches"]["task-1"]["output"], "json");
    }

    #[test]
    fn resolve_flow_named_flow_rejected() {
        let args = serde_json::json!({ "flow": "research-pipeline" });
        let err = WorkflowTool::resolve_flow(&args).unwrap_err();
        assert!(err.contains("Phase 2"), "got: {err}");
    }

    #[test]
    fn resolve_flow_missing_args_errors() {
        let args = serde_json::json!({ "agent": "researcher" });
        assert!(WorkflowTool::resolve_flow(&args).is_err());
    }

    #[test]
    fn descriptor_is_named_workflow_with_object_params() {
        let tool = WorkflowTool;
        let d = tool.descriptor();
        assert_eq!(d.name, "workflow");
        assert_eq!(d.parameters["type"], "object");
        assert!(matches!(tool.default_policy().risk, RiskLevel::Medium));
    }

    #[tokio::test]
    async fn valid_flow_cannot_run_the_legacy_synchronous_executor() {
        let (_instance, session) = crate::test_support::fresh_session().await;
        let ctx =
            crate::test_support::tool_context(session, Arc::new(crate::tool::ToolRegistry::new()));
        let tool = WorkflowTool;
        let error = tool
            .execute(
                serde_json::json!({ "flow": {
                    "kind": "sequence", "steps": [
                        {"kind":"fork", "id":"f", "branches": {
                            "a":{"agent":"a", "task":"one"},
                            "b":{"agent":"b", "task":"two"}
                        }},
                        {"kind":"join", "from":"f", "mode":"all"}
                    ]
                }}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("durable parent"));
    }

    #[tokio::test]
    async fn malformed_late_step_never_spawns() {
        let (_instance, session) = crate::test_support::fresh_session().await;
        let ctx =
            crate::test_support::tool_context(session, Arc::new(crate::tool::ToolRegistry::new()));
        let tool = WorkflowTool;
        for bad in [
            serde_json::json!({"flow":{"kind":"sequence","steps":[{"agent":"a","task":"first"},{"kind":"loop","id":"l","body":{"agent":"a","task":"later"},"maxIterations":2}]}}),
            serde_json::json!({"flow":{"kind":"sequence","steps":[{"agent":"a","task":"first"},{"kind":"join","from":"missing","mode":"all"}]}}),
            serde_json::json!({"tasks":[{"agent":"a","task":"first"},{"agent":"a","task":"later"}],"concurrency":0}),
            serde_json::json!({"flow":{"kind":"sequence","steps":[{"agent":"a","task":"first"},{"agent":"b","task":"later","cwd":"/tmp"}]}}),
        ] {
            assert!(tool.execute(bad, &ctx).await.is_err());
        }
    }
}
