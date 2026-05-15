# Cost Tracking & Usage

Chaz records token usage and cost on every assistant turn and lets you roll those numbers up across sessions. The data lives in the session databases themselves — there's no separate billing log to keep in sync.

## How It's Captured

Each call to a configured `LLMBackend` returns a `TokenUsage` alongside the model response:

- `prompt_tokens`, `completion_tokens`, `total_tokens`
- `cached_tokens`, `cache_creation_tokens`, `reasoning_tokens` (optional, when the provider reports them)
- `cost_usd` (optional)

For OpenAI-compatible backends, chaz sends `usage: { include: true }` on the request. When chatting through OpenRouter that surfaces `cost_usd` directly — no extra reconciliation pass is needed. Providers that don't return a cost still produce token counts; the rollup distinguishes "no cost data" from "$0.00."

Every assistant `Message` entry written to a session carries a `ResponseMetadata` field containing the model name, an optional provider/response ID, the `TokenUsage`, and any extra wire-format fields the backend chose to retain. Tool calls, tool results, acks, and errors are recorded as separate entry types and excluded from usage rollups — only `Message` entries count, which matches what was actually billed.

## `/costs` (TUI)

From inside the TUI, type:

```text
/costs
```

This walks the user-central session catalog, folds every assistant message's metadata into per-session, per-model, and total counts, and renders a plain-text rollup:

- **Total**: calls, prompt + completion tokens (with cached tokens annotated), and total cost when at least one entry reported one.
- **By model**: per-model call counts and cost, sorted by cost descending.
- **Top sessions**: up to ten sessions with the most cost / activity, with gateway tag, call count, and cost.

Unreadable sessions are logged and skipped rather than failing the whole rollup — the goal is "what we can see right now," not strict correctness.

## `chaz usage` (CLI)

For headless or scripted rollups — say, in a cron job, dashboard, or one-off audit — run the subcommand mode. It opens the eidetica store, computes the same rollup, prints it, and exits without starting any gateway.

```bash
chaz --config config.yaml usage
```

### Flags

| Flag                | Effect                                                                                      |
| ------------------- | ------------------------------------------------------------------------------------------- |
| `--json`            | Emit the rollup as JSON for machine consumption (same shape as the in-memory `UsageRollup`) |
| `--gateway <KIND>`  | Only count sessions originating from this gateway. Values: `cli`, `tui`, `matrix`, `spawn`, `other` |
| `--active-only`     | Skip sessions marked `Closed`                                                                |

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

- **Non-message entries** — `ToolCall`, `ToolResult`, `Ack`, and `Error` entries have no `ResponseMetadata` (no LLM call happened for them).
- **Sessions chaz can't open** — corrupted or otherwise unreadable session DBs are logged via `warn!` and skipped.
- **Costs that the provider didn't report** — token counts still aggregate, but the cost column stays at "no data" rather than silently summing to zero.

## See Also

- [Session Model](../architecture/sessions.md) — where `ResponseMetadata` is persisted and how entries flow through a session.
- [TUI Mode](tui.md) — full TUI command reference.
- [Logging](logging.md) — for tracing-based observability of the underlying LLM calls.
