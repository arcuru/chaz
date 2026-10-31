# Configuration

Chaz is configured via a YAML file passed with `--config`.

## Eidetica connection and execution role

Every local entry point (`chaz`, `daemon`, `--print`, `cmd`, and `usage`)
requires an explicit Eidetica connection, existing login, and execution role.
`executor` may call models and tools and runs routines/scheduled agents;
`client` can observe, submit messages, approve requests, and use ordinary data
commands without executing agent work.

Chaz logs into the configured Eidetica account with `Instance::login_account`
and keeps the returned `AccountSession`. The upstream API replaces `User` /
`NewUser` with `AccountSession` / `NewAccount`; the YAML `login` fields and
stored Chaz data names are unchanged. Database operations still use that
account's selected per-database key and native identity, not a host-admin fallback.
This dependency update performs no account or production-state conversion.

Debug output masks password-bearing connection URL userinfo and the values of
`user`, `password`, and `sslpassword` query parameters. Other query parameters
and fragments stay visible; the connection string used by Eidetica is unchanged.

Direct ownership keeps Eidetica in this process:

```yaml
execution: executor
eidetica:
  connection: "sqlite:///var/lib/chaz/eidetica.db"
  login: { username: chaz, passwordless: true }
  sync:
    iroh: true
    http_listen: 127.0.0.1:8765
```

A service client uses the daemon-owned instance. It must not include `sync:`;
the Eidetica daemon owns transports, peers, tickets, and bootstrap:

```yaml
execution: client
eidetica:
  connection: "unix:///run/eidetica/service.sock"
  login: { username: chaz, passwordless: true }
```

The connector never creates a store or user and never falls back from a failed
service connection to SQLite. To keep using an existing direct-owner configuration with its
old data, set `connection` to its resolved `<state_dir>/eidetica.db` and retain
the existing `chaz` login. Switching to a service connection does not import
that data; see the [fresh-state rehearsal](bridges.md#fresh-state-service-rehearsal-no-automatic-migration). Provisioning a new Eidetica store/user is a separate,
explicit operation.

CLI and TUI clients may create sessions and queue turns through that service
connection. The resident executor adopts those sessions from the durable local
index and runs them; the client process remains an observer and never calls the
model or tools. This local handoff does not mark the session as exposed on a
Matrix or Discord login.

Client startup loads the session registry, selected agent references, backend
credentials and command providers. It does not initialize web-search or
embedding credentials, tool handlers, MCP processes or YAML schedules.
Backend credentials are still needed for the model picker's catalog requests;
cached model-window metadata warms in the background. `/memory`, `/skills`
and `/schedule` commands remain available, with their usual permissions and
per-session activation. The executor runs the resulting agent work.

Long-running service-mode daemons monitor the connection. A transient daemon
restart tears down the old Chaz runtime before bounded-backoff reconnect,
re-login, database reopen, hook installation, and persisted-state
reconciliation. Bad credentials or invalid configuration fail immediately.
Interrupted turns are displayed by `/interrupted` and run only after an
explicit `/retry <request_id>`; Chaz never automatically replays a possibly
completed model/tool effect.

Ticket/bootstrap and daemon sync administration commands fail on a service
client before mutation and direct the operator to run them on the Eidetica
owner. Ordinary session, agent, approval, and user-level sync-preference data
operations continue through the service API.

## Full Example

```yaml
# Matrix connection (not needed for TUI-only or CLI-only modes)
homeserver_url: https://matrix.org
username: "chaz"
password: "hunter2" # If unset, prompted on first run
allow_list: "@user:matrix.org|@other:matrix.org" # Regex matched against the sender's Matrix ID
# message_limit: 500           # Optional: per-account message cap while the bot runs (Matrix and Discord)
# room_size_limit: 100         # Optional: refuse to respond in rooms with more than N members (Matrix only)
# chat_summary_model: "gpt-4o-mini"  # Optional: separate model for chat summarization

# Process authority and Eidetica connection (required by chaz/TUI/CLI/daemon)
execution: executor # or client
eidetica:
  connection: "sqlite:///path/to/state/eidetica.db"
  login:
    username: chaz
    passwordless: true

# Logs and other Chaz-local files (does not select the Eidetica database)
state_dir: "/path/to/state" # Default: $XDG_STATE_HOME/chaz

# LLM backends (OpenAI-compatible)
backends:
  - name: openrouter
    type: openaicompatible
    api_key: "${OPENROUTER_API_KEY}" # Env var reference
    api_base: https://openrouter.ai/api/v1
    models:
      - name: anthropic/claude-sonnet-4
        # How hard this model should think. Omit to use the
        # backend or provider default.
        # reasoning: { effort: none }
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
    max_spawn_depth: 10
    tools: null # null = all tools
    # Workers — configured one-shot LLM calls owned by this Agent.
    # Each is invocable via `spawn_worker(name=…)` from this Agent only.
    workers:
      - name: researcher
        system_prompt: "You are a research assistant. Use web_fetch to find information."
        max_spawn_depth: 20
        tools: ["web_fetch", "calculate", "get_time"]
      - name: coder
        # Pull repo-level instructions from a file; layer inline guidance on top.
        system_prompt_files: ["~/code/myproject/AGENTS.md"]
        system_prompt: "Edit files in-place; never rewrite from scratch."
        max_spawn_depth: 15
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

# Scheduled tasks — imported at executor startup as agent-owned schedules in the
# owning agent's DB (Pinned to the resolved session), fired by the same
# RoutineEngine as the /schedule command. Bootstrap YAML schedules are
# cron-only; runtime /schedule commands and schedule tools also support
# fixed-delay intervals. Cron is 6 fields:
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
#
# context_projection: extensions allowed to reshape each model request.
# Empty (the default) sends every request unchanged.
# context_projection:
#   - extension: pruner
#     authority: conversation  # or full_context (may edit instructions/tools)
#     required: false          # true: refuse the call if it cannot run

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

A missing or failing embedder never makes recall worse than the lexical baseline. A working one is fused with BM25 at equal weight, so the model you pick does affect ranking in both directions — see [Measured recall](memory.md#measured-recall) for the numbers and the harness that produces them.

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

### Per-model reasoning

Some models think before they answer, spending extra tokens working through the question. That helps on hard problems but slows down simple ones. You can set how much effort each model puts in:

```yaml
backends:
  - name: proxy
    type: openaicompatible
    api_base: http://127.0.0.1:18081/v1
    models:
      - name: example/model
        reasoning: { effort: none }
```

| Field                                  | Default                     | Meaning                                                                                                                                         |
| -------------------------------------- | --------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| `backends[].models[].reasoning.effort` | Omitted: no control sent    | `none`, `minimal`, `low`, `medium`, `high`, `xhigh` or `max`, forwarded unchanged. Which values a model accepts is up to its provider.          |
| `backends[].models[].reasoning.format` | Omitted: `reasoning` object | Chat Completions only. `openrouter` sends `reasoning: {"effort": …}`; `openai` sends top-level `reasoning_effort`. Unknown values fail parsing. |

What reaches the wire depends on the model's effective API (see [Choosing an API](#choosing-an-api-without-renaming-models)):

| Effective API        | `format` omitted                                     | `format: openrouter`         | `format: openai`        |
| -------------------- | ---------------------------------------------------- | ---------------------------- | ----------------------- |
| `chat-completions`   | `"reasoning": {"effort": …}` (unchanged legacy body) | `"reasoning": {"effort": …}` | `"reasoning_effort": …` |
| `responses`          | `"reasoning": {"effort": …}`; `max` refused          | Refused at startup           | Refused at startup      |
| `anthropic-messages` | Nothing; startup warning                             | Refused at startup           | Refused at startup      |

The setting applies only to the named model; models on the same backend may use different levels and formats.
Live-discovered or unlisted models send no reasoning control.
See [Reasoning controls are requests, not guarantees](#reasoning-controls-are-requests-not-guarantees) for what a provider does with them.

## Reasoning controls are requests, not guarantees

`reasoning.effort` tells Chaz which field to send. The provider decides what it means.
Supported levels depend on the model, the API and the provider, and change over time:

- `none` is not a universal off switch. Some models reject it (OpenRouter marks them `mandatory`), some cannot stop thinking at all, and native Anthropic effort has no `none`.
- A provider may map an unsupported level to the nearest supported one (OpenRouter documents this), return HTTP 400, or ignore a field it does not know.
- Chaz never translates a level, guesses a format from a URL, host or model name, or retries a rejected request without the control. Any of those would silently change cost and behavior.

**Format is explicit.** OpenRouter's Chat dialect is a `reasoning` object; OpenAI Chat Completions and several OpenAI-compatible servers document a top-level `reasoning_effort` string instead.
Omitting `format` keeps the `reasoning` object Chaz has always sent, byte for byte.
Format is per model because one gateway can front providers that speak different dialects.
It is separate from `api`: the native Responses API always uses its own `reasoning.effort`, so any explicit `format` on a Responses or Anthropic Messages model refuses to start, even though the OpenRouter object looks similar.

**Native Anthropic does not send effort.** Chaz has no mapping to Anthropic's `output_config.effort` yet.
A model on `type: anthropic` with `reasoning` configured starts with a warning and keeps the provider default; its requests are unchanged.

**Proxies can override you.** When Chaz talks to a gateway, Chaz's explicit value replaces a gateway _default_ that applies only to requests without one.
A gateway that _forces_ a field overwrites whatever Chaz sent, and Chaz cannot see that.
Chaz reports what it sent, not what reached the model.

**What you see.** At startup the executor logs one line per configured model: `sends <field>=<value>` at info, the Anthropic warning, or the refusal.
When a provider rejects a request that carried a control, the error keeps the provider's message and adds the field and value sent, without claiming that field was the cause.
A provider body that is not OpenAI-shaped (for example a bare 422) is kept as a single-line excerpt of at most 500 characters, with the backend's credential redacted, rather than replaced by a parser error.
Rejections stay non-retryable.
Reasoning token counts appear in usage only when the provider reports them; a missing count is unknown, not zero.

These settings live in the executor's YAML and take effect after restart.

### Walkthrough: one gateway, two reasoning dialects

1. Configure one gateway with a model that takes OpenRouter's object, one that documents `reasoning_effort`, and a native Anthropic model:

   ```yaml
   backends:
     - name: proxy
       type: openaicompatible
       api_base: http://127.0.0.1:18081/v1
       models:
         - name: example/router-model
           reasoning: { effort: low }
         - name: example/openai-model
           reasoning: { effort: low, format: openai }
     - name: claude
       type: anthropic
       models:
         - name: example/claude-model
           reasoning: { effort: low }
   ```

2. Start the executor. Its log states what each model will send:

   ```text
   INFO chaz: Reasoning: backend 'proxy' model 'example/router-model' (chat-completions) sends reasoning.effort=low; provider support is not verified
   INFO chaz: Reasoning: backend 'proxy' model 'example/openai-model' (chat-completions) sends reasoning_effort=low; provider support is not verified
   WARN chaz: Reasoning: backend 'claude' model 'example/claude-model': reasoning.effort=low is not sent; Chaz has no reasoning control for the anthropic-messages API, so the provider default applies
   ```

   Remove `reasoning` from the Anthropic model if you do not want the warning; its requests are the same either way.

3. Change `example/openai-model` to `effort: none` on a model that requires reasoning. The turn fails once, with no retry:

   ```text
   Invalid request: Unsupported value: 'reasoning_effort' does not support 'none' with this model. (request to backend 'proxy' model 'example/openai-model' included reasoning_effort=none; the provider did not say whether that field caused the rejection)
   ```

   Choose a level the model's provider documents, or remove `reasoning`, restart, and resend.

4. Move `example/openai-model` to `api: responses` but leave `format: openai`. The executor refuses to start:

   ```text
   Error: invalid reasoning config: backend 'proxy' model 'example/openai-model': reasoning.format 'openai' selects a Chat Completions field, but the model's API is 'responses'. Remove reasoning.format; Responses always sends reasoning.effort
   ```

   Delete the `format` line. Responses then sends `reasoning: {"effort": "low"}` natively.

## Choosing an API without renaming models

A backend is one provider connection: its URL, credentials, timeout and retries.
The request API can differ by model on that connection without adding a second backend.
`type: openaicompatible` still defaults to Chat Completions; `type: anthropic` still defaults to native Anthropic Messages.

| Field                     | Default              | Meaning                                                                                   |
| ------------------------- | -------------------- | ----------------------------------------------------------------------------------------- |
| `backends[].api`          | Determined by `type` | Request API for models without an override, including live-discovered and unknown IDs.    |
| `backends[].models[].api` | Backend's API        | Override for this exact `name`, written as the raw provider model ID (no backend prefix). |

API values are `chat-completions`, `responses`, and `anthropic-messages`.
Unknown values fail YAML parsing.
OpenAI-compatible backends accept Chat Completions and Responses selection; Anthropic backends accept only Anthropic Messages.
An incompatible selection fails with a configuration error before sending a request.
Responses uses a native stateless client; see [Native Responses conversations](#native-responses-conversations) for replay, reasoning and cache identity.
Endpoint rejection is an error, never an automatic fallback to Chat Completions.

Resolution is model override, then backend `api`, then the default for `type`.
Chaz does not guess from names such as `gpt-*`, and `/models` metadata does not select or override the protocol.
The live catalog supplies picker entries and model information, not an allowlist: you can still request IDs absent from it.
A model override works whether its ID appears in the live catalog or only in YAML.
Models discovered without an override, or supplied manually, use the backend default.

These settings live in the executor's YAML and take effect after restart.
Changing only `api` on an existing model row does not change agent defaults stored in AgentDb, the model selected for a session, or model naming: one backend uses raw IDs, multiple backends still use `backend:model`.
Discovery continues through the backend type's catalog endpoint using the same credentials, even when its request API is set to Responses.

### Favorites are also the default list

`models` is the configured favorites/default list, not a separate API override map.
The first model of the first backend remains the implicit default for unpinned
calls, including `/compact`, which does not inherit the session's selected model.
Discovery supplies neither a default nor a durable offline catalog; when catalog
loading fails, the picker retains the YAML favorites and shows the fetch error.
Edit matching model rows in place, preserve the first/default row and other
favorites, and do not append duplicate rows to override earlier ones: the first
exact match wins.
Replacing the list with only a Responses row, or reordering it, can therefore
change the effective fallback model and protocol even with pinned agents.

### Walkthrough: one connection, two API choices

1. Keep a single provider connection and edit the desired existing model row in place.
   Retain its default row, other favorites and their ordering; this fragment is
   not a replacement catalog:

   ```yaml
   backends:
     - name: proxy
       type: openaicompatible
       api_base: http://127.0.0.1:18081/v1
       api_key: "${PROXY_API_KEY}"
       # api: chat-completions   # already the default
       models:
         - name: example/chat-model
         - name: example/response-model
           api: responses
   ```

2. Restart the executor and select `example/chat-model` as usual.
   Its requests use `/v1/chat/completions`.
   The IDs remain `example/chat-model` and `example/response-model`, not `proxy:example/chat-model`.
   Any additional IDs discovered from `/v1/models` also use Chat Completions unless explicitly overridden.
   With this order, unpinned calls and `/compact` still use `example/chat-model`,
   even after selecting the Responses model for the session.
   If `/v1/models` is unavailable, the picker shows its fetch error and both
   configured favorites remain selectable; restore discovery and reopen the
   picker to add live catalog entries without changing the default.

3. Select `example/response-model` to send requests to `/v1/responses`.
   Its model ID and credentials remain unchanged.
   For a failure/recovery example, see the [native transport walkthrough](#example-one-connection-two-protocols).

4. For a provider where most IDs require Responses, set `backends[].api: responses` instead and give Chat-compatible models an explicit `api: chat-completions` override.
   This also covers newly discovered and unknown IDs, without maintaining a complete YAML model list.

## Agents

Each `agents:` entry seeds an Agent DB on first boot; subsequent edits live in the DB, not the YAML. See [Agents](agents.md) for the runtime model and live-edit commands. Per-agent fields:

| Field                  | Type                   | Notes                                                                                                                                 |
| ---------------------- | ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| `name`                 | string                 | Required. Display name; also the lookup key for `default_agents`, `/agent add`, schedules.                                            |
| `system_prompt`        | string                 | Inline system prompt. Appended after `system_prompt_files` content when both are set.                                                 |
| `system_prompt_files`  | list of paths          | File contents concatenated into the system prompt. `~` expansion supported.                                                           |
| `model`                | string                 | Default model (e.g. `openrouter:anthropic/claude-sonnet-4`). Backend prefix required when ambiguous.                                  |
| `tools`                | list of tool names     | Whitelist. `null` (omitted) means "all tools". Supports `namespace__*` globs for MCP tools.                                           |
| `workers`              | list of WorkerConfig   | Per-Agent Worker templates invocable via `spawn_worker`. See "Worker fields" below.                                                   |
| `max_spawn_depth`      | int                    | Maximum nested spawn depth. Default 10. Does not cap tool calls; see [How a turn ends](agents.md#how-a-turn-ends).                    |
| `autonomous`           | bool                   | Allow firing without an inbound human message (scheduler wakes). Default `false`.                                                     |
| `max_context_tokens`   | int                    | Per-agent override of `context.max_context_tokens`.                                                                                   |
| `tool_profile`         | string                 | References a key in top-level `tool_profiles`.                                                                                        |
| `grants`               | map<tool, Grants>      | Per-tool grant overrides (shell allow/deny, network endpoints, fs paths). Merged per-kind over policy.                                |
| `default_memory_banks` | list of bank names     | Auto-attached at first boot. Missing banks are auto-created.                                                                          |
| `default_skill_banks`  | list of bank names     | Auto-attached at first boot. Missing banks are warned and skipped.                                                                    |
| `presets`              | map<name, AgentPreset> | Named override bundles for Worker templates (model / spawn depth / tools / role suffix / tool_profile); not a `spawn_agent` argument. |

### Worker fields

A Worker is a configured one-shot LLM call declared under an Agent's
`workers:` list. Workers have no identity, no keys, and no persistent
state of their own — entries written during a Worker invocation are
signed by the parent Agent's key. Lookup is per-Agent; Chaz's
`researcher` is distinct from Scout's `researcher`.

| Field                 | Type                   | Notes                                                                                                                                    |
| --------------------- | ---------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| `name`                | string                 | Required. Unique within the parent Agent's `workers:` list. Used as `spawn_worker(name=…)`.                                              |
| `system_prompt`       | string                 | Worker's system prompt. Falls back to (inherits) the parent Agent's prompt when omitted.                                                 |
| `system_prompt_files` | list of paths          | Concatenated into the Worker prompt. `~` expansion supported.                                                                            |
| `model`               | string                 | Override the model. Falls back to the parent Agent's `model`.                                                                            |
| `tools`               | list of tool names     | Narrows the parent Agent's tool list. May include other Worker names; recursion bounded by depth.                                        |
| `max_spawn_depth`     | int                    | Maximum nested spawn depth of this Worker's child session. Falls back to the parent Agent's value. Does not cap the Worker's tool calls. |
| `presets`             | map<name, AgentPreset> | Selectable via the `preset` arg of `spawn_worker`.                                                                                       |

### `default_agents`

Names of agents auto-attached to every freshly-created session, in
order. The first entry effectively becomes the routing host — when an
incoming message has no `@mention` and no explicit `/agent host`, the
resolution chain picks the first authorized agent on the session.

```yaml
agents:
  - name: chaz
  - name: researcher

default_agents: [chaz, researcher]
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

### `agent_groups`

Named alternatives to `default_agents`, chosen when a session is
created: `/new <group>`. Each value is an ordered agent list with the
same semantics as `default_agents` — names must match entries in
`agents:`, and the first entry effectively becomes the routing host.

```yaml
agents:
  - name: chaz
  - name: researcher
  - name: critic

default_agents: [chaz]

agent_groups:
  research: [researcher, critic]
  pair: [chaz, critic]
```

`/new` with no argument uses `default_agents`. `/new research` attaches
`researcher` then `critic` instead — a group **replaces** the default
list rather than adding to it. `/groups` lists what is configured, and
`/new <unknown>` reports the known group names without creating a
session. A group configured as an empty list (`[]`) creates a session
with no agents attached; it does not fall back to `default_agents`.

Groups are read from yaml only. Unlike `default_agents` — which the TUI
edits live under Settings → Peer → Defaults and persists to the peer DB
— there is no runtime editor for groups, so there is no peer-DB
override to shadow the yaml. Settings → Peer → About lists the
configured group names read-only. Editing the yaml and restarting the
peer is the whole update path.

## Security

Security settings control tool approval, network access, shell sandboxing, secret leak detection, and tool rate limiting. See [Security](security.md) for details.

## Approvals

How long a tool-approval prompt may wait for an answer, in seconds. Defaults to 300 (5 minutes) when the block is absent.

```yaml
approvals:
  timeout: 300
```

The daemon reads this block; bridges do not. Each approval request carries the ceiling with it so a bridge can show you how long the prompt has, but no bridge enforces it — the daemon does. On a split deployment that means only the daemon's config file matters here. See [Security → Tool Approval](security.md#tool-approval).

## MCP Servers

External tools via the Model Context Protocol. See [MCP External Tools](mcp.md) for details.

## Schedules

Cron-driven agent wakes. Each entry is imported at executor startup as an **agent-owned schedule** (see [Agents — Schedules](agents.md#schedules)) in the owning agent's DB, Pinned to the resolved session. On fire, the owning agent's turn runs directly with `task` as the wake prompt — no `Directive` is written to the session. `agent:` names the owner (display name or DB id); omit it to use the peer's default agent. `session:` is referenced by name or eidetica DB root ID. Responses are delivered to every Matrix room attached to that session (see [Matrix: channel attachment](matrix.md#session-attachment)).

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

### Dates follow the conversation

The default context strategy adds date markers to the model's conversation log
from existing stored entry timestamps. This is automatic for local, bridge and
scheduled inputs. There is no new setting or stored date-reminder entry.

| Marker field   | Meaning                                           |
| -------------- | ------------------------------------------------- |
| `current_date` | UTC date of the following retained session entry. |
| `timezone`     | `UTC`, the timezone of stored entry timestamps.   |

One marker precedes the first retained entry. Another is added when a later
entry's date differs; same-day entries do not repeat it. New days extend the
log rather than replacing a date in the earlier system prompt. Replaying the
same history produces the same date markers, even on a different day or peer.
Entries stay in their stored order; historical or delayed inputs retain their
own dates. These markers date the conversation, not the wall clock at execution.
Use `get_time` for the actual current time.

For example, continuing an old conversation:

1. On October 6, the model sees this marker followed by the conversation:

   ```text
   current_date: 2026-10-06
   timezone: UTC
   ```

2. A message stored on October 13 appends a new marker before that message:

   ```text
   …October 6 conversation remains unchanged…
   current_date: 2026-10-13
   timezone: UTC
   …October 13 message…
   ```

3. Reopen the session: the same stored history produces the same sequence.
   After compaction or budget truncation removes older entries, the retained
   log starts with the first retained entry's date instead. A summary can still
   mention earlier dates without changing its stored timestamp or the markers.
   If a historical date is mistaken for now, ask the agent to use `get_time`;
   rebuilding context does not silently redate historical messages. With no
   retained entries there is no date to infer, so no marker is invented.

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

`context_strategy:` selects one required installed context provider. The default
is the built-in `baseline_context`; missing, inactive or failed selection stops
dispatch, with no old core fallback. See [One required context strategy](extensions.md#one-required-context-strategy).

| Setting                      | Default            | Description                                              |
| ---------------------------- | ------------------ | -------------------------------------------------------- |
| `context_strategy.extension` | `baseline_context` | Manifest name of the required active selection provider. |

```yaml
context_strategy:
  extension: baseline_context
```

`durable_context:` grants intentional session-backed text/state contributions.
It defaults to `[]`; installation does not grant writes. Names must be distinct.
See [Durable context](extensions.md#durable-context-is-an-explicit-write) for
scope, retry, failure and privacy rules. It is independent of projection grants.

| Key                           | Default | Meaning                                                                                                    |
| ----------------------------- | ------- | ---------------------------------------------------------------------------------------------------------- |
| `durable_context[].extension` | —       | Installed contributor's manifest name.                                                                     |
| `durable_context[].required`  | `false` | Refuse dispatch if the contributor cannot commit; optional failures require a coherent committed fallback. |

`context_projection:` lists the extensions allowed to reshape the request sent on each model call, in order. Installing or activating an extension never grants this; only this list does. See [Context projection grants](extensions.md#context-projection-grants).

| Key                              | Default        | Meaning                                                                                                                                           |
| -------------------------------- | -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| `context_projection[].extension` | —              | Extension (manifest) name. Each name may appear once.                                                                                             |
| `context_projection[].authority` | `conversation` | `conversation`: conversation messages only. `full_context`: may also rewrite instructions and hide or re-describe tools the turn already exposes. |
| `context_projection[].required`  | `false`        | `true`: if the extension is missing, inactive, fails, or returns an invalid request, the model call is refused. `false`: it is skipped.           |

```yaml
context_projection:
  - extension: redactor
    authority: conversation
    required: true
  - extension: pruner
    authority: full_context
```

Conversation entries run before full-context entries; list order holds within each group. A blank or repeated name stops startup.

## Print mode

`chaz -p "<prompt>"` (alias `--print`) runs a single ReAct turn and exits. There is no interactive approval surface, so a small set of tools must be pre-approved to make the mode useful. Defaults to `[shell, write_file]`. Override per-deployment:

```yaml
cli:
  auto_approved_tools: [shell, write_file, web_fetch]
```

`-p --session NAME` reuses a named session across invocations (find-or-create). Without `--session` each invocation creates a fresh ephemeral session. Without `-p`, `--session NAME` opens that session in the TUI instead of the session hub.

## State Directory

Chaz persists all data in the state directory:

- `eidetica.db` — SQLite database backing every chaz eidetica DB: per-session DBs, per-agent DBs, per-bank DBs, plus the peer-local `chaz_group` (sessions/channels/names) and `chaz_peer` (credentials/schedule_last_fired/schedule_state) bookkeeping DBs
- Headjack session data (Matrix sync token, device keys)

The state directory defaults to `$XDG_STATE_HOME/chaz`. Override with `state_dir` in config.

## Native Responses conversations

Select `api: responses` only for models whose configured endpoint supports the
OpenAI Responses protocol.
Chaz posts to `<api_base>/responses`; a rejected request never falls back to
Chat Completions.
Other models on the same connection keep their configured API or the backend
family's default.

| Setting or behavior                    | Responses transport                                                                                                                                                                   |
| -------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `backends[].models[].api: responses`   | Select the native Responses client for this exact model ID.                                                                                                                           |
| `backends[].models[].reasoning.effort` | Send `none`, `minimal`, `low`, `medium`, `high` or `xhigh`; `max` is rejected before sending. Provider/model support may be narrower; an explicit `reasoning.format` refuses startup. |
| `backends[].max_tokens`                | Send an optional `max_output_tokens` ceiling; omission leaves the provider default.                                                                                                   |
| Conversation storage                   | Use stateless requests (`store: false`); replay opaque output only for the same API, requested raw model and connection origin.                                                       |
| Cache identity                         | Send an opaque `prompt_cache_key` scoped to the caller, connection and persisted session.                                                                                             |
| Cache accounting                       | Record the returned `usage.input_tokens_details.cached_tokens`; zero is a reported miss, absence or malformed data is unknown.                                                        |
| Completed model switches               | Convert only retained visible text and complete real call/result pairs; unsupported history refuses before HTTP.                                                                      |
| Pending waits                          | Changed or unknown origin refuses before wait observation, approval or further effects.                                                                                               |

### What survives a restart

The session holds complete ordered native output in its durable transcript;
terminal text output also travels with the final entry's metadata.
On the same origin, Chaz replays those items in order, followed by each tool's
`function_call_output` using the original `call_id`.
Reasoning items are not flattened into text or reconstructed from a summary.
Existing context budgets and explicit compaction still determine which history
is retained in a request.
An interrupted tool batch stays held unless it meets the existing pending-wait
recovery contract; Chaz does not invent a missing tool result.
See [durable turn recovery](../architecture/sessions.md#durable-turn-recovery).

### Switching models is not transferring provider state

Host-owned replay origin records the effective API and requested raw model,
not the provider-returned alias, plus an opaque connection digest.
The digest binds the peer-local credential store, connection name, normalized
endpoint and credential namespace/value without exposing them in history or
requests.
Changing the endpoint, namespace or credential after reopening a session is
not a compatible origin, even if the model ID and connection display name match.
Missing legacy origin is unknown, never assumed compatible.
Context projectors cannot change this origin independently of the native output.

For completed foreign or legacy history, Chaz constructs a request-only view
from retained visible assistant text and complete, unambiguous real function
call/result pairs.
Foreign reasoning and native item IDs are not forwarded; the stored transcript
is never rewritten and earlier effects are not rerun.
Unknown provider output, non-text content, incomplete results or unsupported
ordering instead produce a conversion error before HTTP.
A pending native wait is stricter: restore its original API/model/connection
before resuming, or explicitly interrupt it after inspecting the turn.
Changing targets cannot reinterpret or retire that checkpoint.

Runtime calls in the same persisted session reuse the cache key across turns
and restarts on the same caller and connection.
New sessions, other callers and other connections use different keys, even
with identical prompt text.
Raw database IDs, local paths and credentials are not sent as the key.
Standalone calls without a persisted-session scope omit it.
A stable key is not evidence of a cache hit: only returned usage reports that.
The existing usage ledger and TUI consume the normalized token counts.

### Example: one connection, two protocols

1. Use the two-model connection in [the API selection walkthrough](#walkthrough-one-connection-two-api-choices).
   Its gateway must support both endpoints.
   The Chat model keeps Chat Completions; `example/response-model` uses Responses
   with the same credentials and no added model-name prefix.
   Add `reasoning: { effort: high }` to that model if the endpoint supports it.

2. Select `example/response-model` in a persisted session and ask a question that uses
   a tool.
   Chaz sends the function schemas, then replays the returned reasoning and
   function calls together with their results on the next request.
   Reopen that session to continue with the same scoped key and retained
   native items.

3. If the model is configured with `reasoning: { effort: max }`, the turn fails
   before HTTP with:

   ```text
   Configuration error: Responses reasoning effort 'max' is unsupported; configure a supported effort such as 'xhigh'
   ```

   Choose an effort supported by that endpoint/model, restart the executor
   with the corrected configuration, and resend the question.
   If a process stop instead left the turn interrupted, inspect
   `/interrupted` before explicitly using `/retry <request_id>`.
   If the gateway rejects `/responses` itself, correct the endpoint or the
   model's explicit API setting; Chaz does not hide that rejection by sending
   a Chat request.

4. After a completed turn, select a different Responses model.
   Portable visible history and real tool results continue without the old
   model's encrypted reasoning or native item IDs.
   If history contains an unsupported provider action, a representative error is:

   ```text
   Invalid request: Cannot convert Responses history: unknown provider output item. Restore the original model/connection or start a new session; the transcript is unchanged
   ```

   Restore the original target to keep native replay, or start a new session.
   For a checkpointed native wait, switching models instead reports:

   ```text
   Invalid request: Pending Responses continuation has an incompatible or unknown origin; restore the original API, requested model and connection before resuming, or explicitly interrupt the turn. No wait or tool effect was run
   ```

   Restore the original configuration and inspect `/interrupted` before
   explicitly resuming/retrying; do not treat the refusal as permission to
   repeat effects from an interrupted tool batch.
