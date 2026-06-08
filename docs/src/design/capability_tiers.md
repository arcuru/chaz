# Capability Tiers

Design rationale for how capability grants compose across scopes. For the user-facing config, see [Security → Capability tiers](../user_guide/security.md#capability-tiers-and-attenuation).

## Problem

Capability grants were a flat, per-tool map (`HashMap<String, Grants>` keyed by tool name) composed by `merge_over`, which did **per-kind replacement**. Two flaws followed:

1. **No chokepoint.** Authority was sliced per-tool, but resources are shared across tools — the filesystem is reachable through both `shell` and `write_file`; the network through both `web_fetch` and `shell` (`curl`). "This agent has no network" or "this session is confined to `/work`" was inexpressible as a single setting; you had to restrict every resource-touching tool consistently and trust none routed around it.

2. **Composition widened authority.** `merge_over` let a more-specific layer _replace_ a kind wholesale, so an agent overlay with `deny: []` could remove a denial the baseline imposed. For a layered security model the operator must only ever _subtract_.

The tool-list axis already had the right discipline: `ScopedTools::narrow()` composes by intersection (`parent ∩ child`), so a child can only subtract. The fix was to apply that discipline to `Grants`.

## Model: four tiers, composed by attenuation

Effective authority for any single capability request is the intersection of four scopes:

```
effective = tool policy  ∩  session ceiling  ∩  agent capabilities  ∩  per-tool override
```

Each inner tier can only subtract. Resolution lives in `ToolContext::resolve_call_grants`, called once per tool invocation.

| Tier               | Lives on                               | Why                                                        |
| ------------------ | -------------------------------------- | ---------------------------------------------------------- |
| Tool policy        | `security.tool_policies.<tool>.grants` | Baseline for a tool across all agents                      |
| Session ceiling    | `SessionMeta.capabilities`             | The whole-session bound; travels with the session          |
| Agent capabilities | `agents[].capabilities`                | The agent-wide chokepoint, binding all of an agent's tools |
| Per-tool override  | `agents[].grants.<tool>`               | The finest grain, one tool for one agent                   |

### The `attenuate` operator

`Grants::attenuate(self, narrower)` is monotonic (most-restrictive-wins), replacing `merge_over`. Per capability kind:

- **Allowlists** (`shell.allow`, `network.endpoints`, `fs.allow_read/write`) → **intersection**, with an _empty_ allowlist treated as the permissive top (so `∅ ∩ X = X`, not deny-all — preserving the historical "empty = allow-all" enforcement semantics). Shell and fs use prefix-aware intersection (keep the more-specific prefix); endpoints use coverage-aware intersection mirroring `NetworkPolicy::host_matches` (wildcards, path prefixes, methods).
- **Denylists** (`shell.deny`) → **union**.
- **Booleans** (`network.allow_private`) → **AND**.

The `merge_over` widen-bug becomes structurally impossible.

## Policy layer vs. enforcement point

All four tiers resolve to one effective `Grants`, but _where_ each becomes real differs:

- **Agent capabilities, per-tool override, tool policy** are pure policy resolution — they intersect into the effective `Grants` the host receives and checks in-process.
- **Session ceiling** is the tier that ultimately wants a stronger enforcement point. "No network this session" is a network namespace with no interface; "confined to `/work`" is a mount whose only bind is the working dir. A grant string can't enforce that — `shell` would bypass it. The resolved session ceiling is _exactly_ the parameter set a sandboxed host would be launched with (mounts, netns, WASI preopens).

## Status

- **Done:** the `attenuate` operator; the four-tier resolution; agent-wide `capabilities`; session `capabilities` on `SessionMeta`; `FsGrant` enforcement in `exec_file_read`/`exec_file_write` (advisory — lexical path-prefix bound, symlinks not followed).
- **Deferred (per-session enforced host):** today `ToolContext.host` is effectively a global `NativeToolHost` that checks grants in-process. The remaining work is to construct the host **per session** from the resolved ceiling — first as the grant holder, then as a `BubblewrapToolHost`/WASM host launched with the session ceiling as its mounts/netns/preopens. `BubblewrapToolHost` already exists in-tree (`crates/lib/src/bubblewrap_host.rs`) but is not yet config-selectable or wired to the resolved ceiling. This is the larger lift and is tracked alongside the [WASM sandboxing research](https://github.com/arcuru/chaz) in brain. Two encoding questions ride along: an explicit unset-vs-empty allowlist type so a hard "deny-all" / "no egress" ceiling is expressible, and the source of the session ceiling (creator config vs. home peer vs. explicit `/session` setting).
