# Nested Tool Activity in the TUI

Status: implemented as a TUI-only projection of native session evidence.

The conversation should show what the agent said, with a compact account of its
tools, rather than two unrelated rows for every invocation and result.
This is a TUI projection of existing evidence, not a change to execution, stored
history, model context, or tool payloads.

## Presentation

Use one collapsible **tool run** per identified request/attempt/execution/agent
in a session tab.
A run is not a contiguous slice of the conversation: commentary and other agents
can occur between its tools.
Its header appears at its first identified invocation.
Collapsing it hides only its tool items, wherever they occur; it never hides
messages, commentary, directives, errors, cancellation requests, or approvals.
An individual item contains a recorded invocation and its identity-matched result.
It stays at the invocation's position when the result arrives.

The collapsed run occupies **two terminal rows**, independent of call count and
whether the attempt is active.
The first row identifies the run, agent, call count and most important status.
The second gives outcome counts.
Both rows are sanitized and clipped by terminal-cell width before paragraph
wrapping; prose is neither clipped nor charged against this bound.
A narrow header indicates truncation when space permits; expansion recovers its
full identity and counts.
The bound is per identified run, not per session: two concurrent agents and two
adjacent attempts must still have separate headers.

Expanded runs reveal one compact row per invocation at its original location.
Expanding an item reveals complete recorded arguments, result, outcome and
provenance.
The outer toggle retains inner choices.
The existing `Ctrl+O` and `/expand` global override applies to both levels;
`Ctrl+T` remains new-tab, not expansion.
No new keyboard mode, inspector pane, persistence preference or summarizer is
needed.

### Concrete layouts

This forty-column excerpt comes from the native-snapshot TUI buffer fixture
(with pane borders omitted).
The compact tool rows stay at two; prose remains outside collapse.

```text
chaz [local] [R1 e0]:
  I will inspect these files.

▸ … R1 chaz · 48 tools · attempt acti…
  0 done · 0 failed · 0 cancelled · 4…
chaz [local] [R1 e1]:
  First checks are back; looking
further.
```

`R1` is a tab-local display label for a stable run identity, not a new execution
ID.
`e2` is that run's recorded event sequence, not a global clock.
The header summarizes the entire run, including events later than its position.
The failure marker must precede text that can be clipped; do not show a success
marker simply because one call succeeded.
At very narrow widths the complete counts require expansion.

The following excerpt illustrates expanded items, omitting the full audit identities:

```text
chaz [R1 e0]: I will check them.
▾ ! R1 chaz · 3 tools · attempt active
  1 done · 1 failed · 0 cancelled · 1 without result · 0 unknown
  ▾ R1 #1 query · done [e0.0→e3] path: file-1
    call e0.0 → result e3 (later)
    Arguments:
      path: file-1
    Result:
      first line
        indented line
  ▸ R1 #2 query · failed [e0.1→e1] path: file-2
chaz [R1 e2]: First checks are back;
looking further.
  ▸ R1 #3 query · awaiting result [e4.0→?] path: file-3
```

Moving the result into its invocation item must not imply it preceded the
commentary.
Every paired item exposes invocation/result event positions even when its body
is collapsed; the line above illustrates the full provenance on expansion.
Known interleaved `TurnContext` prose receives the same run/event label, without
changing its content or order.
Debug details include full identities and timestamps.
Unknown provenance is labelled unknown, never assigned to the nearest agent.
There is no second rendered copy of a consumed result.

## Read the evidence that already exists

The existing TUI receives `SessionEntry` values without their row identities.
Those values alone cannot safely pair tools.
Successful display results are also truncated to 500 characters by
`server::display_entries_for_record`.
Neither parsing `name(args)` nor joining adjacent rows can recover the missing
identity or full output.

The existing native records supply what is needed:

- `Session::entries_with_ids()` returns the loaded entries and stable row IDs.
- `TurnTranscriptRecord` supplies request ID, attempt ID, event sequence,
  timestamp, and optional agent/execution identity.
- A `ModelResponse` supplies model sequence and indexed tool calls with IDs,
  names and recorded arguments.
- A `ToolResult` supplies model sequence, call index, call ID, name, complete
  recorded output and typed outcome.
- `TurnAttempt`, `TurnActivity` and `Cancelled` supply lifecycle evidence.

Load the tool evidence in `refresh::snapshot_from_session`, using
`db.new_transaction_at(&tips)` and the existing typed `turn_transcript` and
`turn_attempts` tables.
Read only through the already authorized session handle, at the same immutable
snapshot as the entries, metadata and telemetry.
Do not call the live `Session::turn_transcript` once per attempt: that is both
repeated I/O and a different snapshot.
This needs no new core writer, public API or wire field.
The existing public entry-ID iterator avoids a second entries read.

Both initial `tab_from_snapshot` and later `apply_session_snapshot` must carry
this projection.
Keep row IDs aligned with entries when merging/sorting local command output;
local output has no persisted ID and cannot acquire a tool association.
Keep the projection and expansion sets in tab-local state, not in the DB.
An evidence-read failure retains the last coherent view with the existing stale
read/error indication and retry; it must not cover the snapshot as fresh or
turn missing evidence into success.
An absent table on old history is an empty typed source, with legacy fallback.
Unknown typed tool outcomes have no display audit row in the native writer;
retain their exact identity-matched payload as unknown without consuming any row.

### Exact association

The run key is `(request_id, attempt_id, execution_id, agent)`, scoped by the tab's
session DB ID.
The call key adds `(model_sequence, call_index, call_id)`.
Use the full key, not a shortened display label, provider ID alone, tool name,
text equality, arrival order or timestamp.
Check that the result's name agrees with its invocation before consuming it.

Current `append_turn_transcript_for_branch` writes display rows with keys:

```text
{attempt_id}:{execution_id_or_empty}:{record_sequence:020}:{display_index:010}
```

Construct and look up the exact expected key from the typed record; do not parse
arbitrary IDs by splitting on colons.
For a nonterminal model response, a nonempty content string occupies display
index zero, followed by the tool calls in order.
For a recognized tool result, its audit row is display index zero.
Validate the expected entry kind and sender before replacing any audit row.
Keep compatibility with this writer shape covered by a native-storage test.

Only exactly associated `ToolCall` and `ToolResult` audit rows are consumed.
Never replace or suppress prose with `ModelResponse.content` from the typed
source: terminal model responses are observable before reply hooks, final
publication and attempt settlement.
The existing displayed `Message` remains the authority for the final answer.
Full typed results are used even when the corresponding display audit was
truncated; full means all bytes actually retained by the runtime, not bytes
already removed by its output limit.

Counts describe identified calls in that run, not unpaired rows elsewhere in the
transcript.
Missing associations must not be disguised as complete coverage.

Native-storage prototyping confirmed that this key shape survives reopening,
keeps reused provider IDs separate across attempts, and recovers full results
when their display rows contain only previews.
This establishes the source contract's feasibility, not production TUI acceptance.

### Incomplete and older data

If a call or result cannot be identified unambiguously, keep its original audit
row, labelled `unpaired / identity unavailable`, with its full available text
expandable.
Do not group unknown rows into a purported turn or pair them by adjacency.
An older display-only result can be incomplete; say `stored preview only` when
full evidence is absent rather than pretending to recover its missing suffix.
Malformed argument JSON is plain text, not repaired JSON.
Unknown outcomes stay unknown with retained payload; they are not successes.
Conflicting duplicate identities stay visible as unpaired evidence rather than
silently selecting a winner or discarding a row.

The two-row guarantee applies to identified runs.
Genuinely identity-less history cannot truthfully provide per-turn aggregation;
its explicit unpaired fallback is intentional, including after reopening.
Restored modern history must reproduce the same paired items and run boundaries
as live history, with expansion reset to the normal view default.

## Truthful state

| Evidence                                                             | Display                                                             |
| -------------------------------------------------------------------- | ------------------------------------------------------------------- |
| Invocation, no result, fresh activity for its attempt                | Awaiting result; attempt active                                     |
| Invocation, no result, completed attempt without an execution branch | Missing result                                                      |
| Invocation in a branch, no result, attempt completed elsewhere       | Result not recorded; branch state unknown                           |
| Invocation, no result, observed cancellation                         | Cancelled turn; result missing                                      |
| Invocation, no result, expired or unavailable activity               | Result not recorded; activity unknown                               |
| Typed `Success` result                                               | Done, including an empty result                                     |
| Typed failure outcome                                                | Failed, preserving denied/timed-out/blocked/unavailable/etc. detail |
| Typed `Cancelled` result                                             | Cancelled                                                           |
| Unknown outcome or ambiguous association                             | Unknown/unpaired, never done                                        |
| User's `Cancel` request without executor acknowledgement             | Stop requested, not cancelled                                       |

A failed call need not fail the turn, and completed calls do not prove a completed
turn.
Use separate run lifecycle and per-call outcome counts.
A terminal model response alone does not prove durable completion.
Cancellation must not relabel already successful calls.
A heartbeat only proves attempt activity, not that every pending tool is
executing or that a particular concurrent branch is running.
Likewise, attempt completion is not proof that every competing execution branch
finished.
Keep that distinction explicit: label attempt activity as such, and derive tool
outcomes only from the matching run's records.
Do not add a branch-liveness service or infer completion from another branch's
final answer.
Missing means absent at this snapshot; a later matching result updates the same
item rather than creating a second one.

The current native tool recorder has **no partial-output/streaming variant**.
Show invocation-only snapshots as awaiting, then update the same item when a
settled result appears.
Do not invent a streaming indicator from clipped or malformed JSON.
Incremental/partial prose still uses the existing message path and is never
hidden.
Verify partial snapshots and incomplete output strings; adding provider/tool
stream transport is outside this change.

## Readable contents, without lossy storage

Use the existing quarantine and `view::content` helpers after decoding JSON.
A compact invocation can show a string-valued `command`, `path`, `query` or
`url`, including its field name; otherwise show a short sanitized preview or
JSON kind/size.
These are selected fields, not a claim that omitted arguments do not exist.
A compact result shows a short text preview, or JSON kind/size for structured
values; it never dumps a large object into the header.
No tool-specific plugin registry is needed.

Expanded bodies reuse the existing readable JSON/text formatting without a
preview-length cap, distinguish an empty result from no result, and retain
meaningful error text.
The existing `/debug` mode shows the original recorded argument/result spelling
when expanded, plus exact provenance; sanitization still applies.
This provides raw-content recovery without a third expansion level or new mode.
Do not decode escape sequences in ordinary text or repair clipped JSON.

Reuse `TranscriptCache`, wrap offsets, viewport windowing and selection's source
mapping.
Register expanded body lines with the correct display indentation so selection
copies contents rather than decoration or soft-wrap newlines.
Selection must not expose quarantined control characters.
Keep rendering free of I/O and avoid formatting hidden full payloads.
Build identity indexes in linear passes over the refreshed evidence, not with a
per-entry scan of all records.
Input-only frames must continue to reuse the wrapped cache.
Activity expiry and changed outcomes invalidate status-bearing cached lines.

## Input and integration

Run and item expansion overrides use stable identities, so live refresh or
interleaved local messages do not move a user's choice to another call.
Transient click target indices may resolve into the current projection, but
stored expansion state must not use mutable entry positions.
Keep existing directive expansion and unrelated controls unchanged.

Emit header targets through the existing pending-click/offset translation.
Both summary rows toggle the run; expanded provenance and body lines do not.
Do not put body lines inside a header hit region, and preserve drag/copy
precedence over a toggle.
The independently developed full-header hit-region change belongs to that
change: this renderer must remain compatible with its `(logical_line, target)`
geometry, not duplicate or absorb it.
Before final integration, recheck that source and test both nested headers and
body exclusions against the actual inherited geometry.

## Bounded implementation and verification

1. Add a TUI-local evidence projection and pinned-snapshot hydration, retaining
   row IDs, exact associations, full recorded output and conservative status.
   Test native writer/readback, content offsets, same IDs in different
   attempts/executions/agents, missing results, unknown outcomes and fallback.
2. Connect the projection to the existing transcript builder and input targets.
   Add two-row summaries, distributed nested items, stable expansion overrides,
   chronological labels, readable/raw details and selection mappings.
   Leave all prose and unrelated audit rendering intact.
3. Add the user-guide reference, conceptual section and walkthrough, including
   a missing-result or unpaired-history recovery example.
   Replace these illustrative layouts with actual rendered acceptance evidence.

The implementation acceptance suite must exercise the real snapshot loader,
transcript builder, `TestBackend`, input handlers and copy path, not just a
parallel mock renderer:

- 2, 48 and 96 calls, before and after results, with equal compact tool height;
  repeated interleaved prose, final answers and concurrent/adjacent runs.
- Parallel calls and out-of-order results; repeated provider IDs separated by
  every run/call-key component; no mispairing, lost payload or duplicate result.
- Pending snapshots, partial prose, malformed/partial JSON, empty results,
  each typed failure class, cancellation request versus acknowledgement,
  completed missing results and expired activity.
- Native persisted history reopened at the same snapshot, old display-only
  history, evidence read failure/retry and no premature terminal-prose display.
- Repeated inner/outer toggles and live refresh, `Ctrl+O`, `/expand`, mouse
  headers/body negatives, drag/copy, scrolling, wrapped lines, Unicode and
  narrow/resized terminals; clipped summaries and expanded full/raw recovery.
- Cache reuse on input-only frames and unchanged source payloads after display.

Retain focused test logs and representative actual buffers, run `just doc build`,
then the normal `nix develop .# -c just fix` and
`nix develop .# -c just nix full` gates on the finished implementation.
Design-prototype checks do not substitute for any of those gates.

## Rejected alternatives

- One collapsed row per tool: height still grows with the run.
- A separate group at every prose boundary: interleaved commentary makes tool
  height grow again; prose is not a reason to split identity.
- Moving all tool details above or below the prose: loses the invocation/result
  chronology unless another event log is introduced.
- Pairing by nearest tool name or provider ID alone: wrong for parallel calls,
  retries and adjacent attempts.
- A persistence migration or new streaming service: unnecessary for identified
  recorded calls and outside this presentation change.
