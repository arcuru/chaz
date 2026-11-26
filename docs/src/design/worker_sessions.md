# Inspecting Worker Sessions

A Worker is a template in the calling Agent's configuration. Invoking it creates
one child session with its own Directive and turn attempt. The child uses the
parent's delegated authority and a narrowed tool scope; it has no Agent identity
of its own. The parent and child keep separate transcripts. A link between the
two lets you inspect the child without running it again.

## From a parent call to a reply

Consider a recorded parent turn that calls `spawn_worker` with
`{"name":"helper","task":"Check the reference"}`:

1. **Record the call.** The parent commits the model response containing the
   tool call before executing it. The requested task in that record can differ
   from the submitted task if a tool-call hook edits the arguments.
2. **Prepare the child.** With the parent's capacity permit still held, the
   executor prepares an unindexed child with parent delegation. It writes the
   submitted task (and any supplied context) as a Directive together with a
   backlink in the child DB. It then records the exact call, child DB ID and
   Directive row ID in the parent DB.
3. **Publish and run.** The executor publishes the child in the local catalog
   as `spawn:<uuid>`, installs the Worker runtime with the resolved prompt,
   model, tools and depth, and wakes it for the existing Directive. The UUID is
   the spawn ID; the session handle is the child DB's root ID. Neither the
   parent link nor the catalog row starts the Worker on its own.
4. **Wait.** By default, the parent yields its execution slot and waits on the
   child's in-process completion channel. This lets a child run even when all slots
   were occupied by parents submitting Workers. The parent turn remains
   reserved; it has not completed or been unloaded.
5. **Return the reply.** The child commits its completion and a reference to its
   final entry in the same transaction. The channel carries a notification,
   not the answer. The tool looks up the outcome for the submitted Directive
   and returns the referenced reply text, not the latest message in the child.
   A failed outcome is not a reply. The parent reacquires capacity before
   result hooks or further execution, records its own correlated tool result,
   and can continue its model conversation in the same attempt. The child entry
   holds the Worker's reply; the parent transcript holds what the parent loop
   received after output processing and size limits.

With `"async":true`, step 4 does not happen: the caller keeps its slot and
gets the child DB ID in the spawn result, not the Worker's answer. If
`worker_wait` is in scope, the same invocation can use that ID to wait for the
exact outcome. A bounded wait can return `{"state":"pending"}` without
cancelling the child. A later turn cannot join it just because it knows the ID.
See [Joining background Workers](../user_guide/agents.md#joining-background-workers).

## What the link stores

| Record               | Owner  | What it identifies                                                                           |
| -------------------- | ------ | -------------------------------------------------------------------------------------------- |
| `worker_spawns_v1`   | Parent | Spawn ID → call site, child DB ID and Directive row ID                                       |
| `worker_origin_v1`   | Child  | Parent DB ID and spawn ID                                                                    |
| `worker_outcomes_v1` | Child  | Directive, attempt, optional execution branch, terminal kind and optional final entry row ID |

Inside the parent DB, a call site identifies the request, attempt, optional
execution branch, Agent, model sequence, call index and call ID. A provider
call ID alone is not unique. The TUI matches the parent tool item to that site
and checks the reciprocal link; it does not infer ancestry from timestamps,
permission delegation or the `spawn:` prefix. Old children without a link
remain inspectable with unknown ancestry. Readers reject unknown link versions rather than guessing
which call or result they name. The side records contain neither a
second result body nor a saved Worker configuration.

Preparation and publication span two databases. A failure can leave a prepared
child without a parent link, or a linked child without an attempt. A Directive
with no recorded attempt shows what was submitted, not that a model ran. A
start without a terminal outcome does not establish success; a stop request is
not an acknowledged cancellation. Even a completed attempt with a missing
result reference cannot supply a result preview. Conversely, a parent tool
timeout does not prove the child stopped: its eventual reply can still appear
in the child. Inspect the existing records before considering another spawn.

## Reopening is not resuming

On the same service and login, a connected TUI refreshes the local catalog on
change. Open a Worker from its parent tool item, via `/workers` in the parent,
from the hub, or with `/join`. The child opens for observation; `/parent` follows
the backlink. The parent tool summary keeps the requested task and parent-call status separate
from the child outcome. Open the child for its full transcript rather than
looking for its internal tool traffic in the parent. Neither navigation nor
reopening the TUI submits a Directive, claims a runtime or restores the
parent's wait.

The hub groups children by verified origin, not by a guess from their names.
In the [parent transcript](tool_activity.md), expanding a tool run reveals
individual Worker summaries without interleaving two conversations. The TUI
refreshes visible child evidence at most once every five seconds, with up to
four concurrent reads. A fresh heartbeat means activity was recorded, not that
the reader has proved the executor is still alive. An old heartbeat without a
terminal record leaves the outcome unknown.

Opening uses a locally indexed Worker session and requires existing Write
(or Admin) authority. A client with a stale account snapshot can bind its
observer handle to the same login's existing direct key without overwriting a
key mapping or replacing a bound delegated identity. A reader on an independent
peer may sync the parent link without having the child DB or its keys. The link
remains visible; it does not fetch the child or grant access.

After an executor restart the committed transcripts, links and outcomes remain.
The narrowed Worker runtime and completion channel do not. Resident adoption
skips `spawn:` children, and neither an inline spawn wait nor `worker_wait`
writes a `job_wait` checkpoint. A parent interrupted at either call cannot
automatically pick up the result and continue. Compare [Checkpointed Job
Waits](checkpointed_waits.md) for the distinct `job_wait` protocol.
