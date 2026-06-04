# Configuration

Chaz is configured via a YAML file passed with `--config`.

## Full Example

```yaml
# Matrix connection (not needed for TUI-only or CLI-only modes)
homeserver_url: https://matrix.org
username: "chaz"
password: "hunter2" # If unset, prompted on first run
allow_list: "@user:matrix.org|@other:matrix.org" # Regex matched against the sender's Matrix ID
# message_limit: 500           # Optional: per-account message cap while the bot runs
# room_size_limit: 100         # Optional: refuse to respond in rooms with more than N members
# chat_summary_model: "gpt-4o-mini"  # Optional: separate model for chat summarization

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

# Agent definitions. Each agent's system prompt comes from `system_prompt:`
# (inline string) and/or `system_prompt_files:` (file paths whose contents
# are concatenated, then the inline string is appended).
#
# YAML `agents:` is a *first-boot template only* — the per-agent eidetica
# DB (AgentDb) is the runtime source of truth afterwards. Use `/agent set`
# (TUI) or `!chaz agent set` (Matrix) to edit a live agent. See
# `user_guide/agents.md`.
agents:
  - name: chaz
    system_prompt: "You are Chaz, a helpful AI assistant."
    max_iterations: 10
    tools: null # null = all tools
    # Workers — configured one-shot LLM calls owned by this Agent.
    # Each is invocable via `spawn_worker(name=…)` from this Agent only.
    workers:
      - name: researcher
        system_prompt: "You are a research assistant. Use web_fetch to find information."
        max_iterations: 20
        tools: ["web_fetch", "calculate", "get_time"]
      - name: coder
        # Pull repo-level instructions from a file; layer inline guidance on top.
        system_prompt_files: ["~/code/myproject/AGENTS.md"]
        system_prompt: "Edit files in-place; never rewrite from scratch."
        max_iterations: 15
        tools: ["shell", "read_file", "write_file", "calculate"]
    # Auto-attach memory/skill banks at agent bootstrap. Missing banks are
    # warned and skipped; default_memory_banks auto-creates missing banks.
    default_memory_banks: ["chaz-notes"]
    # default_skill_banks: ["coding-skills"]
    # Allow this agent to run without user input (scheduled wakes)
    # autonomous: false

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
      approval: always # never | unless_auto_approved | always
      rate_limit: 5 # max 5 calls per minute (omit = unlimited)
    web_fetch:
      approval: unless_auto_approved
      timeout: 30 # seconds (default 60)

# MCP external tools. Stdio subprocess transport when `command` is set;
# Streamable HTTP transport when `url` is set. See `user_guide/mcp.md`.
mcp_servers:
  - name: filesystem
    command: npx
    args: ["-y", "@modelcontextprotocol/server-filesystem", "/home/user"]
    env:
      NODE_ENV: production
    default_policy:
      risk: medium
      approval: unless_auto_approved
      timeout: 30
  # - name: remote-mcp
  #   url: "http://localhost:8080/mcp"

# Optional: scan a directory for additional MCP server manifest files
# (.yaml/.json), one server per file. Merged with `mcp_servers` above.
# mcp_server_dir: "/etc/chaz/mcp.d"

# Optional: named tool profiles control how the LLM sees tool definitions.
# Reference one from an agent definition with `tool_profile: brief`.
# tool_profiles:
#   brief:
#     default: full      # full | brief | summary | hidden
#     tools:
#       "filesystem.*": summary
#       shell: full

# Scheduled tasks — imported at startup as agent-owned schedules in the
# owning agent's DB (Pinned to the resolved session), fired by the same
# RoutineEngine as the /schedule command. Cron is 6 fields:
# sec min hour day-of-month month day-of-week. Idempotent by `name`
# within the owning agent.
schedules:
  - name: daily-check
    session: daily-standup # Session name or eidetica DB root ID (Pinned target)
    agent: researcher # Owning agent (display name or DB id). Omit → peer's default agent.
    task: "Run the daily status check" # Wake prompt handed to the agent
    cron: "0 0 9 * * *" # 09:00:00 every day
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

# Extension capabilities — operator-level scoping.
#
# agent_state_allowlist: per-extension agent allowlists for the
# AgentStateAdmin cap. Maps extension name → agent display names.
# An absent entry means unrestricted; an empty list means deny-all
# (logged at WARN on startup — to a tool a scoped-out agent is
# reported identically to a non-existent one).
# agent_state_allowlist:
#   schedule: [chaz, bash]
#   memory: [chaz]             # memory tools can only touch chaz agent

# Multi-agent chat-room tuning. Omit for built-in defaults.
# multi_agent:
#   burst_budget: 6            # max consecutive agent→agent turns before
#                              # the chain is suppressed (until a human or
#                              # schedule speaks). Inspect at runtime with
#                              # /agent room.

# Optional: embedding backend for semantic memory recall.
# Without this section, recall uses BM25 lexical ranking only.
embedding:
  backend: openai
  model: text-embedding-3-small
  api_key: "${OPENAI_API_KEY}"
# Single-shot print mode (-p / --print) cannot prompt for tool approval
# interactively, so a curated allowlist of tools is auto-approved instead.
# Defaults to [shell, write_file]. Override or extend with your own list.
# cli:
#   auto_approved_tools: [shell, write_file, web_fetch]
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

Each `agents:` entry seeds an Agent DB on first boot; subsequent edits live in the DB, not the YAML. See [Agents](agents.md) for the runtime model and live-edit commands. Per-agent fields:

| Field                  | Type                   | Notes                                                                                                  |
| ---------------------- | ---------------------- | ------------------------------------------------------------------------------------------------------ |
| `name`                 | string                 | Required. Display name; also the lookup key for `default_agents`, `/agent add`, schedules.             |
| `system_prompt`        | string                 | Inline system prompt. Appended after `system_prompt_files` content when both are set.                  |
| `system_prompt_files`  | list of paths          | File contents concatenated into the system prompt. `~` expansion supported.                            |
| `model`                | string                 | Default model (e.g. `openrouter:anthropic/claude-sonnet-4`). Backend prefix required when ambiguous.   |
| `tools`                | list of tool names     | Whitelist. `null` (omitted) means "all tools". Supports `namespace__*` globs for MCP tools.            |
| `workers`              | list of WorkerConfig   | Per-Agent Worker templates invocable via `spawn_worker`. See "Worker fields" below.                    |
| `max_iterations`       | int                    | ReAct loop cap. Default 10.                                                                            |
| `autonomous`           | bool                   | Allow firing without an inbound human message (scheduler wakes). Default `false`.                      |
| `max_context_tokens`   | int                    | Per-agent override of `context.max_context_tokens`.                                                    |
| `tool_profile`         | string                 | References a key in top-level `tool_profiles`.                                                         |
| `grants`               | map<tool, Grants>      | Per-tool grant overrides (shell allow/deny, network endpoints, fs paths). Merged per-kind over policy. |
| `default_memory_banks` | list of bank names     | Auto-attached at first boot. Missing banks are auto-created.                                           |
| `default_skill_banks`  | list of bank names     | Auto-attached at first boot. Missing banks are warned and skipped.                                     |
| `presets`              | map<name, AgentPreset> | Named override bundles selectable at spawn time (model / iters / tools / role suffix / tool_profile).  |

### Worker fields

A Worker is a configured one-shot LLM call declared under an Agent's
`workers:` list. Workers have no identity, no keys, and no persistent
state of their own — entries written during a Worker invocation are
signed by the parent Agent's key. Lookup is per-Agent; Ava's
`researcher` is distinct from Chaz's `researcher`.

| Field                 | Type                   | Notes                                                                                              |
| --------------------- | ---------------------- | -------------------------------------------------------------------------------------------------- |
| `name`                | string                 | Required. Unique within the parent Agent's `workers:` list. Used as `spawn_worker(name=…)`.        |
| `system_prompt`       | string                 | Worker's system prompt. Falls back to (inherits) the parent Agent's prompt when omitted.           |
| `system_prompt_files` | list of paths          | Concatenated into the Worker prompt. `~` expansion supported.                                      |
| `model`               | string                 | Override the model. Falls back to the parent Agent's `model`.                                      |
| `tools`               | list of tool names     | Narrows the parent Agent's tool list. May include other Worker names; recursion bounded by depth.  |
| `max_iterations`      | int                    | Configured per-Worker for completeness, but **ignored when invoked under a parent Agent's iteration budget** — nested Workers share the top-level Agent's pool rather than getting a fresh quota. The field still applies when a Worker is invoked outside a running budget (rare; primarily testing paths). |
| `presets`             | map<name, AgentPreset> | Selectable via the `preset` arg of `spawn_worker`.                                                 |

### `default_agents`

Names of agents auto-attached to every freshly-created session, in
order. The first entry effectively becomes the routing host — when an
incoming message has no `@mention` and no explicit `/agent host`, the
resolution chain picks the first authorized agent on the session.

```yaml
agents:
  - name: ava
  - name: researcher

default_agents: [ava, researcher]
```

Each name must match an entry in `agents:`. Names that don't have a
hosted Agent DB yet (configured in YAML but not yet bootstrapped) are
skipped silently with a debug log — the rest still attach. Per-agent
attach failures are logged but don't unwind the rest, so session
creation never fails because of `default_agents`.

Absent or empty `default_agents:` falls back to the legacy
single-default behavior: attach only `agents.first()`. Existing
sessions are unaffected — auto-attach is a creation-time mechanism, so
sessions created before this field was set keep their current
participant list. Bring them up to date by re-running `/agent add
<name>` manually.

## Security

Security settings control tool approval, network access, shell sandboxing, secret leak detection, and tool rate limiting. See [Security](security.md) for details.

## MCP Servers

External tools via the Model Context Protocol. See [MCP External Tools](mcp.md) for details.

## Schedules

Cron-driven agent wakes. Each entry is imported at startup as an **agent-owned schedule** (see [Agents — Schedules](agents.md#schedules)) in the owning agent's DB, Pinned to the resolved session. On fire, the owning agent's turn runs directly with `task` as the wake prompt — no `Directive` is written to the session. `agent:` names the owner (display name or DB id); omit it to use the peer's default agent. `session:` is referenced by name or eidetica DB root ID. Responses are delivered to every Matrix room attached to that session (see [Matrix: channel attachment](matrix.md#session-attachment)).

Optional lifecycle bounds mirror the `schedule_add` tool: `max_fires:` (retire after N fires) and `expires_at:` (RFC 3339 instant after which it stops). Whichever is hit first retires the schedule; omit both for an unbounded cron. Example — wake hourly for a day then stop:

```yaml
schedules:
  - name: hourly-standup-ping
    agent: chaz
    session: ops
    cron: "0 0 * * * *"
    task: "Post the hourly status line."
    max_fires: 24
    # or: expires_at: "2026-06-01T00:00:00Z"
```

## Context

Token budgeting for the LLM context window. Uses tiktoken (cl100k_base) for accurate token counting. `max_context_tokens` sets the total budget, `reserved_output_tokens` is subtracted for the LLM's response. Per-agent overrides via `max_context_tokens` on agent definitions.

## Multi-agent rooms

Tuning for chat-room sessions where multiple agents are active. Currently a single knob:

| Field          | Default | Notes                                                                                                                                                                                                                                                                                                                   |
| -------------- | ------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `burst_budget` | `6`     | Maximum length of an agent→agent message chain since the last human or `Directive`. Once exceeded, further mention-chained agent wakes are suppressed until a human (or schedule) speaks. Runaway backstop. Inspect at runtime with `/agent room`. See [`design/autonomous_agents.md`](../design/autonomous_agents.md). |

```yaml
multi_agent:
  burst_budget: 6
```

## Extensions

Operator-level scoping for the extension framework. See [Extensions](extensions.md) for the per-extension reference.

`agent_state_allowlist:` maps each extension name to the list of agent display names whose runtime state that extension is allowed to read/mutate via the `AgentStateAdmin` capability. An absent entry is unrestricted (all hosted agents visible); an empty list (`[]`) denies the cap entirely (logged at WARN at startup — to a tool a scoped-out agent looks identical to a non-existent one).

```yaml
agent_state_allowlist:
  schedule: [chaz, bash]
  memory: [chaz]
```

## Print mode

`chaz -p "<prompt>"` (alias `--print`) runs a single ReAct turn and exits. There is no interactive approval surface, so a small set of tools must be pre-approved to make the mode useful. Defaults to `[shell, write_file]`. Override per-deployment:

```yaml
cli:
  auto_approved_tools: [shell, write_file, web_fetch]
```

`-p --session NAME` reuses a named session across invocations (find-or-create). Without `--session` each invocation creates a fresh ephemeral session.

## State Directory

Chaz persists all data in the state directory:

- `eidetica.db` — SQLite database backing every chaz eidetica DB: per-session DBs, per-agent DBs, per-bank DBs, plus the peer-local `chaz_group` (sessions/channels/names) and `chaz_peer` (credentials/schedule_last_fired/schedule_state) bookkeeping DBs
- Headjack session data (Matrix sync token, device keys)

The state directory defaults to `$XDG_STATE_HOME/chaz`. Override with `state_dir` in config.
