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

## What Doesn't Roll Up

- **Audit entries** — `ToolCall`, `ToolResult`, and `Ack` entries do not add usage. Ordinary errors have no metadata; a stopped turn's error can carry completed-call totals.
- **Sessions chaz can't open** — corrupted or otherwise unreadable session DBs are logged via `warn!` and skipped.
- **Unsupported estimates** — missing prices, future output, hidden reasoning and unknown cache data are not invented; incomplete data stays visibly partial or unknown.

## See Also

- [Session Model](../architecture/sessions.md) — where `ResponseMetadata` is persisted and how entries flow through a session.
- [TUI Mode](tui.md) — full TUI command reference.
- [Logging](logging.md) — for tracing-based observability of the underlying LLM calls.
