# Agent State Admin Capability

**Status:** Implemented (2026-05-18).
**Depends on:** cap traits landed (`src/extension/caps.rs`), hub wiring (Steps 2–5 of cap refactor), `AgentDbAccess` trait (landed in `src/tools/heartbeat.rs`).

## Security posture

> **This capability system is a guardrail, not a sandbox.** It is designed to stop a poorly behaving agent or tool from doing accidental damage — deleting the wrong timer, scheduling noise into another agent's DB, writing to a path outside `~/code/`. It is **not** designed to contain an LLM or tool that is explicitly, adversarially trying to escape. If we can achieve the latter, that's great, but it is not the requirement driving this design.

The distinction matters:

| What the guardrail stops                                                   | What it doesn't try to stop                                                                |
| -------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| A too-eager agent scheduling timers on every hosted agent                  | A tool that discovers it has `Arc<SessionRegistry>` and walks the object graph to escalate |
| An extension registering tools it didn't declare                           | A WASM extension that exploits a VM escape                                                 |
| A shell tool that `rm -rf ~/` because an agent hallucinated a cleanup step | An agent that builds and runs native code through the shell tool it was granted            |
| A file-write tool scribbling in `/etc`                                     | A tool using `ptrace` or `/proc` to read another process's memory                          |

The ceiling (extension capabilities) and floor (tool policy) together create a **defense-in-depth** model where each layer catches different classes of mistakes. Neither layer alone is a security boundary — together they cover the failure modes that actually show up in practice.

## Problem

Heartbeat tools (`heartbeat_add`, `heartbeat_modify`, `heartbeat_remove`, `heartbeat_list`, `wake_me_up`) read and write agent-owned timers in the target agent's eidetica DB. Today they receive an `Arc<dyn AgentDbAccess>` handle at construction time — an untyped, unscoped, undeclared capability:

1. **Not in the cap system.** `CapabilityKind` has no variant for "access agent state." The trait exists in `tools/heartbeat.rs`, invisible to manifests, extensions, or the hub.
2. **No attenuation.** The handle opens _any_ hosted agent's DB. There's no way for the operator to say "`chazmina` can schedule timers but only on herself."
3. **Ambient authority.** Tools carry `HostedIndex` (can enumerate all hosted agents by name/id) alongside the access handle. Proper ocap discipline says the tool should only see agents it's been granted.

## Design

### Capability Kind

Add a new host-only variant to `CapabilityKind`:

```rust
// src/extension/caps.rs

pub enum CapabilityKind {
    // ... existing variants ...
    /// Read/write agent-owned state (timers, memory, configuration).
    /// Host-only — only chaz core may provide the impl. The hub
    /// scopes each impl to the set of agents declared in the
    /// operator's tool_policy before handing it to the extension.
    AgentStateAdmin,
}
```

`is_host_only()` returns `true` for this variant (same as `SessionRead`/`SessionWrite`).

### Capability Request

```rust
pub enum CapabilityRequest {
    // ... existing variants ...
    /// The extension runs with access to hosted agent DBs. The
    /// `agents` field — when present — is the operator-configured
    /// allowlist. `None` means "all hosted agents" (the operator
    /// hasn't narrowed it yet).
    AgentStateAdmin {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agents: Option<Vec<String>>,
    },
}
```

The `agents` field is not set by the extension's manifest author — it's set by the operator in `tool_policy` and injected into the caps bundle by the hub during resolution. The manifest only declares the _kind_; the operator configures the _scope_.

### Trait

```rust
// src/extension/caps.rs

/// Narrow capability: access hosted agent DBs for state operations
/// (timers, memory, etc.). The hub scopes each impl to the set of
/// agents the operator allows before the extension sees it.
pub trait AgentStateAdmin: Send + Sync {
    /// Resolve an agent name to its `DbEntry`. Only agents in the
    /// operator-configured allowlist are visible; unrecognized or
    /// disallowed names return `Err(...)`.
    fn resolve_agent(&self, name: &str) -> Result<crate::hosted_index::DbEntry, String>;

    /// Open the agent DB identified by `entry`. Must be a DbEntry
    /// obtained from `resolve_agent` on the same handle. The impl
    /// uses the peer's held key to open the DB — no additional
    /// auth check beyond key possession.
    fn open_agent_db<'a>(
        &'a self,
        entry: &'a crate::hosted_index::DbEntry,
    ) -> CapFuture<'a, Result<crate::agent_db::AgentDb, String>>;
}
```

Note: `resolve_agent` replaces the `HostedIndex::find_by_name` calls the tools currently make. The match-by-ID path (`eidetica::entry::ID::parse`) is absorbed into the trait impl — the tool doesn't need to know whether the user passed a name or a DB id; the cap resolves both against the scoped set.

### Scoped Wrapper

The hub's factory builds a scoped implementation from the raw infrastructure handles:

```rust
// src/extension/agent_state.rs (new)

use std::collections::HashSet;
use std::sync::Arc;

use crate::agent_db::AgentDb;
use crate::extension::caps::{AgentStateAdmin, CapFuture};
use crate::hosted_index::{DbEntry, HostedIndex};
use crate::session::SessionRegistry;

/// `AgentStateAdmin` impl scoped to a specific set of agent names.
pub struct ScopedAgentStateAdmin {
    registry: Arc<SessionRegistry>,
    index: HostedIndex,
    /// Display names the cap holder is allowed to access.
    /// Empty = deny-all (cap was not granted at all).
    /// The caller resolves names through this set only.
    allowed: HashSet<String>,
}

impl ScopedAgentStateAdmin {
    pub fn new(
        registry: Arc<SessionRegistry>,
        index: HostedIndex,
        allowed: HashSet<String>,
    ) -> Self {
        Self { registry, index, allowed }
    }
}

impl AgentStateAdmin for ScopedAgentStateAdmin {
    fn resolve_agent(&self, name: &str) -> Result<DbEntry, String> {
        // Resolve via HostedIndex (name or DB id), then check scope.
        let entry = if let Some(e) = self.index.find_by_name(name) {
            e
        } else if let Ok(id) = eidetica::entry::ID::parse(name)
            && let Some(e) = self.index.find_by_id(&id)
        {
            e
        } else {
            return Err(format!("No hosted agent matches '{name}'"));
        };
        // Scope check: the agent's display name must be in the
        // operator-configured allowlist.
        if !self.allowed.is_empty() && !self.allowed.contains(&entry.display_name) {
            return Err(format!(
                "Agent '{}' is outside the allowed set for this capability",
                entry.display_name
            ));
        }
        Ok(entry)
    }

    fn open_agent_db<'a>(
        &'a self,
        entry: &'a DbEntry,
    ) -> CapFuture<'a, Result<AgentDb, String>> {
        Box::pin(async move {
            // Duplicate the scope check (defense in depth — the entry
            // should have come from resolve_agent, but verify anyway).
            if !self.allowed.is_empty() && !self.allowed.contains(&entry.display_name) {
                return Err(format!(
                    "Agent '{}' is outside the allowed set for this capability",
                    entry.display_name
                ));
            }
            let agent_db = self
                .registry
                .open_agent_db(&entry.db_id, Some(&entry.pubkey))
                .await
                .map_err(|e| format!("Failed to open agent DB: {e}"))?
                .ok_or_else(|| format!("No key for agent '{}' DB", entry.display_name))?;
            Ok(agent_db)
        })
    }
}
```

### Extension Caps Slot

```rust
// src/extension/caps.rs

pub struct ExtensionCaps {
    // ... existing slots ...
    /// Granted when the extension's manifest declares
    /// `AgentStateAdmin` and the operator has not denied it.
    /// None when the cap was not requested or was denied.
    pub agent_state_admin: Option<Arc<dyn AgentStateAdmin>>,
}
```

`ExtensionCaps::is_empty()` gains an `agent_state_admin.is_none()` check.

### Capability Declarations

```rust
// src/extension/caps.rs — extend CapProvider

pub enum CapProvider {
    // ... existing variants ...
    AgentStateAdmin(Arc<dyn AgentStateAdmin>),
}
```

### Tool Changes

Heartbeat tools drop `HostedIndex` and `Arc<dyn AgentDbAccess>`, receive `Arc<dyn AgentStateAdmin>` from the caps bundle instead:

```rust
pub struct HeartbeatAdd {
    agent_state: Arc<dyn AgentStateAdmin>,
}

impl HeartbeatAdd {
    pub fn new(agent_state: Arc<dyn AgentStateAdmin>) -> Self {
        Self { agent_state }
    }
}
```

`resolve_target_agent` changes from:

```rust
fn resolve_target_agent(
    ctx: &ToolContext,
    index: &HostedIndex,
    agent_ref: Option<&str>,
) -> Result<DbEntry, String> {
    let name = agent_ref.unwrap_or(ctx.agent_name.as_str());
    // HostedIndex::find_by_name + find_by_id ...
}
```

to:

```rust
fn resolve_target_agent(
    ctx: &ToolContext,
    cap: &dyn AgentStateAdmin,
    agent_ref: Option<&str>,
) -> Result<DbEntry, String> {
    let name = agent_ref.unwrap_or(ctx.agent_name.as_str());
    cap.resolve_agent(name)
}
```

The `open_agent_db` helper uses `cap.open_agent_db(&entry).await` instead of `access.open_agent_db(&entry).await`.

### Operator Configuration (Layer 2 — Blast Radius)

Per-extension agent allowlist in chaz config:

```yaml
# chaz config
agent_state_allowlist:
  heartbeat: [chaz, bash] # heartbeat extension can only touch these two agents
```

An absent entry means unrestricted (all hosted agents visible). An empty entry (`heartbeat: []`) means deny-all.

The hub resolves these at install time via `resolve_agent_allowlist()`:

| Manifest    | Operator    | Result                         |
| ----------- | ----------- | ------------------------------ |
| None        | None        | None (unrestricted)            |
| None        | Some([a,b]) | Some([a,b]) (operator narrows) |
| Some([a,b]) | None        | Some([a,b]) (manifest only)    |
| Some([a,b]) | Some([a])   | Some([a]) (intersection)       |
| Some([a])   | Some([c])   | Some([]) (no overlap)          |
| Some([])    | \*          | Some([]) (manifest deny-all)   |
| \*          | Some([])    | Some([]) (operator deny-all)   |

If the intersection is empty, the effective allowlist is `Some([])` — the `ScopedAgentStateAdmin` rejects all agents. The cap slot is still `Some` (not `None`) because the extension can still function — it just gets `Err` on every operation.

Per-tool scoping (in `tool_policy`) is a future refinement.

### Migration from AgentDbAccess

1. Add `AgentStateAdmin` trait, `CapabilityKind` variant, and `CapabilityRequest` variant to `caps.rs` (pure addition).
2. Add `ScopedAgentStateAdmin` as a new module `src/extension/agent_state.rs`.
3. Wire the hub to build `ScopedAgentStateAdmin` from operator config + `HostedIndex` + `SessionRegistry` during `install_all`.
4. Migrate heartbeat tools: drop `HostedIndex` and `Arc<dyn AgentDbAccess>`, take `Arc<dyn AgentStateAdmin>` from caps.
5. Migrate heartbeat extension: declare the cap in its manifest; tools receive the cap from the bundle.
6. Remove the `AgentDbAccess` trait from `tools/heartbeat.rs` (no consumers remain).

## Relationships to Other Caps

| Capability                     | Relationship                                                                                                                                                                 |
| ------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `SessionRead` / `SessionWrite` | Session-scoped. `AgentStateAdmin` is agent-scoped. The two are orthogonal — a tool might have both, one, or neither.                                                         |
| `ToolRegistration`             | The extension registers tools _using_ `ToolRegistration`; those tools _consume_ `AgentStateAdmin`. Different lifecycle phases (install vs. execute).                         |
| `Memory`                       | Future: `AgentStateAdmin` could subsume `MemoryAccess` for agent-scoped memory. Today they're separate — `Memory` is text search/recall; `AgentStateAdmin` is raw DB access. |
| `Shell` / `FileWrite`          | OS-level, enforced by `ToolHost` at tool-execute time. `AgentStateAdmin` is data-level, enforced by trait scoping at install time.                                           |

## Implementation Log

| Step                                                                                 | Status                          |
| ------------------------------------------------------------------------------------ | ------------------------------- |
| `CapabilityKind::AgentStateAdmin` + trait + request + provider + caps slot           | ✅ `caps.rs`                    |
| `ScopedAgentStateAdmin` wrapper                                                      | ✅ `agent_state.rs` (8 tests)   |
| Hub wiring — `set_hosted_index`, `build_agent_state_admin`                           | ✅ `extension/mod.rs`           |
| Operator config — `agent_state_allowlist` in `Config` + `set_agent_state_allowlist`  | ✅ `config.rs`, `main.rs`       |
| Allowlist intersection — `resolve_agent_allowlist`                                   | ✅ `extension/mod.rs` (8 tests) |
| Tool migration — `Arc<dyn AgentStateAdmin>` replaces `HostedIndex` + `AgentDbAccess` | ✅ `tools/heartbeat.rs`         |
| Extension migration — declares `AgentStateAdmin` in manifest                         | ✅ `extensions/heartbeat.rs`    |
| Remove old `AgentDbAccess`/`RegistryAgentDbAccess` traits                            | ✅                              |
| Clean up unused `_registry` parameter                                                | ✅                              |

## Tests

| Location           | Test                                             | What it verifies                                          |
| ------------------ | ------------------------------------------------ | --------------------------------------------------------- |
| `agent_state.rs`   | `scoped_resolve_allows_known_agent`              | Agent in allowed set resolves correctly                   |
| `agent_state.rs`   | `scoped_resolve_rejects_unknown_agent`           | Agent outside allowed set returns `Err`                   |
| `agent_state.rs`   | `scoped_resolve_resolves_by_id_and_checks_scope` | DB id lookup also enforces allowlist                      |
| `agent_state.rs`   | `scoped_resolve_by_id_rejects_scoped_out_agent`  | ID lookup of denied agent fails                           |
| `agent_state.rs`   | `scoped_open_db_rejects_scoped_out_entry`        | `open_agent_db` checks scope even without `resolve_agent` |
| `agent_state.rs`   | `scoped_open_db_succeeds_for_allowed_agent`      | Happy path — opens DB for allowed agent                   |
| `agent_state.rs`   | `none_allowlist_is_unrestricted`                 | `None` → all agents visible                               |
| `agent_state.rs`   | `empty_allowlist_denies_all`                     | `Some([])` → deny-all                                     |
| `extension/mod.rs` | `both_none_is_unrestricted`                      | No manifest or operator allowlist                         |
| `extension/mod.rs` | `operator_narrows_unrestricted_manifest`         | Operator restricts manifest                               |
| `extension/mod.rs` | `manifest_only_when_operator_absent`             | Manifest stands alone                                     |
| `extension/mod.rs` | `intersection_when_both_set`                     | Intersect of manifest + operator                          |
| `extension/mod.rs` | `no_overlap_returns_empty_deny_all`              | Non-overlapping = deny-all                                |
| `extension/mod.rs` | `manifest_empty_is_deny_all`                     | Manifest empty overrides operator                         |
| `extension/mod.rs` | `operator_empty_is_deny_all`                     | Operator empty overrides manifest                         |
| `extension/mod.rs` | `operator_matches_manifest_exactly`              | Exact match preserved                                     |
