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

# Web search tool (ordered failover chain)
web_search:
  backends:
    - type: tavily
      api_key: "${TAVILY_API_KEY}"
    - type: brave
      api_key: "${BRAVE_API_KEY}"
    - type: duckduckgo

# Optional: expose eidetica sync on an HTTP port alongside the default iroh P2P transport.
# Useful for debugging or when the remote peer doesn't have iroh connectivity.
# Omit this field to use iroh P2P only (stable peer identity, no address needed).
# sync_listen: "0.0.0.0:8765"

# Optional: embedding backend for semantic memory recall.
# Without this section, recall uses BM25 lexical ranking only.
embedding:
  backend: openai
  model: text-embedding-3-small
  api_key: "${OPENAI_API_KEY}"
```

## Web search

The `web_search` tool is always registered. Backends are an **ordered preference list**: the tool tries each entry in turn and falls through on any error, returning the first success. Empty results are a legitimate answer and do **not** trigger failover.

Omit the `web_search:` section entirely to use DuckDuckGo alone (keyless).

| Backend      | Endpoint                                      | Auth                   | Notes                                                             |
| ------------ | --------------------------------------------- | ---------------------- | ----------------------------------------------------------------- |
| `kagi`       | `https://kagi.com/api/v0/search`              | `Authorization: Bot …` | Invite-only beta; separate API billing from Search                |
| `tavily`     | `https://api.tavily.com/search`               | API key in JSON body   | Results tuned for LLM consumption                                 |
| `brave`      | `https://api.search.brave.com/res/v1/web/...` | `X-Subscription-Token` | Brave Search API                                                  |
| `serper`     | `https://google.serper.dev/search`            | `X-API-KEY`            | Google SERP as JSON                                               |
| `searxng`    | `<url>/search?format=json`                    | none                   | Requires `url:` on the entry; JSON output enabled on the instance |
| `duckduckgo` | `https://html.duckduckgo.com/html/`           | none                   | HTML scrape; keyless fallback                                     |

### SearxNG

`searxng` entries take a `url:` instead of an `api_key:`. The URL is the instance root — `/search?q=<query>&format=json` is appended.

```yaml
web_search:
  backends:
    - type: searxng
      url: "http://localhost:8888" # self-hosted docker container
    - type: duckduckgo # safety net if the instance is down
```

Self-hosting is ~5 minutes with the official docker-compose stack at https://docs.searxng.org/admin/installation-docker.html and doesn't require opening any ports externally.

Public instances at https://searx.space work but come and go, and many rate-limit aggressively or disable the JSON output format. If you see `searxng returned HTTP 403` / `HTTP 429` in the logs, either self-host or pick a different instance. JSON must be enabled server-side in the instance's `settings.yml` (`search.formats: [html, json]`).

API keys accept the same `${VAR}` / `$VAR` environment substitution as LLM backend keys and are stored in the SecretStore at startup. Entries with a missing or unresolvable `api_key` on a keyed backend are skipped at startup with a warning; if the resulting list is empty, a single DuckDuckGo entry is added so the tool always has a fallback.

**Recommended pattern**: put your highest-quality paid backend first, a cheaper or alternative backend second for rate-limit resilience, and `duckduckgo` last as a keyless safety net.

## Embeddings

The `embedding:` block configures semantic recall for the [memory tools](memory.md). Omit it to keep recall lexical-only (BM25); add it to enable hybrid lexical + semantic ranking via Reciprocal Rank Fusion. See [Searching memory](memory.md#searching-memory) for ranking details.

```yaml
embedding:
  backend: openai
  model: text-embedding-3-small
  api_key: "${OPENAI_API_KEY}"
```

| Field      | Required | Default                           | Notes                                                                                                                                                                                             |
| ---------- | -------- | --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `backend`  | no       | `openai`                          | Provider kind. Currently only `openai` (any OpenAI-compatible `/v1/embeddings` endpoint).                                                                                                         |
| `model`    | yes      | —                                 | Model name as the API expects (e.g. `text-embedding-3-small`, `nomic-embed-text`).                                                                                                                |
| `provider` | no       | matches `backend` (e.g. `openai`) | Tag used to namespace the model in the storage subtree name (`embeddings:<provider>/<model>`). Override when pointing at a self-hosted endpoint so the subtree distinguishes it from real OpenAI. |
| `api_base` | no       | `https://api.openai.com/v1`       | Override for OpenAI-compatible endpoints (Ollama, LM Studio, Together, etc.).                                                                                                                     |
| `api_key`  | yes      | —                                 | API key. Same `${VAR}` / `$VAR` env substitution as LLM backend keys; stored in the SecretStore at startup.                                                                                       |

### Self-hosted (e.g. Ollama)

```yaml
embedding:
  backend: openai
  provider: ollama # so the subtree is "embeddings:ollama/nomic-embed-text", not "openai/..."
  model: nomic-embed-text
  api_base: http://localhost:11434/v1
  api_key: "ignored" # Ollama doesn't check the key but reqwest needs something
```

### Switching models

Each model writes to its own subtree (`embeddings:<provider>/<model>`); subtrees coexist on the same DB and sync together. Switching models leaves the old subtree dormant — entries written under the old model stop contributing to recall until they're reindexed (planned: `/memory reindex`). Until then, recall against the new model uses semantic only on the entries written since the switch, plus BM25 on everything.

### Failure handling

The embedder is best-effort:

- API down on **write** → memory is still stored, the vector is skipped, a `warn` is logged.
- API down on **recall** → query falls back to BM25 only.
- DB has no rows under the active model's subtree → BM25 only.

Configuring an embedder never makes recall worse than the lexical baseline.

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

- `eidetica.db` — SQLite database backing every chaz eidetica DB: per-session DBs, per-agent DBs, per-bank DBs, plus the peer-local `chaz_group` (sessions/channels/names) and `chaz_peer` (credentials/heartbeat_last_fired/schedule_state) bookkeeping DBs
- Headjack session data (Matrix sync token, device keys)

The state directory defaults to `$XDG_STATE_HOME/chaz`. Override with `state_dir` in config.
