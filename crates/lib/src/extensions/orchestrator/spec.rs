//! Flow-spec types, normalization, and validation for the orchestrator.
//!
//! The shapes here mirror pi-orchestrator's `FlowSpec` for supported inline
//! graph forms only; this tool does not support loops or cwd. The
//! canonical (normalized) form is what serde serializes; the
//! [`normalize_value`] pass accepts pi's compact authoring sugar (bare
//! strings, omitted `kind`, fork `taskTemplate` / `{branch}`) and lowers
//! it to that canonical form before typed parsing.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Per-spawn output typing. `Text` (default) returns raw agent text;
/// `Json` parses the agent's reply as JSON and fails the node if it
/// isn't valid JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputMode {
    Text,
    Json,
}

/// A node in the workflow graph. Internally tagged on `kind`, matching
/// pi's discriminated union.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FlowSpec {
    Spawn(SpawnSpec),
    Sequence(SequenceSpec),
    Fork(ForkSpec),
    Join(JoinSpec),
}

impl FlowSpec {
    pub fn kind_str(&self) -> &'static str {
        match self {
            FlowSpec::Spawn(_) => "spawn",
            FlowSpec::Sequence(_) => "sequence",
            FlowSpec::Fork(_) => "fork",
            FlowSpec::Join(_) => "join",
        }
    }

    pub fn id(&self) -> Option<&str> {
        match self {
            FlowSpec::Spawn(s) => s.id.as_deref(),
            FlowSpec::Sequence(s) => s.id.as_deref(),
            FlowSpec::Fork(s) => Some(&s.id),
            FlowSpec::Join(s) => s.id.as_deref(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SpawnSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub agent: String,
    pub task: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputMode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SequenceSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub steps: Vec<FlowSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ForkSpec {
    /// Required — the matching join references this id.
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// `BTreeMap` so branch iteration / serialization is deterministic
    /// (pi sorts branch keys for the same reason).
    pub branches: BTreeMap<String, FlowSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JoinMode {
    All,
    Any,
    Quorum,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OnFailure {
    FailFast,
    CollectErrors,
}

/// How a join folds the fork's branch results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum JoinReducer {
    /// Return the `{branches, errors}` structure verbatim.
    Collect,
    /// Route the collected results through one more agent.
    Agent(AgentReducer),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentReducer {
    pub agent: String,
    pub task: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputMode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JoinSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Id of the fork node whose results to collect.
    pub from: String,
    pub mode: JoinMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quorum: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reducer: Option<JoinReducer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_failure: Option<OnFailure>,
}

// ---------------------------------------------------------------------------
// Normalization — compact authoring forms → canonical FlowSpec Value
// ---------------------------------------------------------------------------

/// Defaults a fork pushes down into its branches (supported subset).
#[derive(Default, Clone)]
struct SpawnDefaults {
    agent: Option<String>,
    task_template: Option<String>,
    output: Option<Value>,
    branch_key: Option<String>,
}

fn str_field(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

fn resolve_branch_task(defaults: &SpawnDefaults) -> Option<String> {
    let key = defaults.branch_key.clone().unwrap_or_default();
    match &defaults.task_template {
        Some(t) => Some(t.replace("{branch}", &key)),
        None => defaults.branch_key.clone(),
    }
}

fn normalize_spawn(
    obj: &Map<String, Value>,
    label: &str,
    defaults: &SpawnDefaults,
) -> Result<Value, String> {
    let agent = str_field(obj, "agent").or_else(|| defaults.agent.clone());
    let agent = match agent {
        Some(a) if !a.trim().is_empty() => a,
        _ => {
            return Err(format!(
                "{label}.agent must be a non-empty string or inherit one from the parent fork."
            ));
        }
    };
    let task = str_field(obj, "task").or_else(|| resolve_branch_task(defaults));
    let task = match task {
        Some(t) if !t.trim().is_empty() => t,
        _ => {
            return Err(format!(
                "{label}.task must be a non-empty string or inherit one from fork.taskTemplate."
            ));
        }
    };

    let mut out = Map::new();
    out.insert("kind".into(), Value::from("spawn"));
    if let Some(id) = str_field(obj, "id") {
        out.insert("id".into(), Value::from(id));
    }
    if let Some(lbl) = str_field(obj, "label") {
        out.insert("label".into(), Value::from(lbl));
    }
    out.insert("agent".into(), Value::from(agent));
    out.insert("task".into(), Value::from(task));
    if obj.contains_key("cwd") {
        return Err(format!(
            "{label}.cwd is unsupported: agent sessions run on this Peer."
        ));
    }
    if let Some(output) = obj
        .get("output")
        .cloned()
        .or_else(|| defaults.output.clone())
    {
        out.insert("output".into(), output);
    }
    Ok(Value::Object(out))
}

/// Lower a raw (LLM-authored) flow value to canonical form. Accepts:
/// bare strings (→ spawn on that agent), objects with no `kind` (→ spawn),
/// and fork branch sugar (`taskTemplate`, `{branch}`, inherited agent).
fn normalize_value(raw: &Value, defaults: &SpawnDefaults) -> Result<Value, String> {
    normalize_value_inner(raw, "flow", defaults)
}

fn normalize_value_inner(
    raw: &Value,
    label: &str,
    defaults: &SpawnDefaults,
) -> Result<Value, String> {
    // Bare string → spawn on that agent.
    if let Some(s) = raw.as_str() {
        let mut obj = Map::new();
        obj.insert("agent".into(), Value::from(s));
        return normalize_spawn(&obj, label, defaults);
    }
    let obj = raw
        .as_object()
        .ok_or_else(|| format!("{label} must be an object or agent-name string."))?;

    let kind = obj.get("kind").and_then(|v| v.as_str());
    let allowed: &[&str] = match kind {
        None | Some("spawn") => &["kind", "id", "label", "agent", "task", "output"],
        Some("sequence") => &["kind", "id", "label", "steps"],
        Some("fork") => &[
            "kind",
            "id",
            "label",
            "branches",
            "agent",
            "taskTemplate",
            "output",
            "concurrency",
        ],
        Some("join") => &[
            "kind",
            "id",
            "label",
            "from",
            "mode",
            "quorum",
            "reducer",
            "onFailure",
        ],
        _ => &[],
    };
    if let Some(unknown) = obj.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("{label} has unsupported field {unknown:?}."));
    }
    match kind {
        None | Some("spawn") => normalize_spawn(obj, label, defaults),
        Some("sequence") => {
            let steps = obj
                .get("steps")
                .and_then(|v| v.as_array())
                .ok_or_else(|| format!("{label}.steps must be an array."))?;
            if steps.is_empty() {
                return Err(format!("{label}.steps must not be empty."));
            }
            let mut out = Map::new();
            out.insert("kind".into(), Value::from("sequence"));
            if let Some(id) = str_field(obj, "id") {
                out.insert("id".into(), Value::from(id));
            }
            if let Some(lbl) = str_field(obj, "label") {
                out.insert("label".into(), Value::from(lbl));
            }
            let mut steps_out = Vec::with_capacity(steps.len());
            for (i, step) in steps.iter().enumerate() {
                steps_out.push(normalize_value_inner(
                    step,
                    &format!("{label}.steps[{i}]"),
                    &SpawnDefaults::default(),
                )?);
            }
            out.insert("steps".into(), Value::Array(steps_out));
            Ok(Value::Object(out))
        }
        Some("fork") => {
            let branches = obj
                .get("branches")
                .and_then(|v| v.as_object())
                .filter(|m| !m.is_empty())
                .ok_or_else(|| format!("{label}.branches must be a non-empty object."))?;
            let bd = SpawnDefaults {
                agent: str_field(obj, "agent"),
                task_template: str_field(obj, "taskTemplate"),
                output: obj.get("output").cloned(),
                branch_key: None,
            };
            let mut branches_out = Map::new();
            for (k, v) in branches {
                let mut d = bd.clone();
                d.branch_key = Some(k.clone());
                branches_out.insert(
                    k.clone(),
                    normalize_value_inner(v, &format!("{label}.branches.{k}"), &d)?,
                );
            }
            let mut out = Map::new();
            out.insert("kind".into(), Value::from("fork"));
            out.insert(
                "id".into(),
                Value::from(str_field(obj, "id").unwrap_or_else(|| "fork".into())),
            );
            if let Some(lbl) = str_field(obj, "label") {
                out.insert("label".into(), Value::from(lbl));
            }
            if let Some(c) = obj.get("concurrency") {
                let n = c.as_u64().ok_or_else(|| {
                    format!("{label}.concurrency must be an integer between 1 and 16.")
                })?;
                if !(1..=16).contains(&n) {
                    return Err(format!(
                        "{label}.concurrency must be an integer between 1 and 16."
                    ));
                }
                out.insert("concurrency".into(), Value::from(n));
            }
            out.insert("branches".into(), Value::Object(branches_out));
            Ok(Value::Object(out))
        }
        Some("join") => {
            // Join carries no spawn sugar — pass the recognized fields through.
            let mut out = Map::new();
            out.insert("kind".into(), Value::from("join"));
            if let Some(id) = str_field(obj, "id") {
                out.insert("id".into(), Value::from(id));
            }
            if let Some(lbl) = str_field(obj, "label") {
                out.insert("label".into(), Value::from(lbl));
            }
            for key in ["from", "mode", "quorum", "reducer", "onFailure"] {
                if let Some(v) = obj.get(key) {
                    out.insert(key.into(), v.clone());
                }
            }
            Ok(Value::Object(out))
        }
        Some("loop") => Err(format!("{label}.kind loop is unsupported.")),
        Some(other) => Err(format!(
            "{label}.kind must be one of spawn, sequence, fork, join (got {other:?})."
        )),
    }
}

/// Normalize a raw flow value and parse it into a typed [`FlowSpec`],
/// then check fork/join reference integrity.
pub fn parse_flow(raw: &Value) -> Result<FlowSpec, String> {
    let canonical = normalize_value(raw, &SpawnDefaults::default())?;
    let flow: FlowSpec =
        serde_json::from_value(canonical).map_err(|e| format!("invalid flow spec: {e}"))?;
    validate_references(&flow)?;
    Ok(flow)
}

// ---------------------------------------------------------------------------
// Reference integrity — fork/join pairing (supported subset)
// ---------------------------------------------------------------------------

fn collect_ids(
    spec: &FlowSpec,
    label: &str,
    ids: &mut BTreeMap<String, (&'static str, usize)>,
) -> Result<(), String> {
    if let Some(id) = spec.id()
        && let Some(prev) = ids.insert(
            id.to_string(),
            (
                spec.kind_str(),
                match spec {
                    FlowSpec::Fork(f) => f.branches.len(),
                    _ => 0,
                },
            ),
        )
    {
        return Err(format!(
            "{label}.id duplicates \"{id}\", already used by a {} node.",
            prev.0
        ));
    }
    match spec {
        FlowSpec::Sequence(s) => {
            for (i, step) in s.steps.iter().enumerate() {
                collect_ids(step, &format!("{label}.steps[{i}]"), ids)?;
            }
        }
        FlowSpec::Fork(f) => {
            for (k, b) in &f.branches {
                collect_ids(b, &format!("{label}.branches.{k}"), ids)?;
            }
        }
        FlowSpec::Spawn(_) | FlowSpec::Join(_) => {}
    }
    Ok(())
}

fn visit_refs(
    spec: &FlowSpec,
    label: &str,
    visible_forks: &[String],
    ids: &BTreeMap<String, (&'static str, usize)>,
    joined: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    match spec {
        FlowSpec::Spawn(_) => {}
        FlowSpec::Sequence(s) => {
            let mut local = visible_forks.to_vec();
            for (i, step) in s.steps.iter().enumerate() {
                visit_refs(step, &format!("{label}.steps[{i}]"), &local, ids, joined)?;
                if let FlowSpec::Fork(f) = step {
                    local.push(f.id.clone());
                }
            }
        }
        FlowSpec::Fork(f) => {
            for (k, b) in &f.branches {
                let scoped = Vec::new();
                visit_refs(b, &format!("{label}.branches.{k}"), &scoped, ids, joined)?;
            }
        }
        FlowSpec::Join(j) => {
            let target = ids
                .get(&j.from)
                .ok_or_else(|| format!("{label}.from references unknown fork \"{}\".", j.from))?;
            if target.0 != "fork" {
                let kind = target.0;
                return Err(format!(
                    "{label}.from must reference a fork node, but \"{}\" is a {kind}.",
                    j.from
                ));
            }
            if !visible_forks.iter().any(|f| f == &j.from) {
                return Err(format!(
                    "{label}.from: fork \"{}\" is not in scope at this point.",
                    j.from
                ));
            }
            if let Some(prev) = joined.insert(j.from.clone(), format!("{label}.from")) {
                return Err(format!(
                    "{label}.from references fork \"{}\", already joined at {prev}.",
                    j.from
                ));
            }
            if j.mode == JoinMode::Quorum && j.quorum.is_none() {
                return Err(format!("{label}.quorum is required when mode=\"quorum\"."));
            }
            if let Some(quorum) = j.quorum
                && (j.mode != JoinMode::Quorum || quorum == 0 || quorum > target.1)
            {
                return Err(format!(
                    "{label}.quorum must be between 1 and the fork branch count, and requires mode=quorum."
                ));
            }
        }
    }
    Ok(())
}

/// Validate fork/join id references: every join points at a fork that is
/// declared, in scope, and not already joined.
pub fn validate_references(flow: &FlowSpec) -> Result<(), String> {
    let mut ids = BTreeMap::new();
    collect_ids(flow, "flow", &mut ids)?;
    let mut joined = BTreeMap::new();
    let visible = Vec::new();
    visit_refs(flow, "flow", &visible, &ids, &mut joined)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_bare_string_has_no_task_and_errors() {
        // A bare string only carries an agent; with no task (and no fork
        // branch key to fall back to) it's invalid — pi parity.
        let err = parse_flow(&Value::from("researcher")).unwrap_err();
        assert!(err.contains("task must be"), "got: {err}");
    }

    #[test]
    fn bare_string_branch_uses_branch_key_as_task() {
        let raw = serde_json::json!({
            "kind": "fork",
            "id": "f",
            "branches": { "alpha": "researcher", "beta": "scout" }
        });
        let flow = parse_flow(&raw).unwrap();
        let FlowSpec::Fork(f) = flow else {
            panic!("expected fork")
        };
        let FlowSpec::Spawn(alpha) = &f.branches["alpha"] else {
            panic!()
        };
        assert_eq!(alpha.agent, "researcher");
        assert_eq!(alpha.task, "alpha");
    }

    #[test]
    fn object_without_kind_is_spawn() {
        let raw = serde_json::json!({"agent": "scout", "task": "find it"});
        let flow = parse_flow(&raw).unwrap();
        assert!(matches!(flow, FlowSpec::Spawn(_)));
    }

    #[test]
    fn fork_pushes_task_template_into_branches() {
        let raw = serde_json::json!({
            "kind": "fork",
            "id": "f1",
            "agent": "researcher",
            "taskTemplate": "Research {branch}",
            "branches": { "alpha": {}, "beta": {} }
        });
        let flow = parse_flow(&raw).unwrap();
        let FlowSpec::Fork(f) = flow else {
            panic!("expected fork")
        };
        let FlowSpec::Spawn(alpha) = &f.branches["alpha"] else {
            panic!()
        };
        assert_eq!(alpha.agent, "researcher");
        assert_eq!(alpha.task, "Research alpha");
    }

    #[test]
    fn fork_without_join_ref_ok_join_with_unknown_fork_errors() {
        let raw = serde_json::json!({
            "kind": "sequence",
            "steps": [
                { "kind": "join", "from": "nope", "mode": "all" }
            ]
        });
        let err = parse_flow(&raw).unwrap_err();
        assert!(err.contains("unknown fork"), "got: {err}");
    }

    #[test]
    fn quorum_requires_quorum_field() {
        let raw = serde_json::json!({
            "kind": "sequence",
            "steps": [
                { "kind": "fork", "id": "f", "branches": { "a": "x", "b": "y" } },
                { "kind": "join", "from": "f", "mode": "quorum" }
            ]
        });
        let err = parse_flow(&raw).unwrap_err();
        assert!(err.contains("quorum is required"), "got: {err}");
    }

    #[test]
    fn canonical_flow_round_trips_through_serde() {
        // A canonical pi flow file (already normalized). Deserialize then
        // serialize and assert structural equality — the byte-compat check.
        let canonical = serde_json::json!({
            "kind": "sequence",
            "steps": [
                {
                    "kind": "fork",
                    "id": "research",
                    "branches": {
                        "a": { "kind": "spawn", "agent": "researcher", "task": "topic a", "output": "json" },
                        "b": { "kind": "spawn", "agent": "researcher", "task": "topic b" }
                    }
                },
                {
                    "kind": "join",
                    "from": "research",
                    "mode": "all",
                    "reducer": { "kind": "agent", "agent": "consolidator", "task": "merge" }
                }
            ]
        });
        let flow: FlowSpec = serde_json::from_value(canonical.clone()).unwrap();
        let back = serde_json::to_value(&flow).unwrap();
        assert_eq!(
            canonical, back,
            "flow spec must round-trip byte-identically"
        );
    }
}

#[cfg(test)]
mod admission_scope_tests {
    use super::*;

    #[test]
    fn rejects_scope_hidden_in_late_node_or_reducer() {
        assert!(
            serde_json::from_value::<FlowSpec>(serde_json::json!({
                "kind":"spawn", "agent":"a", "task":"do it", "workspace":"/tmp"
            }))
            .is_err()
        );
        for graph in [
            serde_json::json!({"kind":"sequence","steps":[
                {"agent":"a","task":"early"},
                {"agent":"b","task":"later","workspace":"/tmp"}
            ]}),
            serde_json::json!({"kind":"sequence","steps":[
                {"kind":"fork","id":"f","branches":{"a":{"agent":"a","task":"early"}}},
                {"kind":"join","from":"f","mode":"all","reducer":{
                    "kind":"agent","agent":"a","task":"reduce","scope":"private"}}
            ]}),
        ] {
            assert!(parse_flow(&graph).is_err(), "unexpectedly accepted {graph}");
        }
    }
}
