# Configuration

Chaz is configured via a YAML file passed with `--config`.

## Full Example

```yaml
# Matrix connection (not needed for TUI-only)
homeserver_url: https://matrix.org
username: "chaz"
password: "hunter2"
allow_list: "@user:matrix.org|@other:matrix.org" # Regex

# Persistence
state_dir: "/path/to/state" # Default: $XDG_STATE_HOME/chaz

# LLM backends (OpenAI-compatible)
backends:
  - name: openrouter
    type: openaicompatible
    api_key: "${OPENROUTER_API_KEY}" # Env var reference
    api_base: https://openrouter.ai/api/v1
    models:
      - name: anthropic/claude-sonnet-4
      - name: google/gemini-2.5-pro
  - name: local
    type: openaicompatible
    api_key: "not-needed"
    api_base: http://localhost:11434/v1
    request_timeout: 30 # LLM request timeout in seconds (default: 120)
    max_retries: 5 # Retry attempts for transient errors (default: 3)
    models:
      - name: llama3

# Agent definitions
agents:
  - name: default
    role: chaz
    max_iterations: 10
    allowed_tools: null # null = all tools
    can_spawn: ["researcher", "coder"]
  - name: researcher
    role: researcher
    max_iterations: 20
    allowed_tools: ["web_fetch", "calculate", "get_time", "remember", "recall"]
  - name: coder
    role: coder
    max_iterations: 15
    allowed_tools: ["shell", "read_file", "write_file", "calculate"]

# Roles (system prompts)
roles:
  - name: chaz
    description: Default assistant
    prompt: "You are Chaz, a helpful AI assistant."
  - name: researcher
    description: Research agent
    prompt: "You are a research assistant. Use web_fetch to find information."
  - name: coder
    description: Coding agent
    prompt: "You are a coding assistant. Read and write files, run shell commands."

# Security
security:
  auto_approved_tools:
    - get_time
    - calculate
    - read_file
    - remember
    - recall
  shell_allowlist: ["ls", "cat", "grep", "find", "wc", "head", "tail"]
  shell_denylist: ["rm", "sudo", "chmod", "chown"]
  allowed_endpoints:
    - host: "api.example.com"
      path_prefix: "/v1"
      methods: ["GET"]
  leak_policy: "redact" # "redact" (default) or "block"
  tool_policies:
    shell:
      approval: Always
      rate_limit: 5 # max 5 calls per minute
    web_fetch:
      approval: UnlessAutoApproved
      timeout: 30

# MCP external tools (subprocess JSON-RPC)
mcp_servers:
  - name: filesystem
    command: npx
    args: ["-y", "@modelcontextprotocol/server-filesystem", "/home/user"]
    default_policy:
      risk: medium
      approval: unless_auto_approved
      timeout: 30

# Scheduled tasks
schedules:
  - name: daily-check
    session: daily-standup # Session name or eidetica DB root ID
    task: "Run the daily status check"
    cron: "0 9 * * *"
    enabled: true

# Context window management
context:
  max_context_tokens: 128000
  reserved_output_tokens: 4096
```

## Backends

Each backend requires a `name`, `type`, `api_base`, and optionally `api_key` and `models`.

The `api_key` field supports environment variable references: `"${VAR_NAME}"` or `"$VAR_NAME"`. Keys are resolved at startup and stored in eidetica's SecretStore. They are never included in LLM context.

When multiple backends are defined, model names are prefixed with the backend name (e.g., `openrouter:anthropic/claude-sonnet-4`). With a single backend, no prefix is needed.

### Resilience

All LLM HTTP requests are wrapped in a configurable timeout and retried on transient failures:

| Field             | Default | Description                                                       |
| ----------------- | ------- | ----------------------------------------------------------------- |
| `request_timeout` | `120`   | Seconds before an LLM request times out                           |
| `max_retries`     | `3`     | Retry attempts for transient errors (429, 5xx, timeouts, network) |

Retries use exponential backoff (1s, 2s, 4s, … capped at 30s). Rate-limit responses (HTTP 429) with a `Retry-After` header are honored as the minimum delay. Non-retryable errors (401, 403, 400) fail immediately.

## Agents

Agent definitions control which tools an agent can use, which other agents it can spawn, and its system prompt. See [Agents](agents.md) for details.

## Security

Security settings control tool approval, network access, shell sandboxing, secret leak detection, and tool rate limiting. See [Security](security.md) for details.

## MCP Servers

External tools via the Model Context Protocol. See [MCP External Tools](mcp.md) for details.

## Schedules

Cron-driven task injection into sessions. Each schedule writes a `Directive` entry to the target session on a cron schedule. Sessions are referenced by name or eidetica DB root ID. Responses from scheduled runs are delivered to every Matrix room attached to that session (see [Matrix: channel attachment](matrix.md#session-attachment)).

## Context

Token budgeting for the LLM context window. Uses tiktoken (cl100k_base) for accurate token counting. `max_context_tokens` sets the total budget, `reserved_output_tokens` is subtracted for the LLM's response. Per-agent overrides via `max_context_tokens` on agent definitions.

## State Directory

Chaz persists all data in the state directory:

- `eidetica.db` — SQLite database containing all sessions, memory, secrets, and registry
- Headjack session data (Matrix sync token, device keys)

The state directory defaults to `$XDG_STATE_HOME/chaz`. Override with `state_dir` in config.
