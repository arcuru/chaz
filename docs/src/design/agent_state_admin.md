# Agent State Admin Capability

**Status:** Implemented (2026-05-18). Error/UX reconciliation 2026-05-19 (Gap 3): uniform not-found error. Startup deny-all `WARN`: shipped 2026-08-18 (see the second status update below).
**Depends on:** cap traits landed (`crates/lib/src/extension/caps.rs`), hub wiring (Steps 2–5 of cap refactor), `AgentDbAccess` trait (landed in `crates/lib/src/tools/schedule.rs`).

> **Status update (2026-05-27):** the `ExtensionCaps` bundle layer described below was deleted by `refactor(extension): delete inert ExtensionCaps bundle layer` (commit `03ba480`). The cap itself still exists and is still operator-scoped via `agent_state_allowlist`, but it is now reached through `PeerHandles.agent_state_allowlist` plus `ScopedAgentStateAdmin` built inside an `ExtensionInstance` (see [Extension Framework](../architecture/extensions.md)). The "Extension Caps Slot" / `CapProvider::AgentStateAdmin` sections below describe an intermediate shape that no longer exists in code; the security posture and operator-config layer (`agent_state_allowlist`, intersection table, startup deny-all `WARN`) are unchanged.

> **Status update (2026-08-18):** the manifest∩operator intersection model never shipped and is retracted as a future refinement. `resolve_agent_allowlist()` and the hub-side factory (`build_agent_state_admin`) do not exist, and no extension declares `AgentStateAdmin` in its manifest — `requested_capabilities` is declaration vocabulary with no live declarers. The operator's `agent_state_allowlist` map is the **only** scoping input: each consuming extension reads its own entry from `PeerHandles.agent_state_allowlist` at instantiate time and builds its `ScopedAgentStateAdmin` itself. What this pass added is the startup deny-all `WARN` (`deny_all_warning` in `agent_state.rs`, logged at the consuming extension's construction site). The Capability-Request `agents` injection, the intersection table, and the hub-factory passages below describe that future refinement, not shipped behavior.

## Security posture

> **This capability system is a guardrail, not a sandbox.** It is designed to stop a poorly behaving agent or tool from doing accidental damage — deleting the wrong schedule, scheduling noise into another agent's DB, writing to a path outside `~/code/`. It is **not** designed to contain an LLM or tool that is explicitly, adversarially trying to escape. If we can achieve the latter, that's great, but it is not the requirement driving this design.

The distinction matters:

| What the guardrail stops                                                   | What it doesn't try to stop                                                                |
| -------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| A too-eager agent scheduling schedules on every hosted agent               | A tool that discovers it has `Arc<SessionRegistry>` and walks the object graph to escalate |
| An extension registering tools it didn't declare                           | A WASM extension that exploits a VM escape                                                 |
| A shell tool that `rm -rf ~/` because an agent hallucinated a cleanup step | An agent that builds and runs native code through the shell tool it was granted            |
| A file-write tool scribbling in `/etc`                                     | A tool using `ptrace` or `/proc` to read another process's memory                          |

The ceiling (extension capabilities) and floor (tool policy) together create a **defense-in-depth** model where each layer catches different classes of mistakes. Neither layer alone is a security boundary — together they cover the failure modes that actually show up in practice.

## Problem

Heartbeat tools (`schedule_add`, `schedule_modify`, `schedule_remove`, `schedule_list`, `schedule_once`) read and write agent-owned schedules in the target agent's eidetica DB. Today they receive an `Arc<dyn AgentDbAccess>` handle at construction time — an untyped, unscoped, undeclared capability:

1. **Not in the cap system.** `CapabilityKind` has no variant for "access agent state." The trait exists in `tools/schedule.rs`, invisible to manifests, extensions, or the hub.
2. **No attenuation.** The handle opens _any_ hosted agent's DB. There's no way for the operator to say "`chazmina` can schedule schedules but only on herself."
3. **Ambient authority.** Tools carry `HostedIndex` (can enumerate all hosted agents by name/id) alongside the access handle. Proper ocap discipline says the tool should only see agents it's been granted.

## Design

### Capability Kind

Add a new host-only variant to `CapabilityKind`:

```rust
// crates/lib/src/extension/caps.rs

pub enum CapabilityKind {
    // ... existing variants ...
    /// Read/write agent-owned state (schedules, memory, configuration).
    /// Host-only — only chaz core may provide the impl. The hub
    /// scopes each impl to the set of agents declared in the
    /// operator's `agent_state_allowlist` map before handing it to the extension.
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

The `agents` field is not set by the extension's manifest author — it's set by the operator (in the top-level `agent_state_allowlist` map, not `tool_policy`) and injected during resolution, per the intersection table below. The manifest only declares the _kind_; the operator configures the _scope_. _(Future refinement — see the 2026-08-18 status update; today nothing injects this field.)_

### Trait

```rust
// crates/lib/src/extension/caps.rs

/// Narrow capability: access hosted agent DBs for state operations
/// (schedules, memory, etc.). The hub scopes each impl to the set of
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
// crates/lib/src/extension/agent_state.rs

/// An `AgentStateAdmin` whose `resolve_agent` and `open_agent_db`
/// reject agents outside an operator-configured allowlist.
pub struct ScopedAgentStateAdmin {
    registry: Arc<SessionRegistry>,
    index: HostedIndex,
    /// `Some(set)` — only agents in `set` are accessible, and an
    /// empty set means deny-all. `None` — unrestricted; all hosted
    /// agents are visible.
    allowed: Option<HashSet<String>>,
}

impl ScopedAgentStateAdmin {
    /// Build a scoped handle. `None` = unrestricted (the operator
    /// hasn't narrowed this extension); `Some(list)` = only the named
    /// agents; `Some(empty)` = deny-all (surfaced once at startup —
    /// see `deny_all_warning`).
    pub fn new(
        registry: Arc<SessionRegistry>,
        index: HostedIndex,
        allowlist: Option<Vec<String>>,
    ) -> Self { /* … */ }

    /// `true` when `display_name` is within this handle's scope.
    fn in_scope(&self, display_name: &str) -> bool { /* … */ }
}

impl AgentStateAdmin for ScopedAgentStateAdmin {
    fn resolve_agent(&self, name: &str) -> Result<DbEntry, String> {
        // Resolve via HostedIndex (name or DB id), then check scope.
        // A scoped-out agent is reported as not-found, identical to a
        // genuinely missing one (see Error semantics below).
        /* … */
    }

    fn open_agent_db<'a>(&'a self, entry: &'a DbEntry) -> CapFuture<'a, AgentDb> {
        // Defense in depth — the entry should have come through
        // `resolve_agent`, but verify the scope anyway. Same
        // not-found masking as the resolve path.
        /* … */
    }
}
```

The encoding that shipped is `Option<HashSet<String>>`, not a bare `HashSet`: `None` (absent operator entry) means unrestricted, while `Some(empty)` means deny-all. An earlier draft of this section used `HashSet<String>` with "empty = deny-all", which left no way to express "no narrowing applied" — the two meanings are distinct and the `Option` keeps them apart.

### Extension Caps Slot

```rust
// crates/lib/src/extension/caps.rs

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
// crates/lib/src/extension/caps.rs — extend CapProvider

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
  schedule: [chaz, bash] # schedule extension can only touch these two agents
```

An absent entry means unrestricted (all hosted agents visible). An empty entry (`schedule: []`) means deny-all.

This map is, by design, the **operator mutation surface** — it is
peer-local operator policy, applied once at startup. There is no runtime
command to mutate it (a runtime override would need a persistence/sync
model — peer-local vs. synced, who may change it — that is an open
decision, deliberately deferred rather than guessed). A deny-all entry
is surfaced at boot via `WARN` (see Error semantics below) because at
the tool boundary it is indistinguishable from a working configuration.

In the shape that shipped, this map is the **only** scoping input: the
consuming extension reads its own entry
(`peer.agent_state_allowlist.get(<extension name>)`) and passes it to
`ScopedAgentStateAdmin::new` as the `Option<Vec<String>>` allowlist —
`None` when the entry is absent, the list otherwise. There is no
manifest side to intersect with (see the 2026-08-18 status update).

**Future refinement — manifest∩operator intersection.** If extensions
ever declare `AgentStateAdmin { agents }` in their manifests, resolution
would intersect the two sides:

| Manifest    | Operator    | Result                         |
| ----------- | ----------- | ------------------------------ |
| None        | None        | None (unrestricted)            |
| None        | Some([a,b]) | Some([a,b]) (operator narrows) |
| Some([a,b]) | None        | Some([a,b]) (manifest only)    |
| Some([a,b]) | Some([a])   | Some([a]) (intersection)       |
| Some([a])   | Some([c])   | Some([]) (no overlap)          |
| Some([])    | \*          | Some([]) (manifest deny-all)   |
| \*          | Some([])    | Some([]) (operator deny-all)   |

If the intersection is empty, the effective allowlist is `Some([])` — the `ScopedAgentStateAdmin` rejects every agent with the uniform not-found error. The extension still loads and runs; it just gets `Err` on every operation.

Per-tool scoping (in `tool_policy`) is a future refinement.

### Error semantics (Gap 3 reconciliation)

A scoped-out agent is **indistinguishable from a non-existent one** at
the cap boundary: both `resolve_agent` and `open_agent_db` return the
uniform `No hosted agent matches '<ref>'` — the same wording `/agent`
uses for an unresolved ref. This collapses the original "two errors for
one concept" wart (a denied lookup used to say "outside the allowed set"
while a missing one said "not found") and avoids leaking the existence
of out-of-scope agents to extension tools.

Because the deny-all (`Some([])`) state is invisible at the tool
boundary, it would otherwise be a silent footgun. So the consuming
extension emits a one-time **`WARN`** at its construction site when its
allowlist resolves to an empty list, naming the extension and the
config key to fix (`deny_all_warning` in `agent_state.rs`); healthy
shapes — unrestricted or a non-empty list — stay silent. The operator
finds out at boot, not from a confused user staring at a "not found"
error.

### Migration from AgentDbAccess

1. Add `AgentStateAdmin` trait, `CapabilityKind` variant, and `CapabilityRequest` variant to `caps.rs` (pure addition).
2. Add `ScopedAgentStateAdmin` as a new module `crates/lib/src/extension/agent_state.rs`.
3. Hub exposes the operator map — `set_hosted_index` + `set_agent_state_allowlist` on `ExtensionHub`, wired from `Config` at server build. Extensions build their own `ScopedAgentStateAdmin` from the map at instantiate; there is no hub-side factory.
4. Migrate heartbeat tools: drop `HostedIndex` and `Arc<dyn AgentDbAccess>`, take `Arc<dyn AgentStateAdmin>`.
5. Migrate heartbeat extension: build the scoped handle from the operator map in `instantiate`. The manifest declares nothing — host-only cap requests are declaration vocabulary with no live declarers (see the 2026-08-18 status update).
6. Remove the `AgentDbAccess` trait from `tools/schedule.rs` (no consumers remain).

## Relationships to Other Caps

| Capability                     | Relationship                                                                                                                                                                 |
| ------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `SessionRead` / `SessionWrite` | Session-scoped. `AgentStateAdmin` is agent-scoped. The two are orthogonal — a tool might have both, one, or neither.                                                         |
| `ToolRegistration`             | The extension registers tools _using_ `ToolRegistration`; those tools _consume_ `AgentStateAdmin`. Different lifecycle phases (install vs. execute).                         |
| `Memory`                       | Future: `AgentStateAdmin` could subsume `MemoryAccess` for agent-scoped memory. Today they're separate — `Memory` is text search/recall; `AgentStateAdmin` is raw DB access. |
| `Shell` / `FileWrite`          | OS-level, enforced by `ToolHost` at tool-execute time. `AgentStateAdmin` is data-level, enforced by trait scoping at install time.                                           |

## Implementation Log

| Step                                                                                 | Status                                                              |
| ------------------------------------------------------------------------------------ | ------------------------------------------------------------------- |
| `CapabilityKind::AgentStateAdmin` + trait + request variant                          | ✅ `caps.rs` (declaration vocabulary — no manifest declares it yet) |
| `ScopedAgentStateAdmin` wrapper                                                      | ✅ `agent_state.rs` (10 tests)                                      |
| Hub wiring — `set_hosted_index` + `set_agent_state_allowlist`, wired at server build | ✅ `extension/mod.rs`, `server/build.rs`                            |
| Operator config — `agent_state_allowlist` in `Config`                                | ✅ `config.rs`                                                      |
| Startup deny-all `WARN` — `deny_all_warning` + call site in schedule `instantiate`   | ✅ `agent_state.rs`, `extensions/schedule.rs` (2026-08-18)          |
| Tool migration — `Arc<dyn AgentStateAdmin>` replaces `HostedIndex` + `AgentDbAccess` | ✅ `tools/schedule.rs`                                              |
| Extension wiring — schedule builds its own scoped handle from the operator map       | ✅ `extensions/schedule.rs`                                         |
| Remove old `AgentDbAccess`/`RegistryAgentDbAccess` traits                            | ✅                                                                  |
| Manifest declaration + `resolve_agent_allowlist()` intersection                      | ⛔ not shipped — retracted as future refinement (2026-08-18)        |

## Tests

| Location         | Test                                              | What it verifies                                          |
| ---------------- | ------------------------------------------------- | --------------------------------------------------------- |
| `agent_state.rs` | `scoped_resolve_allows_known_agent`               | Agent in allowed set resolves correctly                   |
| `agent_state.rs` | `scoped_resolve_rejects_unknown_agent`            | Agent outside allowed set returns the uniform not-found   |
| `agent_state.rs` | `scoped_resolve_resolves_by_id_and_checks_scope`  | DB id lookup also enforces allowlist                      |
| `agent_state.rs` | `scoped_resolve_by_id_rejects_scoped_out_agent`   | ID lookup of denied agent fails                           |
| `agent_state.rs` | `scoped_open_db_rejects_scoped_out_entry`         | `open_agent_db` checks scope even without `resolve_agent` |
| `agent_state.rs` | `scoped_open_db_succeeds_for_allowed_agent`       | Happy path — opens DB for allowed agent                   |
| `agent_state.rs` | `none_allowlist_is_unrestricted`                  | `None` → all agents visible                               |
| `agent_state.rs` | `empty_allowlist_denies_all`                      | `Some([])` → deny-all, surfaces as not-found              |
| `agent_state.rs` | `deny_all_warning_names_extension_and_config_key` | Startup WARN names the extension and the config key       |
| `agent_state.rs` | `deny_all_warning_silent_for_healthy_shapes`      | Unrestricted / non-empty lists produce no warning         |

The `extension/mod.rs` intersection tests listed in earlier drafts of this
document were never written — they belong to the manifest∩operator model
retracted in the 2026-08-18 status update.
