# Extensions

Chaz's tools, hooks, and slash commands are organized into **extensions** —
each one a bundle of related capabilities (filesystem tools, web tools,
the schedule scheduler, etc.). Every session has its own active subset:
you can disable an extension on one session and keep it on another.

The `/extensions` command controls per-session activation and settings.

## Listing extensions

```
/extensions
/extensions list
```

Both forms show every extension registered on this peer:

```
Extensions on this peer (✓ = active on this session):
  ✓ core [0.3.0] — Tool
  ✓ path_normalizer [0.3.0] — ToolCall
  ✓ security_warnings [0.3.0] — ToolResult
  ✓ fs [0.3.0] — Tool
  ✓ system [0.3.0] — Tool
  ✓ web [0.3.0] — Tool
  ✓ memory [0.3.0] — Tool
  ✓ schedule [0.3.0] — Command, Tool
```

The version in brackets is the chaz binary version that registered the
extension. The trailing list is the hook kinds it declared — handy for
seeing at a glance which extensions provide tools, which provide
commands, and which only hook into the agent lifecycle.

By default every new session starts with every extension active.

## Adding an extension

```
/extensions add memory
```

Activates `memory` on the current session. Takes effect on the **next
agent turn**:

- The extension's tools become visible to the LLM (added to the tool
  list it sees in its next response).
- The extension's `tool_call` / `tool_result` / `before_agent_start`
  hooks start firing.
- The extension's slash commands start dispatching (e.g. `/schedule`
  begins to work).

Re-running `/extensions add` on an already-active extension is a no-op
and reports back that it's already active.

## Removing an extension

```
/extensions remove memory
```

Deactivates `memory` on the current session. Takes effect on the next
agent turn — the next LLM call won't see `remember` / `recall` /
`list_memory_banks` in its tool list, hooks stop firing, and any of the
extension's slash commands return a clear error pointing back to
`/extensions add`.

**Removal persists across restarts.** Each add/remove appends an event
to the session's eidetica `extensions` log; the session-start
reconciler folds the log to determine the current set rather than
defaulting everything back on. If you want the extension back, run
`/extensions add` explicitly.

Some extensions are practical floors (`core`, `system`) — chaz still
runs without them, but you lose `shell`, `spawn_agent`, `get_time`,
etc. Disable at your own risk.

## Inspecting settings

```
/extensions settings schedule
```

Prints the per-session settings JSON for the named extension. With no
overrides written, this returns `{}` — extensions fall back to their
own [`default_settings`](../architecture/extensions.md#per-session-settings)
when a key is missing.

## Changing settings

```
/extensions set <name> <key> <value>
```

Merges `key = value` into the named extension's per-session settings.
`<value>` is JSON-parsed first, so:

- `/extensions set schedule max_retries 3` stores the number `3`
- `/extensions set schedule enabled true` stores the boolean `true`
- `/extensions set schedule label "morning sweep"` stores a string
- `/extensions set schedule tags '["urgent","ops"]'` stores an array
- `/extensions set schedule label foo` — `foo` doesn't parse as JSON,
  so it's stored as the string `"foo"`

Each call replaces the value at that key while preserving every other
key already stored. There's no `/extensions unset` yet — to clear a
key, overwrite it with `null`:

```
/extensions set memory custom_limit null
```

## What "active on this session" means

Activation status is **per session**, persisted in the session's
eidetica DB, and syncs along with the session via eidetica's
replication. The same chaz binary can have one session with `memory`
disabled and another with it enabled.

When a session is opened on a peer where the binary supports an
extension that didn't exist when the session was last touched, that
new extension is default-activated on the first session*start. When a
session is opened on a peer where the binary is \_missing* an extension
that the session had been using, the activation event for that
extension stays in the log but does nothing — chaz can't load code
it doesn't have. Re-opening the session on a peer that has the
extension reactivates it from the existing log.

## Matrix syntax

In Matrix rooms, the command works the same way under the `!chaz`
prefix:

```
!chaz extensions
!chaz extensions add memory
!chaz extensions remove memory
!chaz extensions settings schedule
!chaz extensions set schedule poll_secs 60
```

## Where extensions come from today

Every extension currently shipped with chaz is compile-time built into
the binary; the `extension_ref` shown by `/extensions list` records the
binary version that built it. Future versions will support extensions
loaded from external sources — eidetica DBs, IPLD addresses, or git
commits — but those loaders aren't implemented yet. See
[the architecture doc](../architecture/extensions.md#extension-identity)
for the shape that machinery will take.
