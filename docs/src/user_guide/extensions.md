# Extensions

Chaz's tools, hooks, and slash commands are organized into **extensions** —
each one a bundle of related capabilities (filesystem tools, web tools,
the schedule scheduler, etc.). Every session has its own active subset:
you can disable an extension on one session and keep it on another.
Individual agents can also opt out of an extension just for themselves
(see [Per-agent scope](#per-agent-scope)).

The `/extensions` command controls activation and settings.

## Built-in extension catalog

These ship in the chaz binary today. The "Provides" column lists the
[hook kinds](../architecture/extensions.md#hook-kinds) the extension
declares — `Tool` and `Command` are the surfaces a user notices; the
others are runtime hooks that fire around each agent turn.

| Extension           | Provides            | What it gives you                                                                                                                                    |
| ------------------- | ------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| `core`              | Tool                | `shell`, `compact`, `spawn_agent`, `spawn_task`. The always-available baseline; disabling it is a footgun.                                           |
| `system`            | Tool                | `get_time`, `calculate`, `describe_tool`. Small dependency-free helpers.                                                                             |
| `fs`                | Tool                | `read_file`, `write_file`, `edit_file`.                                                                                                              |
| `web`               | Tool                | `web_fetch`, `web_search`.                                                                                                                           |
| `memory`            | Command, Tool       | `/memory` + `remember` / `recall` / `list_memory_banks`. See [Memory](memory.md).                                                                    |
| `skills`            | Command, Tool       | `/skills` + `skill_list` / `skill_search` / `skill_show`, plus the per-session catalog prompt injection.                                             |
| `schedule`          | Command, Tool       | `/schedule` + `schedule_add` / `schedule_modify` / `schedule_remove` / `schedule_list` / `schedule_once`.                                            |
| `agent_schedule`    | _(routine handler)_ | Standalone fire path for agent-owned schedules. Not directly toggled by users; no tools or commands.                                                 |
| `mcp-<server>`      | Tool                | One extension per configured [MCP server](mcp.md), named `mcp-<server_name>`. Wraps the server and registers its tools under the server's namespace. |
| `path_normalizer`   | ToolCall            | Strips trailing slashes from `path` arguments on filesystem tools before they execute.                                                               |
| `security_warnings` | ToolResult          | Scans tool output for prompt-injection patterns and logs warnings (warning-only — output is unmodified).                                             |

Disabling an extension hides its tools from the LLM, stops its hooks
from firing, and disarms its slash commands. The `core` and `system`
extensions are practical floors — chaz still runs without them, but
agents lose `shell`, `spawn_agent`, `get_time`, etc.

## Listing extensions

```
/extensions
/extensions list
```

Both forms show every extension registered on this peer, marked for the
agent responding in this session:

```
Extensions on this peer (✓ = live for agent 'chaz' this session; ✗ = disabled for this agent):
  ✓ core [0.3.0] — Tool
  ✓ path_normalizer [0.3.0] — ToolCall
  ✓ security_warnings [0.3.0] — ToolResult
  ✓ fs [0.3.0] — Tool
  ✓ system [0.3.0] — Tool
  ✓ web [0.3.0] — Tool
  ✓ memory [0.3.0] — Command, Tool
  ✓ skills [0.3.0] — Command, Tool
  ✓ agent_schedule [0.3.0] — —
  ✓ mcp-filesystem [0.3.0] — Tool
  ✗ schedule [0.3.0] — Command, Tool  (session: on, agent: off)
```

The version in brackets is the chaz binary version that registered the
extension. The trailing list is the hook kinds it declared — handy for
seeing at a glance which extensions provide tools, which provide
commands, and which only hook into the agent lifecycle.

The marker reflects the **effective** state for the responding agent:

- `✓` — active for this agent on this session.
- `✗` — disabled. A trailing `(session: on, agent: off)` means the
  session has it on but _this agent_ opted out (see
  [Per-agent scope](#per-agent-scope)); otherwise it's off for the
  whole session.

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

## Per-agent scope

`add` and `remove` take an optional trailing scope token — `session`
(the default) or `agent`:

```
/extensions remove schedule agent
/extensions add schedule agent
```

- **`session`** (default) — edits the session's `extensions` log.
  Affects every agent responding in the session.
- **`agent`** — edits the _responding agent's_ Living Agent DB. The
  agent can only **narrow** the session set: `remove … agent` records an
  opt-out for that one agent, and `add … agent` clears a prior opt-out.
  It cannot turn an extension on that the session has turned off — the
  session set is the upper bound.

The effective set for a turn is therefore **session set minus the
agent's opt-outs**. An agent's opt-outs travel with it when the agent
syncs to other peers, so a shared agent keeps its narrowed set
everywhere it runs. `remove … agent` only works for an agent hosted on
this peer (the one whose DB this peer can write).

This is useful when several agents share a room but one of them
shouldn't, say, touch the scheduler — disable it for that agent without
affecting the others.

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

## Walkthrough: disable scheduling for one agent in a shared room

Scenario: three agents (`chaz`, `nova`, `archivist`) share one Matrix room. You want `nova` to stop being able to schedule itself — but `chaz` and `archivist` should keep their schedulers.

1. From the TUI or Matrix room, while `nova` is the responding agent, scope the change to the agent:

   ```
   /extensions remove schedule agent
   ```

   This writes a `Deactivated` event to `nova`'s Living Agent DB. The session's active set is untouched.

2. Verify with `/extensions`. While `nova` is responding you'll see:

   ```
   ✗ schedule [0.3.0] — Command, Tool  (session: on, agent: off)
   ```

   Switch the responding agent to `chaz` (`@chaz: …` in the room) and run `/extensions` again — `schedule` is `✓` for `chaz`. The opt-out only narrows `nova`.

3. Confirm `nova` can't use the surface. Have `nova` try:

   ```
   @nova: schedule a daily summary for 9am
   ```

   The `schedule_*` tools aren't in `nova`'s tool list any more. If `nova` (or you, while `nova` is responding) tries `/schedule list`, the dispatcher returns:

   ```
   /schedule is provided by the 'schedule' extension, which is not active on this session. Use `/extensions add schedule` to enable it.
   ```

   (The slash-command check fires on the session set; for an agent-only opt-out the slash command still works for other agents in the same session — only the tools disappear for `nova`'s turns.)

4. **Recovery**: to undo the opt-out, run `/extensions add schedule agent` while `nova` is responding. The agent's DB records an `Activated` event that clears the opt-out; the next `nova` turn picks it up.

5. **Cross-peer note**: agent opt-outs travel with the agent DB through eidetica sync. If `nova` is co-owned with another peer, that peer also stops surfacing `schedule` for `nova` once the event has synced — no need to repeat the command there.

## Matrix syntax

In Matrix rooms, the command works the same way under the `!chaz`
prefix:

```
!chaz extensions
!chaz extensions add memory
!chaz extensions remove memory
!chaz extensions remove schedule agent
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
