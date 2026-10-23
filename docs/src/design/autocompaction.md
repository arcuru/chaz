# Automatic Compaction

Chaz's automatic compaction borrows Pi's coding-agent compaction rather than inventing a new strategy.
The reference is Pi at [`936e5f40`](https://github.com/earendil-works/pi/blob/936e5f40aede9327b9142b63506ed2b68bc6b4ac/packages/coding-agent/src/core/compaction/compaction.ts) (`compaction.ts`, `utils.ts` and `docs/compaction.md`).
User-facing behaviour is in [Automatic compaction](../user_guide/extensions.md#automatic-compaction).

## What is taken from Pi

- **Cut point.** Walk back from the newest message accumulating estimated tokens until `keepRecentTokens` (default 20,000) is reached, then cut at the nearest user or assistant message at or after that point; never at a tool result. If the newest tool results alone exceed the target, keep their call message.
- **Split requests.** When the cut lands inside a request that is still running, its completed prefix is summarized separately from older history with Pi's turn-prefix prompt, and the two summaries are kept apart. A preparation makes at most two summary calls, as Pi does.
- **Iterative update.** A later compaction passes the previous summary with Pi's update prompt and sends only messages it did not already cover.
- **Prompts and serialization.** Pi's structured checkpoint and turn-prefix formats and labelled transcript. Chaz also tells the summarizer not to follow instructions inside that transcript.
- **Summary validity.** A response that hit its token cap, errored or asked for a tool is not a checkpoint. The output ceiling is reduced so input and output fit the model's window.

## Where Chaz differs

| Pi                                                                               | Chaz                                                                                                                                      | Why                                                                                                                                    |
| -------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| Triggers at `contextWindow - reserveTokens` (16,384).                            | Triggers at a user-set size, default 80% of the window, never above the request budget.                                                   | Owner-selected `/autocompact <length>` contract.                                                                                       |
| Compacts between turns of the agent loop, blocking.                              | Prepares in the background while tool work continues; applies at the next model call; waits only at the hard request budget.              | Owner-selected background preparation.                                                                                                 |
| Summarizes the split turn's user request into "Original Request".                | Current, queued and retried user input is never summarized; it stays verbatim between the history summary and the request-prefix summary. | Request-preservation contract from the explicit `/compact` and retry work.                                                             |
| Appends a durable `CompactionEntry`; context is rebuilt from `firstKeptEntryId`. | The summary replaces covered messages only in the request copy for the current turn. Durable history is unchanged.                        | Keeps the transcript, checkpoints and tool results authoritative and avoids a new session-global shared state across agents and peers. |
| `keepRecentTokens` is independent of the trigger.                                | The kept tail is capped at half the trigger.                                                                                              | With user-chosen small triggers, an uncapped tail could never let compaction make progress.                                            |
| Retries transient summary stream drops per its retry policy.                     | Zero automatic retries; one preparation in flight.                                                                                        | Owner-selected spend contract.                                                                                                         |
| Overflow or early length stop selects one compact-and-retry.                     | Not implemented; a request over budget is refused before dispatch.                                                                        | Overflow classification is separate work.                                                                                              |

Chaz sends the full bodies of every covered tool result, plus call/result identities, rather than Pi's 2,000-character preview.
A source that cannot fit the summary model's window is refused, not silently shortened.

Pi's own file-operation tracking (`readFiles` / `modifiedFiles`) is not copied: Chaz tools have no uniform file-operation metadata.

## Native and extension responsibilities

The `autocompact` extension owns policy only: enablement, trigger, kept tail and summary ceiling, stored in its per-session settings, plus the `/autocompact` command and status display.
It has no request-editing capability.

The runtime owns source selection and application:

- **Source binding.** A preparation records the exact messages it covers. At application every covered message and its host provenance must still match, and an applied range may only be extended from its own start. The runtime fences against native compact command results (including their source snapshots) and legacy Summary rows at immutable Eidetica snapshots. Competing compaction discards the result and disables further preparation for that turn. Reconstructed or reset source also invalidates an already applied view. Messages appended after the cut are outside coverage and never invalidate it.
- **Safe application.** Results are applied only in the per-call step before a model request, after every dispatched tool in the batch has a result. The authoritative message vector, recorder stream, model sequence, approvals, rate limits and usage accumulator are untouched, so nothing restarts and no tool is replayed. Cache anchors on covered messages are dropped; others follow their message.
- **Progress.** A result must make the estimated request smaller. Failed, stale or non-shrinking results mark their cut as exhausted; a new preparation needs a later cut.
- **Hard budget.** Thresholds and waiting use the complete projected request, not just the unprojected history. Projectors see the budget but defer size validation until compaction can wait; capability, cache and exchange checks still apply. After a wait they rebuild from originals, and the final complete-envelope dispatch gate is mandatory. When the next request is over the request budget, the call waits for the pending preparation (cancellable with the turn). If a summary was rejected at that boundary or nothing is left to summarize, the call fails visibly with `over-budget`.
- **Spend.** Summaries use `BackendManager::complete_bounded`: the turn's own route and model, no tools, one call, with output bounded by both the summary ceiling and the configured backend ceiling. Bounded chat-completions responses that stop on `length` are reported as truncated, so reasoning that exhausts the ceiling cannot produce a silent partial summary. SDK-level retries are disabled for bounded calls as well. Available provider usage is accumulated in the extension's per-agent status for the turn, separate from the reply, including finished preparations that were not needed. Aborting a request or a provider error may leave its usage unknown. Requests use the backend's configured timeout (120 seconds unless overridden), not a new compaction deadline.

## Limits

- Summaries are not persisted. A later turn that is still over the trigger summarizes its history again; persisting a precise-coverage summary across turns and agents needs a coverage and ownership contract for shared sessions.
- Token counts are Chaz's local estimate (`cl100k_base` plus envelope overhead), not the provider's tokenizer. If discovery has no model window, the configured request budget also bounds auxiliary input plus output. Provider usage is shown only for the summary calls themselves.
- Background summary calls share the active turn's authorized backend/model and configured access limits; they do not acquire a separate executor slot.
- This adds no session entry or command wire variant. Existing and rollback readers ignore the opaque extension settings/status stores; native snapshot summaries remain the history-selection boundary.
