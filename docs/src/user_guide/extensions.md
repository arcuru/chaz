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

| Extension           | Provides                     | What it gives you                                                                                                                                    |
| ------------------- | ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| `context_prune`     | Tool, _(context projection)_ | Opt-in historical pointers and `tool_history_query`; see [Pointers instead of old tool payloads](#pointers-instead-of-old-tool-payloads).            |
| `baseline_context`  | _(context strategy)_         | Required default history/layout selection; see [One required context strategy](#one-required-context-strategy).                                      |
| `core`              | Tool                         | `shell`, `compact`, `spawn_agent`, `job_status`, `job_wait`, `spawn_worker`. The always-available baseline; disabling it is a footgun.               |
| `system`            | Tool                         | `get_time`, `calculate`, `describe_tool`. Small dependency-free helpers.                                                                             |
| `fs`                | Tool                         | `read_file`, `write_file`, `edit_file`.                                                                                                              |
| `web`               | Tool                         | `web_fetch`, `web_search`.                                                                                                                           |
| `memory`            | Command, Tool                | `/memory` + `remember` / `recall` / `list_memory_banks`. See [Memory](memory.md).                                                                    |
| `skills`            | Command, Tool                | `/skills` + `skill_list` / `skill_search` / `skill_show`, plus the per-session catalog prompt injection.                                             |
| `schedule`          | Command, Tool                | `/schedule` + `schedule_add` / `schedule_modify` / `schedule_remove` / `schedule_list` / `schedule_once`.                                            |
| `agent_schedule`    | _(routine handler)_          | Standalone fire path for agent-owned schedules. Not directly toggled by users; no tools or commands.                                                 |
| `mcp-<server>`      | Tool                         | One extension per configured [MCP server](mcp.md), named `mcp-<server_name>`. Wraps the server and registers its tools under the server's namespace. |
| `path_normalizer`   | ToolCall                     | Strips trailing slashes from `path` arguments on filesystem tools before they execute.                                                               |
| `security_warnings` | ToolResult                   | Scans tool output for prompt-injection patterns and logs warnings (warning-only — output is unmodified).                                             |

Disabling an extension hides its tools from the LLM, stops its hooks
from firing, and disarms its slash commands. The `core` and `system`
extensions are practical floors — chaz still runs without them, but
agents lose `shell`, `spawn_agent`, `get_time`, etc. The configured context
strategy is different: disabling it stops model dispatch, rather than hiding tools.

## Listing extensions

```
/extensions
/extensions list
```

Both forms show every extension registered on this peer, marked for the
agent responding in this session:

```
Extensions on this peer (✓ = live for agent 'chaz' this session; ✗ = disabled for this agent):
  ✓ baseline_context [0.3.0] — —
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

## Context projection grants

An extension can offer a **context projector**: code that sees the
request about to be sent to the model and returns a reshaped one, such
as recall inserted near the end, a large old tool output replaced with a
stub, or secrets redacted. It runs before every model call of a turn,
including each step of a tool loop, not once per turn. The required baseline selection runs before this optional projection chain.

What is automatic and what you control:

- **You grant it.** Only extensions listed in `context_projection` in
  the config ([reference](configuration.md#extensions)) ever run as
  projectors. Installing or `/extensions add`-ing one is not a grant.
- **Activation still applies.** A granted projector runs only while the
  extension is active for the session and the responding agent.
  `/extensions remove` stops it from the next model call, even mid-turn.
  Session removal and agent opt-outs also stop the extension's tools at
  their next actual execution; changing model-facing declarations does
  not freeze earlier execution permissions.
- **Authority is per grant.** `conversation` projectors cannot change the
  system instructions or the tool list the model sees. `full_context`
  projectors can, but they can only hide or re-describe tools the turn
  already has. Neither affects what tools actually run.
- **The originals are kept.** The session transcript, the stored tool
  results, and the next call's starting point are the unprojected
  originals. A projection never becomes history.

The host checks every projector's output: tool calls and their results
must stay paired and unaltered (a whole old exchange may be dropped),
opaque provider data must match, the current turn's own tool exchanges
must stay, and the whole request must fit the model's context budget.

Failure modes:

| Situation                                                | `required: false`                       | `required: true`                      |
| -------------------------------------------------------- | --------------------------------------- | ------------------------------------- |
| Extension missing, inactive, or not offering a projector | Skipped; never called                   | Turn ends with an error; nothing sent |
| Projector errors, panics, takes over 30 s                | Skipped; earlier projectors' edits kept | Turn ends with an error; nothing sent |
| Output breaks a rule above                               | Output discarded; earlier edits kept    | Turn ends with an error; nothing sent |
| Request over budget after every step                     | Turn ends with an error; nothing sent   | Turn ends with an error; nothing sent |

Mark a projector `required` when sending without it would be wrong, as
with redaction. Leave it optional when it only saves tokens.

## Walkthrough: a required projector that gets switched off

Scenario: your config grants a `redactor` extension as a required
conversation projector, and someone disables it in one session.

1. The grant in `config.yaml`:

   ```yaml
   context_projection:
     - extension: redactor
       required: true
   ```

   Every model call in every session now passes through `redactor`
   first.

2. In one session, someone runs:

   ```
   /extensions remove redactor
   ```

3. The next model call in that session is refused before anything
   reaches the model provider. The turn ends with an error entry:

   ```
   Error: required context projector 'redactor' is unavailable: it is not active for this session and agent
   ```

   If the extension is not installed on this peer at all, the reason
   reads `no instance exists for this agent or session` instead.

4. **Recovery:** run `/extensions add redactor` in that session and send
   the message again. To let the session continue without redaction,
   set `required: false` (or remove the entry) and restart chaz; the
   projector is then skipped wherever it is inactive.

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

## Durable context is an explicit write

A context projector changes only the next request. A durable contributor
intentionally stores conversation text and strategy state in the session's
Eidetica database, so they survive reopen and sync to the session's peers.
No shipped extension uses this endpoint yet. It is available to trusted
in-tree extensions without a feature flag or plugin loader.

| Setting                       | Default | Meaning                                                                                                                                     |
| ----------------------------- | ------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `durable_context[].extension` | —       | Contributor's manifest name; distinct names run in list order.                                                                              |
| `durable_context[].required`  | `false` | Stop dispatch if this contributor is absent, inactive or fails. Optional failures discard the new batch and require a valid committed view. |

Installation and activation are not write grants. The responding agent and
session bound the contributor's text/state namespace, with session instances
taking precedence over agent and global instances. It cannot write another
extension's state. The host commits a batch and its dedup receipt together,
then builds context. A retry of the same request does not inject again; a new
request may. This also applies after reopening the session.

Stored text is escaped conversation data, not a new instruction or tool grant.
Current instructions and tools always come from current authority. Disabling
the extension hides its durable text at the next model call but does not erase
its records. Re-enabling it can expose those records again. Contributions are
retained independently of compaction summaries and must still fit the request
budget. An oversized request is refused, not silently sent or summarized.

Do not use this capability to persist private external recall into a synced
session. It grants no new audience, auxiliary model calls or retention policy.
The legacy schedule-turn path cannot create a contribution without a durable
request identity; optional writes skip and required writes refuse there.

## Walkthrough: retry and disable a durable contributor

This example assumes your trusted in-tree extension `notes` provides the
`DurableContextContributor` endpoint and returns one text item, `remember this`.
It is an example consumer, not a preinstalled extension.

1. Grant it and restart with the updated config:

   ```yaml
   durable_context:
     - extension: notes
       required: true
   ```

   Send a message. The model sees this host-framed User item only after its
   batch has committed:

   ```text
   <custom_context>
   remember this
   </custom_context>
   ```

2. If the turn is interrupted, retry that same request. Reopening the session
   does not change the result: the stored receipt prevents a second injection.
   A later, genuinely new message has a distinct invocation and can add notes.

3. Disable `notes` with `/extensions remove notes`. The next turn is refused
   before any provider request, with this error entry:

   ```text
   Error: required durable contributor 'notes' is unavailable or inactive
   ```

4. Recover with `/extensions add notes`, then send a new message. The refusal
   above is recorded as a completed error, so `/retry` cannot reopen it; an
   earlier interrupted request can still be retried. To continue without notes,
   remove its grant or set `required: false` and restart. Optional failure keeps
   earlier committed batches only if the database view remains coherent;
   storage corruption is never a reason to use a stale in-memory fallback.

## One required context strategy

`baseline_context` ships in the normal build and is selected by default.
It keeps the existing layout: current instructions, recent selected history
with complete prior tool exchanges when they fit, ephemeral recall, then
explicit durable contributions. The newest selected entry remains included;
an oversized final request is refused at dispatch rather than silently sent.

| Control                                    | Default                                   | Meaning                                                                                    |
| ------------------------------------------ | ----------------------------------------- | ------------------------------------------------------------------------------------------ |
| `context_strategy.extension`               | `baseline_context`                        | Required installed selection provider; replacement is explicit.                            |
| `baseline_context` session setting `cache` | Last tool, system, latest user; 5 minutes | Typed anchor/TTL choices, not provider fields.                                             |
| `cache.anchors`                            | `["last_tool", "system", "latest_user"]`  | At most four anchors; `{"message": 1}` selects a final-request conversation item by index. |
| `cache.ttl`                                | `five_minutes`                            | `five_minutes` or `one_hour`; unsupported providers omit markers.                          |

Selection is a permission boundary, not an optimization with a hidden fallback.
Disabling or losing the configured strategy stops the next model call, including
an active tool loop. Another trusted in-tree extension can replace it through
`context_strategy.extension`; it must publish `ContextStrategy` and be active.
Activation alone cannot select a replacement or grant it extra tools.

The host reads a coherent Eidetica view. The extension chooses scoped references;
the host renders transport framing, complete native exchanges and durable text,
and assigns source identities. The selected name and settings are recorded in
session-backed, agent/extension-scoped `context_selections` state. Session settings
remain in `extension_settings` and are read anew each turn. Neither is a saved
model request or an execution grant. There is no projection cache.

Native memory recall still uses its existing activation, bank eligibility,
query extraction and per-bank search budgets. Its typed invocation carries the
agent, session and request identity plus recent message text. Recall is still
ephemeral: changing recall does not accumulate old tails in the session. A recall
failure keeps the successful required selection; it never substitutes another
strategy. Persisting recall requires a separate explicit durable contributor
and grant; native memory does not install one.

Adapters map selected anchors to supported wire fields. Native Anthropic and
OpenRouter's Anthropic models support explicit markers; other OpenAI-compatible
providers receive no markers. A message anchor stays on that item's last block,
even if Anthropic coalesces adjacent User messages. Marker placement does not
prove a provider cache hit, and the default policy is unchanged.
If an optional durable contribution is revoked, the host removes its message
anchors and remaps the remaining anchors before projection. A projector that
changes message positions must update its returned cache indices as well.

## Walkthrough: change cache settings and recover a disabled strategy

1. Inspect and change the baseline's session settings:

   ```text
   /extensions settings baseline_context
   Settings for 'baseline_context' on this session:
   {}
   /extensions set baseline_context cache {"anchors":["last_tool","system","latest_user"],"ttl":"five_minutes"}
   Set baseline_context.cache = {"anchors":["last_tool","system","latest_user"],"ttl":"five_minutes"}
   ```

   The next turn reads the new setting. This example keeps the default policy;
   changing TTL or anchors is a deliberate operator choice, not an automatic
   optimization. Invalid types, indices or more than four anchors stop selection.

2. Disable it in this session:

   ```text
   /extensions remove baseline_context
   ```

   Send a new message. No provider request is sent; the error reads:

   ```text
   Error: required context strategy 'baseline_context' is unavailable or inactive
   ```

3. Recover with `/extensions add baseline_context`, then send a new message.
   The refusal was recorded as a completed error, so it is not a retry target.
   An earlier interrupted request can still be retried through the usual path.

4. To use a replacement, install its trusted in-tree implementation and activate
   it, then restart with:

   ```yaml
   context_strategy:
     extension: my_context
   ```

   `my_context` now owns selection. Removing it refuses dispatch even though
   `baseline_context` remains installed. No external loader or Cargo variant
   is needed or provided.

## Pointers instead of old tool payloads

`context_prune` ships in the normal build, but does not substitute anything by
default. It needs an operator `context_projection` grant, current session/agent
activation, and the session setting `enabled: true`. Keep the grant optional:
pruning is an optimization, not the required context strategy.

| Setting or tool                   | Default  | Meaning                                                                                      |
| --------------------------------- | -------- | -------------------------------------------------------------------------------------------- |
| `context_prune.enabled`           | `false`  | Substitute eligible historical results in outgoing requests only.                            |
| `context_prune.min_bytes`         | `16384`  | Minimum retained result size in UTF-8 bytes.                                                 |
| `context_prune.min_savings_bytes` | `1024`   | Minimum reduction in the framed request payload; replacements must also be strictly shorter. |
| `context_prune.exclude_tools`     | `[]`     | Tool names whose results stay unchanged.                                                     |
| `tool_history_query.reference`    | Required | Stable `tr1:` reference from a pointer, not a visible message index.                         |
| `tool_history_query.offset`       | `0`      | Stored UTF-8 byte offset; must be a character boundary.                                      |
| `tool_history_query.max_bytes`    | `16384`  | Page bound in stored bytes, from 1 through 16384.                                            |

The thresholds deliberately leave small results alone. No model chooses them,
no summary is generated, and no auxiliary inference runs. A pointer reports
the recorded byte length, content hash, stored truncation marker and recovery
call. The host preserves the tool-result envelope, call arguments, identities,
order and opaque provider continuation data.

Only successful results in completed historical exchanges are eligible.
Current or incomplete exchanges, recovery-tool output, and bodies already
edited by an earlier transform stay unchanged. If the model-facing tool list
omits `tool_history_query`, no pointers are emitted. The selected required
strategy still controls which exchanges enter the request; pruning does not
retrieve extra history or silently replace that strategy.

### Retained evidence is not an archive promise

Recovery reads existing Eidetica `turn_transcript` records. The reference binds
the session and immutable recorded source, including its bytes. It survives a
real database reopen and context compaction because neither a visible-vector
index nor an in-memory replacement map participates in its identity.

A reference is not permission. Recovery requires the normal current tool grant
for `tool_history_query`, current extension activation and session participation,
and current scoped visibility of the original source tool. Living Agent
participation requires its current Active Write key, even if the roster cache
still lists it. Detaching the last agent denies recovery from an already-running
turn; an empty roster does not restore legacy access. Removing that source
from the caller's allowed tools makes its results unavailable to recovery.
Session/agent extension revocations attenuate this scope. Possessing a synced
database or a `full_context` projection grant alone does not grant recovery or
permission to execute the original tool. Reading evidence never executes it;
filesystem/network execution grants remain separate resource permissions.
Compaction hides context, not retained evidence authorized under this scope.

`status: "missing_or_not_authorized"` reports an unavailable source without
revealing whether it exists elsewhere. `stored_truncation_marker: true` reports
the recording bound's marker; legacy records have no independent truncation
bit, so even a literal marker is reported conservatively. Recovery promises
only recorded, retained bytes after tool hooks, security scanning and recording
bounds. It cannot restore earlier output, bytes never recorded, or sources
removed by a future retention policy.

Disabling `enabled` stops all substitutions on the next logical call. The
required baseline exposes retained originals under its normal selection and
budgeting policy. If the resulting request exceeds the final budget, the host
sends nothing; it does not secretly keep a disabled pruner running. Re-enabling
reconstructs fresh pointers from current sources and settings. There is no
persisted replacement state to leak across toggles. Optional read/projector
failure keeps the preceding accepted request, including required transforms;
the remaining chain and final budget check still run.

### Walkthrough: enable, recover, and disable

1. Add an optional grant to `config.yaml` and restart your disposable/test peer:

   ```yaml
   context_projection:
     - extension: context_prune
       authority: conversation
       required: false
   ```

   Permit `tool_history_query` and the desired original source tools in the
   agent's allowed tools. Activate the extension if it was removed:

   ```text
   /extensions add context_prune
   /extensions set context_prune enabled true
   Set context_prune.enabled = true
   ```

2. Complete a turn that produces a large tool result, then send another message.
   The historical result can become a pointer inside the same `tool_output`
   envelope. Copy its `tr1:` reference into a recovery call (replace `REF` with
   the full reference):

   ```json
   { "reference": "REF", "offset": 0, "max_bytes": 4096 }
   ```

   `tool_history_query` returns JSON with `status: "retained"`, `content`,
   `stored_bytes`, `sha256`, `stored_truncation_marker` and `next_offset`.
   Request the next page using that offset until `next_offset` is `null`.
   Reopen the peer or compact context and the same authorized reference still
   resolves while its records are retained.

3. Remove the source tool from the caller's allowed tools and try the reference
   again. The response reports:

   ```json
   {
     "reference": "REF",
     "status": "missing_or_not_authorized",
     "offset": 0,
     "next_offset": null,
     "stored_bytes": 0,
     "sha256": null,
     "stored_truncation_marker": false,
     "content": ""
   }
   ```

   Restore the source-tool allowlist to recover access. A foreign-session or
   forged reference returns the same unavailable status. Removing the recovery
   tool itself instead prevents tool execution.

4. Stop substitutions without revoking read-only recovery:

   ```text
   /extensions set context_prune enabled false
   Set context_prune.enabled = false
   ```

   Send another message: original historical payloads return, subject to normal
   baseline budgeting. An over-budget final request is refused before provider
   dispatch. Set `enabled true` again to reconstruct pointers, or remove the
   optional projection grant to stop projection everywhere on this peer.
