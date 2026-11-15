# Inspecting Worker Sessions

Status: partial implementation. Same-service catalog refresh and inspection-only
Worker opening are implemented. Exact spawn relations, terminal references,
parent summaries, grouping and parent navigation below remain an implementation
contract, not shipped behavior.

A Worker template is configuration, a session is a durable transcript, and a turn
attempt is an execution of one request in that transcript.
A `spawn_worker` invocation creates a child session, but those identities are not
interchangeable.
This increment connects the existing identities, repairs discovery by an already
connected TUI, and makes inspection explicitly non-executing.
It does not turn Workers into Agent jobs or introduce a universal work scheduler.

## What fails today

`spawn_worker` registers a `spawn:UUID` child, delegates authority from its parent,
writes a Directive, and normally waits for the result.
The child's transcript persists.
The synchronous tool result contains the answer, not a child-session reference.
Neither the permission delegation nor that answer identifies the exact spawning
call for the UI.

There are two different discovery boundaries:

- Separate Chaz processes using the **same Eidetica service and login** share the
  session catalog. A running TUI caches `session_list_fresh` without invalidating
  it for another process's child creation. A newly connected TUI can already see
  and inspect that child. The older client's `AccountSession` can also lack the
  newly created child's key mapping: a direct `/join` then reports
  `no session signing key` even though a newly connected client can open it.
- Independent Eidetica peers have independent local catalogs. A synced parent
  needs a durable child reference to make the child discoverable there. Learning
  a reference does not establish availability or authorization to read its target.

These are not one catalog-replication problem.
Do not sync `chaz_group`, enumerate every account database on each frame, or add
an Agent session exposure that would enroll the Worker in ordinary execution.

## One relation, existing payloads

Use additive versioned side records, not new transcript variants or fields in
`SessionMeta` that older full-meta writers could discard.
The following names describe the implementation contract:

| Record                                                    | Canonical owner | Contents                                                                          |
| --------------------------------------------------------- | --------------- | --------------------------------------------------------------------------------- |
| `worker_spawns_v1`, keyed by spawn ID                     | Parent session  | Exact invocation identity; child session ID; child Directive row ID               |
| `worker_origin_v1`, one origin                            | Child session   | Parent session ID and spawn ID, a backlink to the relation                        |
| `worker_outcomes_v1`, keyed by attempt/execution identity | Child session   | Request/attempt/execution identity, terminal kind and optional final entry row ID |

The existing spawn UUID can be the spawn ID; it identifies this creation, not a
logical task, Agent or attempt.
The parent relation is insert-only.
A conflicting reuse is an error, not an overwrite or a reason to create another
child automatically.
The backlink is a reference, not a second copy of the relation or a permission.
Validate both directions before treating an association as confirmed.
Do not infer ancestry from auth delegation, `spawn:` alone, matching text,
provider call ID alone, or timestamps.

The invocation identity is the same tuple used by nested tool activity, scoped
by the parent DB:

```text
request_id, attempt_id, execution_id?, agent,
model_sequence, call_index, call_id
```

`ToolContext` currently carries the request and an opaque `tool_call_key`, but
not the full attempt/branch identity.
Thread a typed call-site value from the server's known attempt/branch and the
runtime's known model sequence/index/ID before dispatch.
Keep the existing opaque key's consumers unchanged; do not parse it to recover
identity or search for the latest active attempt.
The direct registration helpers used outside a persisted runtime call may retain
an explicitly unknown origin; they must not manufacture native call identities.
Production recorded spawns must carry the full tuple.

Names, assignment, context, result text, tool arguments and tool outcomes already
have canonical payloads.
Read the Worker name and assignment from the exact recorded parent invocation,
and the full result from its recorded source.
Do not copy them into a second catalog, summary record or accounting row.
Post-hook tool arguments may differ from the model proposal: label a parent
preview as the requested assignment; the child Directive is the actual submitted
assignment and remains available in the child.
Do not archive another Worker configuration snapshot for this feature.

## Publish references before execution

Use the existing separate preparation/publication pattern for child creation,
without adopting Agent-job admission or changing Worker tool/depth ceilings:

1. Allocate an unindexed child and establish the existing parent delegation.
2. In one child transaction, insert its Directive and origin backlink. Retain the
   returned Directive row ID in memory. Do not install a processing callback or
   claim runtime yet.
3. Commit the parent relation, containing that child and Directive ID, through
   the actual parent writer identity. Only completed child setup is referenced.
4. Publish the child's ordinary local catalog row and install the existing
   Worker-specific runtime with its narrowed tools, prompt, model and depth.
   Explicitly wake that runtime for the already committed Directive.

Prevent generic resident adoption from running this prepared `spawn:` child.
Only the original Worker registration starts it; the reference and catalog are
not executable requests.
Refactor only the small creation/registration boundary needed for this ordering.
Keep unrelated `create_child_session` consumers and Agent-job admission intact.

Eidetica transactions do not span parent and child databases.
Failure before the parent reference can leave an unindexed child; preserve it,
report the failure, and do not execute it.
Failure after the reference but before runtime registration leaves a discoverable
child with a recorded Directive and no execution evidence.
Report that state honestly. Do not automatically retry creation or reconstruct
Worker authority from display metadata after restart.
A timed-out parent call is not proof its child stopped.
A lost reply is not permission to replay the spawn.

## Terminal evidence without another transcript

The existing completed `TurnAttempt` does not identify its final display entry
in every path: response metadata is optional, and the latest entry can belong
to another turn.
Add only the missing reference for new Worker children.
When `complete_turn_attempt_for_branch` commits the initial child's completion,
write a `worker_outcomes_v1` row in that **same transaction** after obtaining the
final entry's inserted row ID.
Bind it to the origin's Directive, actual attempt and optional execution branch.
Keep all original entries and native transcript records unchanged.
Use no copied result body.

Terminal kinds distinguish a committed reply, failure, acknowledged cancellation
and completed-without-reply.
Derive cancellation from the executor's actual terminal evidence, not a stop
request. The stored final-entry reference supplies the text where one exists.
Unknown future kinds and broken references are unavailable evidence, never success.
Do not reuse `job_acceptance`, `job_result` or the Executor job queue for Workers.

The TUI reads a coherent child snapshot and reports evidence, not remote liveness:

| Evidence for the linked Directive                 | Presentation                                            |
| ------------------------------------------------- | ------------------------------------------------------- |
| Directive, no attempt                             | Not started / no attempt recorded                       |
| Started attempt with fresh activity               | Activity recorded; no terminal result                   |
| Started attempt with stale/missing activity       | Outcome unknown                                         |
| Stop request only                                 | Stop requested; outcome not yet recorded                |
| Matching committed outcome and final entry        | Completed, failed or cancelled, with its result preview |
| Completed attempt but no usable outcome reference | Completed attempt; result unavailable                   |
| Missing/denied/unsupported child                  | Transcript unavailable; retain the reference            |

Attempt generation and native branch evidence resolve multiple attempts; do not
select by wall clock or let another branch's completion settle this one.
The parent tool's own pending/failed/succeeded status remains separate.
For example, a timed-out `spawn_worker` call can have a child whose later reply is
inspectable; neither fact should overwrite the other.
No descendant usage roll-up is added.

## Discovery and authorized opening

Repair the two same-service issues rather than inventing a sharing protocol:

- Observe catalog writes through an independent read callback; do not consume
  the server's single new-session receiver. Coalesce notifications, invalidate
  the cached list, and refresh asynchronously while the hub is visible. Refresh
  on explicit hub entry too. Keep cached rows during a read failure and show
  their stale/error status. Preserve selected session identity across refresh.
- Reuse the existing same-login job-observer opening mechanism for locally
  indexed Worker sessions, factored narrowly if useful. This is an explicit
  Worker-observer opening path, not a fallback added to general `open_session`
  and not a replacement for native account-metadata freshness repair.
  Repair a missing key-map hint only with the same login's existing direct key,
  validated by
  the service against the actual child. Mapping is not an auth grant. Do not
  replace an already bound delegated identity with a broader key or mistake
  lookup failure for permission to use arbitrary account keys.
- Retain the current native materialization boundary: observation requires
  existing Write authority (Admin allowed) where the service requires it.
  Unsupported delegated service permission resolution must return unavailable,
  not panic, claim Read-only support, or gain new permission.

Parent records travel with the parent's existing sync contract.
A reader can therefore discover a child reference without sharing the local
catalog. Resolve it only through existing authorized local/keyed handles and
already established sync mechanisms. Do not bootstrap arbitrary remote DBs,
auto-approve shares, install a new delegation, or contact an address supplied by
record text. A missing target remains a visible unavailable reference, with the
existing explicit sharing/sync procedure as recovery.
This increment does not promise automatic acquisition of every child by every
independent peer. It must work end to end in the actual split Chaz daemon/TUI,
same-service topology, and preserve references when the parent is shared.

## Parent transcript, hub and navigation

Extend [nested tool activity](tool_activity.md), not its persistence or grouping
rules. Match a Worker relation to exactly one tool item using the full call key.
A Worker item has a compact name/assignment, child evidence status, result preview
and an **Open child** action. Its expanded details retain the complete original
arguments/result and distinct parent-call outcome.
Do not inline the child's tool traffic into the parent.

Keep the outer run's existing two-row collapsed bound and all interleaved parent
prose. Its second row can advertise linked Workers and an **Open workers** action;
expanding the run reveals the individual Worker summaries. A parent-filtered
Worker picker provides the full list without making a large spawn batch an
unbounded collapsed transcript region.
Use sanitized, terminal-width-aware previews and existing full-content copy paths.
A display label or preview is never a session identifier or an instruction.

The hub derives a forest from verified origins, retaining independent roots and
unknown/legacy ancestry as ordinary navigable rows.
Read only small origin/relation metadata to establish grouping; never hydrate
all transcripts or compute recency/usage to sort the tree.
Bound metadata concurrency and coalesce refreshes. Apply a settled ordering at a
refresh boundary, retaining cursor identity, rather than moving rows on every
lazy description fill. Retain current root ordering; use stable IDs to break
ties. Cycles, conflicting parents and missing parents cannot hide a session or
cause an infinite walk: show an explicit unknown/conflicted root/reference.
Existing Agent-job ancestry may be projected from its existing validated records;
this does not alter job admission, scheduling or the separate Tasks view.

Use stable session IDs for all actions:

- **Open child** opens or focuses an observation-only tab at the linked Directive.
- **Open workers** (also `/workers` in the parent TUI) opens the parent-filtered hub;
  arrows and Enter select the child, including when its result is not yet known.
- **Parent** in a child header (also `/parent`) returns to the parent and the exact
  spawning item when available; otherwise it explains why the target is unavailable.
- Existing `/join` and hub selection recognize Worker sessions and use this same
  observation path, including legacy `spawn:` children without exact ancestry.

Reuse the non-executing callback/snapshot machinery used by job observation,
without treating a Worker as an accepted Agent job or enabling job steering.
Opening a Worker must not call `register_session`, `register_child_session`,
`claim_runtime`, auto-attach an Agent, publish a Directive, or execute a hook/tool.
The Worker inspector has no message composer, retry, settings mutation or stop
control in this first increment. Navigation, expansion, scrolling and copy remain.
Returning to an already open parent preserves its normal draft and tool expansion.
A newly opened parent uses the existing client observation path; navigation must
not acquire executor authority even when the TUI itself is executor-capable.

## Compatibility and bounded implementation

Old history is untouched. `spawn:` indicates creation kind only, not ancestry.
Never backfill links by parsing old free-form replies or auth settings.
New records are optional metadata, not instructions used to rebuild model context
or execution authority. Older writers can continue creating unlinked children;
new readers label them unknown. Older readers must still read the unchanged
transcripts and ignore the side stores without executing anything new.
Test that compatibility with the supported older binary before enabling writers;
unknown record versions retain an unavailable view, not guessed v1 semantics.
No conversation-schema bump or Store redesign is justified by these display
references alone.

Implement in bounded, integrated steps:

1. Add typed relation/origin readers and the safe opening/observation path; prove
   same-service catalog invalidation and stale-key recovery with positive and
   negative authorization cases.
2. Add native call-site threading, staged Worker publication and atomic outcome
   references. Test partial-publication and missing-result boundaries with real
   Eidetica transactions, not a JSON-only simulation.
3. Integrate the parent tool projection, grouped hub and navigation, plus reference,
   conceptual and worked-example user documentation. Preserve existing nested
   tool, selection and navigation tests.
4. Run whole-change review and final project gates on the final signed candidate.

The remaining work is bounded engineering under these contracts, not an owner
request for a comprehensive session/task taxonomy.
If an actual API limitation requires new access, destructive migration or new
execution semantics, stop at that boundary rather than substituting a workaround.

## Required evidence

Extend the existing PTY infrastructure with a real client-role TUI, separate
resident Chaz executor, disposable Eidetica service and fenced loopback model stub.
Do not substitute an in-process registry test for this transport exercise.
Capture actual terminal screens/raw PTY and stub request counts for:

- A hub already loaded before multiple native Worker children are created;
  live and completed child rows, per-call summaries, and summary → child → parent.
- Parent prose, sibling roots, legacy children without ancestry, repeated child
  inspection, narrow/resized terminals, mouse/keyboard actions and safe copy.
- A failed child, failed parent call with later child completion, missing outcome,
  cancellation request versus acknowledgement, and unavailable/unauthorized targets.
- Reopening TUI and restarting the disposable executor, including an interrupted
  child: no automatic Worker reconstruction, no success invented from parent status.
- No additional model calls, runtime ownership, Agent attachment or Directive
  publication caused by listing, refresh, opening or navigating back.
- Existing same-login direct authorization as a positive control; an unrelated
  login, forged/mismatched relation and unsupported delegated service identity as
  negative controls. A reference is never the permission check.
- Parent sync between independent Eidetica peers preserving references, with
  authorized available targets inspectable and absent/denied targets honest.
- Old reader/new side-record compatibility and old history/new reader behavior.

Run normal Chaz Nix full gates and the relevant two-peer `just e2e` gate.
Record whole-change review, final verification and ready-for-review PR readback.
The source change does not deploy or migrate a production instance.
