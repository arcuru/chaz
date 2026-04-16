# Logging

Chaz uses the [tracing](https://docs.rs/tracing) crate for structured logging. Output goes to stderr in a human-readable format by default.

## Controlling Log Output

Set the `RUST_LOG` environment variable to control verbosity:

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

```bash
# Log to file while keeping TUI clean
RUST_LOG=info chaz --config config.yaml --tui 2> chaz.log

# Background with log file
RUST_LOG=info nohup chaz --config config.yaml > /dev/null 2> chaz.log &

# Follow logs
tail -f chaz.log
```

## Security Audit Trail

For security auditing, `warn` level captures all enforcement actions:

```bash
# Filter to security-relevant events
RUST_LOG=chaz::security=info,chaz::runtime=warn chaz --config config.yaml --tui 2>&1 | grep -E "WARN|denied|blocked|SSRF|leak|approval"
```

Key events to monitor:
- `Secret detected in output` — a tool returned content matching a secret pattern
- `Network request blocked` — SSRF or endpoint policy denial
- `Shell command denied` — command not in allowlist or on denylist
- `Approval decision` — tool approval granted or denied, with tool name
