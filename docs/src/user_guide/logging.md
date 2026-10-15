# Logging

Chaz uses the [tracing](https://docs.rs/tracing) crate for structured logging. Default verbosity is `info`.

## Where Logs Go

The sink depends on the bridge mode, because the TUI and print modes need to keep stdout clean for their own output:

| Mode               | Default destination                                      |
| ------------------ | -------------------------------------------------------- |
| TUI (default)      | `<state_dir>/chaz-tui.log` (daily-rotated, keeps 7 days) |
| `-p` / `--print`   | `<state_dir>/chaz-cli.log` (daily-rotated, keeps 7 days) |
| `cmd`              | `<state_dir>/chaz-cmd.log` (daily-rotated, keeps 7 days) |
| `daemon`           | stdout (collected by systemd / docker / your supervisor) |
| `chaz usage` (CLI) | stderr (stdout is the rollup output)                     |

Every mode that reserves stdout for its own output — the TUI's screen, a
`-p` reply, a `cmd` result — logs to the rotated file instead. `daemon`
reserves nothing, so it logs to stdout.

Transport bridges are separate processes and log on their own stderr.

`state_dir` comes from `state_dir:` in the config or the platform XDG state dir (typically `~/.local/state/chaz`). The startup banner prints the exact log path for the TUI/print cases. Tail with `tail -f <state_dir>/chaz-tui.log`.

## Startup phase timings

At `info` level, `Startup:` records separate `elapsed_ms` values for selected
agent references, peer defaults, hosted indices, stored secrets, backend
credentials, web search, embedding, security policies, extension dependencies
and server settings. Extension installation and tool registration have their
own timings. Session-registry logs further separate group/peer database setup
and callback installation. `critical_path_ms` measures the whole server build;
`time_to_gateway_ms` also includes connection setup.

For a slow client launch:

1. Find that launch's `Session registry initialized` and `Startup:` records
   in the TUI log. Compare phases from the same launch, not different loads.
2. Check `executor_components=false` on web search, embedding and extension
   installation. These execution phases are skipped for clients, while
   backend credentials and command-provider installation still run.
3. Identify the slow phase before changing configuration. A slow registry
   lookup is not extension installation; rendering readiness also does not
   establish that the separate executor is ready to answer.

A client launch might show these fields (timestamps omitted; timings vary):

```text
Startup: web search configured elapsed_ms=0 executor_components=false
Startup: embedding configured elapsed_ms=0 executor_components=false
Extensions installed elapsed_ms=3 executor_components=false mcp_servers=0
```

The zero execution-phase timings do not mean the whole launch is instant.
If the registry phase dominates, inspect its group/peer database timings
rather than disabling command providers or changing executor credentials.

## Controlling Verbosity

Set the `RUST_LOG` environment variable:

```bash
# Default: info level and above
chaz --config config.yaml

# Debug: detailed operational info (tool results, LLM traffic)
RUST_LOG=debug chaz --config config.yaml

# Errors only
RUST_LOG=error chaz --config config.yaml

# Per-module filtering
RUST_LOG=chaz_core::runtime=debug,chaz_core::security=warn chaz --config config.yaml
```

## What Gets Logged

### info (default)

At the default level you'll see:

- **Startup**: config loaded, agent count, tool registry ready, bridge mode, eidetica sync address
- **Sessions**: new session creation, backfill, bridge callback registration
- **Agent lifecycle**: ReAct loop completion and tool-iteration count
- **Tool execution**: shell commands run, files written, web fetches initiated
- **Security**: approval decisions (approve/deny/approve-all), MCP server restarts
- **Scheduling**: schedule fires, manual triggers

### TUI readiness milestones

Use these separate `info` events to distinguish startup, historical loading and
an explicit creation request:

| Event                                 | Meaning                                                                                     |
| ------------------------------------- | ------------------------------------------------------------------------------------------- |
| `Session registry callback installed` | Registry initialization is complete; `total_elapsed_ms` covers registry setup only          |
| `TUI first frame drawn`               | The loading view is painted before connection/build; includes `first_frame_ms`              |
| `TUI launch ready`                    | Connection/build and any selected-session hydration succeeded; includes `usable_session_ms` |
| `TUI ready frame drawn`               | The ready loop has painted the hub or loaded conversation                                   |
| `TUI session catalog completed`       | A historical catalog attempt finished; includes its generation, elapsed time and success    |
| `TUI new session requested`           | A creation was accepted; includes the action ticket                                         |
| `TUI session composer ready`          | The observed session tab is ready for input; includes ticket, session ID and agent          |

Compare request and composer timestamps for the same ticket. The interval from
startup or gateway readiness to a later user request includes user think time,
so it is not creation latency. A completed catalog attempt may be obsolete;
its generation must still match the hub before its rows can be applied.
The first-frame event does not claim that the backend is ready or that a named
session has finished loading.

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
- Bridge crashes

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

For `daemon` (logs default to stdout), redirect to capture them:

```bash
# Background with log file
RUST_LOG=info nohup chaz --config config.yaml daemon > chaz.log 2>&1 &

# Follow logs
tail -f chaz.log
```

TUI, `-p` / `--print`, and `cmd` modes already log to a daily-rotated file in `state_dir` (see above) — no redirect needed. To follow live, tail that file in another terminal.

## Security Audit Trail

For security auditing, `warn` level captures all enforcement actions. The exact pipeline depends on where the logs land for your bridge:

```bash
# Daemon mode — logs are on stdout/stderr, filter live
RUST_LOG=chaz_core::security=info,chaz_core::runtime=warn chaz --config config.yaml daemon 2>&1 | \
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
