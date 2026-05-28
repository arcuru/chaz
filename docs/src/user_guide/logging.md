# Logging

Chaz uses the [tracing](https://docs.rs/tracing) crate for structured logging. Default verbosity is `info`.

## Where Logs Go

The sink depends on the gateway mode, because the TUI and `--cli` modes need to keep stdout clean for their own output:

| Mode               | Default destination                                      |
| ------------------ | -------------------------------------------------------- |
| Matrix (default)   | stdout (collected by systemd / docker / your supervisor) |
| `--tui`            | `<state_dir>/chaz-tui.log` (daily-rotated, keeps 7 days) |
| `--cli`            | `<state_dir>/chaz-cli.log` (daily-rotated, keeps 7 days) |
| `chaz usage` (CLI) | stderr (stdout is the rollup output)                     |

`state_dir` comes from `state_dir:` in the config or the platform XDG state dir (typically `~/.local/state/chaz`). The startup banner prints the exact log path for the TUI/CLI cases. Tail with `tail -f <state_dir>/chaz-tui.log`.

## Controlling Verbosity

Set the `RUST_LOG` environment variable:

```bash
# Default: info level and above
chaz --config config.yaml --tui

# Debug: detailed operational info (tool results, LLM traffic)
RUST_LOG=debug chaz --config config.yaml --tui

# Errors only
RUST_LOG=error chaz --config config.yaml --tui

# Per-module filtering
RUST_LOG=chaz::runtime=debug,chaz::security=warn chaz --config config.yaml --tui
```

## What Gets Logged

### info (default)

At the default level you'll see:

- **Startup**: config loaded, agent count, tool registry ready, gateway mode, eidetica sync address
- **Sessions**: new session creation, backfill, gateway callback registration
- **Agent lifecycle**: ReAct loop completion, max iterations reached, tool-aware fallback
- **Tool execution**: shell commands run, files written, web fetches initiated
- **Security**: approval decisions (approve/deny/approve-all), MCP server restarts
- **Scheduling**: schedule fires, manual triggers

### warn

Warnings indicate degraded or blocked operations:

- **Secret leaks**: detected patterns and action taken (redact or block)
- **Network policy**: SSRF blocks (private IPs, internal hostnames), endpoint denials
- **Shell policy**: commands denied by allowlist/denylist
- **Tool issues**: execution errors, timeouts, unknown tools, rate limiting
- **Injection**: prompt injection patterns detected in tool output
- **Approval**: channel failures (auto-deny)

### error

Errors indicate failures needing attention:

- Matrix login or sync failures
- Session database errors (load, persist, commit)
- Agent execution failures
- Gateway crashes

### debug

Verbose output useful for development and troubleshooting:

- LLM request/response details (model, message count, finish reason)
- Individual tool results with content preview
- Model resolution (requested vs resolved, backend selected)
- File read operations with byte counts
- Memory store/recall operations
- HTTP response status and body size
- Shell command exit codes
- MCP JSON-RPC message traffic

## Redirecting to a File

For Matrix mode (logs default to stdout), redirect to capture them:

```bash
# Background with log file
RUST_LOG=info nohup chaz --config config.yaml > chaz.log 2>&1 &

# Follow logs
tail -f chaz.log
```

TUI and `--cli` modes already log to a daily-rotated file in `state_dir` (see above) — no redirect needed. To follow live, tail that file in another terminal.

## Security Audit Trail

For security auditing, `warn` level captures all enforcement actions. The exact pipeline depends on where the logs land for your gateway:

```bash
# Matrix mode — logs are on stdout/stderr, filter live
RUST_LOG=chaz::security=info,chaz::runtime=warn chaz --config config.yaml 2>&1 | \
  grep -E "WARN|denied|blocked|SSRF|leak|Approval"

# TUI / CLI mode — logs are in a rolling file, tail and filter
tail -F ~/.local/state/chaz/chaz-tui.log | \
  grep -E "WARN|denied|blocked|SSRF|leak|Approval"
```

Key events to monitor:

- `Secret detected in output` — a tool returned content matching a secret pattern
- `Network request blocked` — SSRF or endpoint policy denial
- `Shell command denied` — command not in allowlist or on denylist
- `Approval decision` — tool approval granted or denied, with tool name
