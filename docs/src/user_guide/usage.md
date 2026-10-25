# Cost Tracking & Usage

Chaz records token usage and cost on every assistant turn and lets you roll those numbers up across sessions. The data lives in the session databases themselves — there's no separate billing log to keep in sync.

## How It's Captured

Each call to a configured `LLMBackend` returns a `TokenUsage` alongside the model response:

- `prompt_tokens`, `completion_tokens`, `total_tokens`
- `cached_tokens`, `cache_creation_tokens`, `reasoning_tokens` (optional, when the provider reports them)
- `cost_usd` (optional)

For OpenAI-compatible backends, chaz sends `usage: { include: true }` on the request. When chatting through OpenRouter that surfaces `cost_usd` directly — no extra reconciliation pass is needed. Providers that don't return a cost still produce token counts; the rollup distinguishes "no cost data" from "$0.00."

Assistant `Message` entries carry optional `ResponseMetadata` containing the returned model name, an optional provider/response ID, turn-total `TokenUsage`, and extra wire-format fields the backend retained.
A stopped turn's `Error` also carries its completed-call totals.
The status bar, `/costs`, `chaz usage`, and the `chaz usage <period>` tool query
read one deduplicated session snapshot: per-call observations first, older
identified transcripts next, and compatible legacy turn summaries last.
Tool-call/result and ack entries do not add usage. A completed call remains
visible even if a scheduled turn ends with `no_reply` or a later call fails.
The last-call input size is separate from cumulative usage.

## Reported, estimated and unknown

Chaz keeps provider reports separate from local fallback estimates.
It estimates the outgoing request after context projection using its existing
token counter, and estimates output only from returned text and tool-call names
and arguments. Hidden reasoning and future output remain unknown. These counts
can differ from a provider's tokenizer or billing rules.

| Display or JSON field                                     | Meaning                                                                                                 |
| --------------------------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| `~` in any view                                           | Includes a local fallback estimate, not provider-reported billing                                       |
| `partial`                                                 | At least one observed call lacks data, or only part of its cost is estimable                            |
| Existing `prompt_tokens`, `completion_tokens`, `cost_usd` | Reported-only totals, unchanged JSON meaning                                                            |
| `estimated`                                               | Separate fallback `prompt_tokens`, `completion_tokens`, `cost_usd` totals                               |
| `coverage`                                                | Call counts with prompt/completion/cost reports or estimates, cache reports, and partial cost estimates |
| `cost_reported`                                           | At least one provider reported cost, including an explicit zero                                         |

Prices come from configured or already cached model information, in USD per
million tokens. Reads never fetch a pricing catalog. Unknown prices stay unknown;
known cache reads require a known cache-read price. Chaz never estimates cache
hits or cache writes. Without cache information, an input-cost estimate uses the
uncached rate and is marked approximate. No cache-write price is invented.

Each logical call has one presentation snapshot that is replaced when a report
arrives. Its identity includes the invocation, optional execution branch and
model-call sequence. This is not a billing log, replayable context, an execution
lease, a completion marker or budget-enforcement authority. Scheduled invocations
do not acquire durable request attempts or restart recovery through telemetry.
Only counts and provenance are stored here, not private schedule prompts.

A legacy summary remains one summary observation; it cannot reconstruct an old
per-model tool loop or prove that its provider supplied every field. Old numeric
fields retain their historical reported semantics. New OpenAI-compatible and
Anthropic responses distinguish omitted fields from explicitly reported zero.
The inclusive `since` filter uses the call's request-observation timestamp for
new calls, the transcript timestamp for old calls, and the entry timestamp for
legacy summaries. A filtered-out call does not reappear as a final summary.

### Watching a fallback get replaced

1. Send a tool-using prompt. While its first model request waits, a 1,000-token
   local count might show `~1000 (0 reported + ~1000) prompt` in `/costs`.
   If only input has a known $2/million price, cost shows
   `~$0.0020 ($0.0000 reported + ~$0.0020) partial`.
   Output is unknown, not a prediction.
2. The provider reports 900 input tokens, 20 output tokens and `$0.0030`.
   The same call becomes `900 prompt + 20 completion | $0.0030`.
   The local count and cost are replaced, not added. JSON now has
   `prompt_tokens: 900`, `estimated.prompt_tokens: 0` and
   `coverage.prompt_reported: 1`.
3. A later call fails or omits cost. The first report remains and the rollup
   marks the remaining coverage `partial`; missing prices remain unknown.
   Reopen the session or run `chaz usage --json` to read the same evidence.
   This is an accounting observation, not a provider invoice.

## `/costs` (TUI)

From inside the TUI, type:

```text
/costs
```

This walks the user-central session catalog, folds the same deduplicated call evidence into per-session, per-model, and total counts, and renders a plain-text rollup:

- **Total**: calls, prompt + completion tokens (with cached tokens annotated), and separately labeled reported/estimated cost with coverage.
- **By model**: per-model call counts and cost, sorted by cost descending.
- **Top sessions**: up to ten sessions with the most cost / activity, with gateway tag, call count, and cost.

Unreadable sessions are logged and skipped rather than failing the whole rollup — the goal is "what we can see right now," not strict correctness.

## `chaz usage` (CLI)

For headless or scripted rollups — say, in a cron job, dashboard, or one-off audit — run the subcommand mode. It opens the eidetica store, computes the same rollup, prints it, and exits without starting any bridge.

```bash
chaz --config config.yaml usage
```

### Flags

| Flag               | Effect                                                                                              |
| ------------------ | --------------------------------------------------------------------------------------------------- |
| `--json`           | Emit the rollup as JSON for machine consumption (same shape as the in-memory `UsageRollup`)         |
| `--gateway <KIND>` | Only count sessions originating from this gateway. Values: `cli`, `tui`, `matrix`, `spawn`, `other` |
| `--active-only`    | Skip sessions marked `Closed`                                                                       |

### Examples

```bash
# Plain-text rollup across every session
chaz --config config.yaml usage

# JSON, suitable for piping into jq
chaz --config config.yaml usage --json | jq '.total'

# Spend attributable to autonomous CLI invocations only
chaz --config config.yaml usage --gateway cli

# Ignore sessions that have been explicitly closed
chaz --config config.yaml usage --active-only
```

The JSON output preserves the per-session and per-model breakdowns, the applied filter, and a `cost_reported` flag that lets downstream consumers distinguish "we know the cost was zero" from "the backend never reported one."

## Context-window snapshots (`/context`)

`/context` (TUI) or `!chaz context` (Matrix) answers "how full is this agent's context window?" for the agent responding in the current session, and hands the same answer to that agent's next turn, so you can run it and then ask the agent about it.

### A snapshot, not a live meter

- **Last completed request** is measured, not estimated. It is the provider's input-token count for the final request of the agent's latest measured reply in this session, not the turn's summed `prompt_tokens`. The TUI's live meter can also show estimates and in-flight calls; `/context` does not.
- **Next request** is never measured in advance. The report says how many transcript entries (messages, directives, summaries) were added since the measurement; the snapshot itself also adds its own length.
- **Context budget** is what the runtime targets for the agent's next turn on this peer, with its source:
  - `model catalog window` — the model's known window;
  - `explicit agent cap (max_context_tokens)` — the agent's cap is lower than the window, or the window is unknown;
  - `fallback: configured max_context_tokens default` — neither is known.
- **Reserved for the reply** is the `reserved_output_tokens` share of the budget the context builder keeps free.
- **Used** shows the numerator and denominator explicitly against the budget, and against the model window too when they differ.
- Anything chaz does not know is reported as `unknown`.

The command records the report on the session DB (the `context_reports` store, one entry per agent). Running it again replaces that agent's pending snapshot; other agents' snapshots are unchanged. On the agent's next turn in this session, the `context` extension appends it after the conversation, labelled as diagnostic data the user requested rather than instructions, and records that it was delivered (`context_report_deliveries`). Later turns don't repeat it. Running `/context` itself does not wake the agent or call a model. Because both stores live on the session DB, the snapshot reaches the turn even when the command ran in a TUI service client or on a Matrix bridge peer. The budget, though, is resolved on the peer that ran the command.

Failure modes:

- **No measurement yet.** A new session, an agent that hasn't replied, or a provider that reports no usage gives `Last completed request: unknown` and `Used: unknown`.
- **Another turn consumes the snapshot first.** It goes to the agent's next turn, whatever woke it (for example a schedule fire). Run `/context` again.
- **Turn failed.** Delivery is recorded during context assembly. If that request later fails, the snapshot is not re-sent. Run `/context` again.
- **Recording failed.** A report followed by `Not shared with …: failed to record the snapshot` was shown to you but not saved for delivery. Check the connection and existing session write permission before trying again.

| Surface | Command         | Shown to                                  |
| ------- | --------------- | ----------------------------------------- |
| TUI     | `/context`      | You, plus the current agent's next turn   |
| Matrix  | `!chaz context` | The room, plus the room agent's next turn |

`/extensions remove context` disables both the command and delivery for a session. A pending snapshot waits until `/extensions add context` enables it again. Per-agent opt-outs also suppress delivery.

### Walkthrough: checking headroom before a long task

1. After a few turns with `chaz`, run `/context`:

   ```text
   Context for agent 'chaz' (snapshot 2027-01-15T08:02:05Z)
   Last completed request: 45,210 input tokens, measured 2027-01-15T08:00:05Z (2m ago) on test-model
   Next request: not yet measured; 0 transcript entries added since the measurement
   Context budget: 200,000 tokens for test-model — model catalog window; 4,096 reserved for the reply
   Model window: 200,000 tokens
   Used: 45,210 / 200,000 = 22.6% of budget

   (chaz will see this snapshot on its next turn in this session.)
   ```

2. Ask `chaz` "Given /context, can you read the whole log file in one go?". That request includes the snapshot after the conversation, alongside any other enabled context tails.
3. Ask a follow-up. The snapshot is not attached again; the agent sees only its earlier answer.
4. Failure path: in a new session where the agent hasn't replied yet, `/context` shows

   ```text
   Last completed request: unknown (no measured reply from this agent in this session)
   Next request: not yet measured
   ...
   Used: unknown
   ```

   Send one message, then run `/context` again to get a measured snapshot.

## What Doesn't Roll Up

- **Audit entries** — `ToolCall`, `ToolResult`, and `Ack` entries do not add usage. Ordinary errors have no metadata; a stopped turn's error can carry completed-call totals.
- **Sessions chaz can't open** — corrupted or otherwise unreadable session DBs are logged via `warn!` and skipped.
- **Unsupported estimates** — missing prices, future output, hidden reasoning and unknown cache data are not invented; incomplete data stays visibly partial or unknown.

## See Also

- [Session Model](../architecture/sessions.md) — where `ResponseMetadata` is persisted and how entries flow through a session.
- [TUI Mode](tui.md) — full TUI command reference.
- [Logging](logging.md) — for tracing-based observability of the underlying LLM calls.
