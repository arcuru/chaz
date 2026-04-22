# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Chaz is an AI agent orchestrator for Matrix written in Rust. It connects to Matrix rooms via headjack/matrix-sdk and responds using OpenAI-compatible LLM backends (e.g., OpenRouter). Features a ReAct tool-calling loop, session-based conversation history (via eidetica), and a TUI mode for testing without Matrix.

**Status**: Active development — Phases 0–16 + Living Agents Stages 1–8 + Memory Banks Stage 9 (A–E) complete. Recent (Stage 9, 2026-04-21): memory model rewritten around eidetica key-possession instead of a capability flag. A memory bank is any eidetica DB with a `memory` subtree — including every Agent DB's own memory (so one agent can be granted Read on another's memory just by key grant). New standalone `MemoryBankDb` primitive (`src/memory_bank_db.rs`) plus a `memory_banks` Table store on each AgentDb holding `MemoryBankRef { name, db_id, permission }` rows. `remember`/`recall` take an optional `bank` arg — no arg = self memory (own AgentDb::memory), arg = look up in memory_banks and open with agent's key. New `list_memory_banks` tool mirrors the `describe_tool` discovery pattern. New `/memory new|list|delete|grant|revoke|share|import` shared commands (Stage 9.D). Grant writes the bank's AuthSettings first (authoritative) then mirrors a ref into the agent's memory_banks (view), rolling back the auth on view-write failure so the two sides stay consistent. Deleted: `GlobalRemember`/`GlobalRecall` tools, `MemoryGrant` type, `Grants.memory` field, `global_memory` central-DB store — cross-agent sharing is now "grant a bank" not a capability flag. Self-memory access is no longer grant-gated: an agent always has its own DB key, so if `remember`/`recall` is in its allowlist it just works. External bank access is authoritatively gated by the bank's AuthSettings. Known gap inherited from Stage 5: bank writes aren't signed by the agent-specific key (blocked on eidetica's `open_database_with_key`). Earlier (Stage 8, 2026-04-20): `Server::hydrate_agent_from_db` re-reads the agent's `AgentDb::config` per-message and rebuilds the runtime `Agent` via `AgentRegistry::build_from_db_config`, so origin-peer config edits propagate through sync without a restart. Stage 6 yaml-as-template pivot (2026-04-21): `bootstrap_from_config` no longer overwrites existing AgentDb config; yaml is a seeds-on-first-boot template and AgentDb is the runtime source of truth. `/agent set <ref> <field> <value>` for live field edits. `/agent new` `k=v` now covers `role`/`model`/`tools`/`can_spawn`/`allowed_callers`/`autonomous`/`max_iterations`/`tool_profile`/`max_context_tokens`. Earlier (Stage 6): Living Agents CRUD via `/agent new|share|import|hosted|delete`. `AgentRegistry` is `RwLock<Vec<Agent>>` so runtime registration works. Stage 5: `spawn_agent` delegates to Living Agents, `spawn_task` generates ephemeral revocable keys, both wire parent→child `DelegatedTreeRef { max: Admin(0) }`. Stages 1–4: agents have persistent eidetica-DB identity signed by per-agent keys; peer-local `AgentIndex` maps pubkeys to hosted agents in O(1); session AuthSettings is the authoritative participant list (`SessionMeta.agents` mirrors); routing via `resolve_agent_for_entry` (override > @mention > host agent > first authorized > legacy > default); heartbeat rules live inside each session DB, fired by a peer-local `HeartbeatRunner` (`last_fired` peer-local in central DB); shared commands `/agent add|remove|list|host` and `/heartbeat add|remove|list`. Earlier: Phase-16 session/transport decoupling; typed capability grants + per-agent overlays; MCP Streamable HTTP + directory scanning + dynamic tool re-discovery; loop detection; LLM request timeout + retry with exponential backoff.

## Build & Development Commands

Development environment uses Nix flakes with treefmt (enter via `direnv allow` or `nix develop .#`). The `justfile` is the primary task runner:

```bash
just build          # cargo build
just test           # cargo test (alias: just t)
just lint           # clippy + other lints
just fmt            # treefmt (rustfmt, alejandra, prettier)
just ci             # all checks: lint fmt test nix build
just nix build      # nix build
just nix check      # nix flake check
```

Note: `nix develop` may pick up eidetica's dev shell due to the git dependency. Use `nix develop .#` to explicitly select chaz's shell.

307 unit tests covering security (leak detection, SSRF, sanitizer, XML wrapping, rate limiting), context budgeting (tiktoken), agent spawn permissions, tool profiles (globs), secret resolution, loop detection, MCP (SSE parsing, JSON-RPC response handling, tool metadata refresh/removal, config deserialization, directory scanning, subprocess integration lifecycle/error/death), grants (shell/network checks, legacy migration, per-kind merge), Living Agents (Agent DB create/open/bootstrap-as-template, AgentIndex register/find/sync, SessionMeta.agents round-trip, session attach/detach + auth-and-meta-and-history effects, routing via AuthSettings + legacy fallback, @mention parsing, host-agent fallback, override precedence, heartbeat rule CRUD + last_fired persistence, parent→child delegation, ephemeral task-key lifecycle + revoke-without-breaking-reads, AgentRegistry runtime register + dedup, Agent::from_db_config field mapping, create_new_agent_db dup rejection, open_agent_db-None-without-key, `/agent set` edits DB+registry), memory (own-memory round-trip, per-agent isolation across peers, bank write with permissions, read-only bank rejection, unknown-bank lists available, list_memory_banks output), memory banks commands (new rejects duplicate, list formatting, delete preserves DB, grant writes auth+ref consistent, revoke reverses both, share/import happy paths), and live config hydration (build_from_db_config resolves config-defined roles, upsert replace/insert, config-edit pickup, Server::hydrate_agent picks up DB edits + returns input when no DB). Use `CARGO_TARGET_DIR=target-test cargo test --bin chaz`. Build deps: `pkg-config` (provided by Nix dev shells; rustls-based — no openssl/sqlite needed).

## Architecture

```
main.rs              CLI args, config, eidetica init, secret store, security context, tool registry, gateway dispatch
commands.rs          Transport-neutral session commands: Command enum, CommandContext, CommandOutcome, dispatch(). Gateways parse their syntax → Command → dispatch → render.
config.rs            Config, Backend (api_key_ref → SecretStore), AgentConfig, SecurityConfig types
types.rs             ConversationId (gateway-agnostic)
agent.rs             Agent (with spawn perms, presets), AgentRegistry (Arc-shared, YAML-configurable). Runtime view of yaml-declared agents. Bridged to Agent DBs by display_name.
agent_db.rs          Living Agents: AgentDb handle wrapping an eidetica Database with well-known stores (config, memory, meta, history, memory_banks). AgentDbConfig/AgentMeta JSON blobs. MemoryBankRef + BankPermission for Stage 9.B bank grants. create_agent_db generates a fresh per-agent key via User::add_private_key; bootstrap_from_config seeds AgentDbs from yaml on first boot only (yaml-as-template — subsequent boots leave existing DB config alone).
agent_index.rs       Peer-local agents-I-host index. DocStore in central DB keyed by Agent DB root ID, value = { db_id, display_name, pubkey }. Supports register/unregister/find_by_{id,name,pubkey}/sync_from_bootstrap. Needed because eidetica has no "DBs where key K has permission P" inverse query.
memory_bank_db.rs    Memory Banks Stage 9.A: standalone eidetica DB owned by a per-bank key. Single `memory` Table (same name/shape as AgentDb::memory so tool code treats agents and banks uniformly) plus a small `meta` DocStore. create_memory_bank / find_memory_bank helpers parallel agent_db.
memory_bank_index.rs Peer-local banks-I-host index. Same shape/rationale as agent_index (DocStore in central DB, never synced). Maintained by /memory new and /memory import.
heartbeat.rs         Per-session HeartbeatRule Table (cron + task + target_agent_db_id). HeartbeatRunner polls hosted sessions every 30s; fires due rules whose target agent is in agent_index. last_fired kept peer-local in central DB (heartbeat_last_fired DocStore) so peers don't coordinate.
session.rs           SessionRegistry (index-only: sessions/matrix_channels/session_names DocStores + new-session events) + Session (per-conversation eidetica DB with entries Table + meta DocStore) + EntryType (Message, Directive, ToolCall, ToolResult, Ack, Error, Summary) + SessionMeta (per-session config: name, agent, model, role, backend, agents: Vec<AgentRef>, host_agent_db_id — lives in the session's own DB so it syncs) + AgentRef + attach_agent_to_session/detach_agent_from_session (writes session AuthSettings + mirrors meta + logs to agent history) + resolve_agent / resolve_agent_for_entry (mention-aware) + parse_mentions.
context.rs           ContextBuilder — token-budgeted context assembly from session entries, with Summary boundary support
tool.rs              Tool trait, ToolDescriptor, ToolPolicy, ToolPolicyRegistry, ToolRegistry, ScopedTools, ToolProfile, PresentationMode
grants.rs            Typed capability grants (Grants, ShellGrant, NetworkGrant, FsGrant, EndpointPattern), per-kind merge_over, legacy SecurityConfig migration shim
mcp/                 MCP integration, split across submodules:
  mod.rs             Public surface: re-exports McpServer/McpTool, plus load_server_configs_from_dir + start_mcp_servers
  parse.rs           Pure parsers: parse_sse_body, parse_jsonrpc_response (no I/O, heavily tested)
  transport.rs       Transport enum + StdioTransport + HttpTransport (each owns its own send_request/send_notification)
  server.rs          McpServer (per-connection manager) + McpTool (Tool trait wrapper) + subprocess integration tests
tools/
  mod.rs             Re-exports all tools
  agent.rs           spawn_agent — delegate to a Living Agent (agent_ref = display name or DB ID) via attach_agent_to_session + parent-delegated child session (sync/async)
  task.rs            spawn_task — one-shot ephemeral run. Generates a fresh keypair, grants Write(100) on a child session, runs the ReAct loop, then revokes the key on completion (session persists as audit record)
  compact.rs         compact — write a Summary entry to compact conversation history (Low risk)
  describe.rs        describe_tool — returns full tool description/schema for on-demand discovery (Low risk)
  time.rs            get_time — current UTC time (Low risk)
  calculate.rs       calculate — math expressions (Low risk)
  shell.rs           shell — execute commands (High risk, approval required; allowlist/denylist read from ctx.grants().shell)
  file.rs            read_file (Low), write_file (Medium, approval unless auto-approved)
  web.rs             web_fetch — HTTP GET/POST (Medium risk; endpoint policy + SSRF toggle read from ctx.grants().network)
  memory.rs          remember/recall — optional `bank` arg. No bank = self memory (agent's own AgentDb::memory). With bank = look up name in agent's `memory_banks` subtree, open bank DB with agent's key, operate on its `memory` store. list_memory_banks for on-demand discovery.
security/
  mod.rs             SecurityContext (leak detector, auto-approved tools, approval channel)
  secrets.rs         SecretStore — eidetica DocStore-backed secret storage, HashMap cache, env var resolution
  leak_detector.rs   LeakDetector — 12 secret patterns, redact/block policy
  network.rs         NetworkPolicy — endpoint allowlisting, SSRF protection
  sanitizer.rs       Sanitizer — prompt injection detection (warning-only)
runtime.rs           ReAct loop with security: approval gate, timeouts, leak scanning, injection warnings; RuntimeEventSink for audit trail; retry with exponential backoff; loop detection (tool call fingerprinting)
server.rs            Callback-driven Server: registers on_local_write on session DBs, processing loop, agent task spawning, response delivery, child session management
gateway/
  mod.rs             Gateway trait, ApprovalExchange/ApprovalDecision
  matrix/
    mod.rs           MatrixGateway — lifecycle, sync, retry, text handler, `dispatch_in_room` helper. Parses `!chaz <cmd>` into Command → commands::dispatch → renders outcome to room.
    commands.rs      Matrix-specific glue: rate_limit, room backend resolution, `send` (no-context), `rename` (renames Matrix room)
    history.rs       Room history reading for backfill
  tui.rs             TuiGateway — ratatui async terminal app, Elm architecture (App/Action/ui). Parses `/<cmd>` into Command → commands::dispatch → renders outcome. TUI-only: picker modal, /debug, /raw, /clear (display).
error.rs             Structured error types: top-level Error enum, LlmError (retryable/permanent classification, thiserror)
backends.rs          BackendManager (model-based routing), LLMBackend trait, ChatContext (legacy: Matrix commands, /compact), Message
openai.rs            OpenAI-compatible backend implementing LLMBackend
role.rs              Role/system prompt management
defaults.rs          Built-in default config and roles
```

### Key flows

**Message flow (Matrix):** Matrix sync → text handler → writes SessionEntry to session DB → eidetica on_local_write callback fires → server processing loop detects user message → spawns agent task → agent runs full ReAct loop → writes response SessionEntry to session DB → callback fires → server detects agent response → delivers via ResponseDelivery channel → Matrix response task sends to room. Different rooms run in parallel.

**Message flow (TUI):** Input box → writes SessionEntry to session DB → callback fires → server runs agent → writes response → on_local_write sends `()` notify → event loop re-reads session from eidetica → renders updated entries in ratatui terminal.

**ReAct loop:** ContextBuilder assembles token-budgeted RuntimeMessages from session entries (respecting Summary boundaries) → call LLM with tool definitions (with retry on transient errors) → if tool_calls: check loop detection (fingerprint-based, threshold 3) → check approval requirement → if approved: execute with timeout → emit RuntimeEvent → scan output for leaks → scan for injection (warn) → feed results back, loop → if text: return final response. Falls back to no-tools execution if backend doesn't support tools. Forces a summary if iteration cap (10) is reached or loop detected. LLM calls retry transient errors (429, 5xx, timeout, network) with exponential backoff (1s–30s, honors Retry-After headers). All LLM HTTP requests wrapped in configurable timeout (default 120s). Runtime emits ToolCall/ToolResult events via optional RuntimeEventSink; server writes these to the session DB as audit trail entries.

**spawn_agent / spawn_task:** `server.register_child_session(..., parent_session_db_id = Some(parent))` creates a child via `SessionRegistry::create_child_session` (writes `DelegatedTreeRef { max: Admin(0) }` into child's auth so parent admins inherit admin). spawn_agent then `attach_agent_to_session` on the Living Agent (stable pubkey, Write(10)). spawn_task instead `new_ephemeral_key` + `grant_write_on_session(pubkey, "task:…", Write(100))`, runs, then `revoke_key_on_session` regardless of outcome. Both write a Directive entry → on_local_write fires → process_session detects Directive → agent task → response → completion channel signals caller. Sync (default) and async (`"async": true`) modes; async spawn_task skips revoke since the task is still running.

### Key patterns

- **Gateway = bridge**: Gateways translate platform events ↔ session DB entries. Each registers its own on_local_write callback to detect agent responses and deliver to its transport. Server is transport-agnostic.
- **Callback-driven server**: Server registers on_local_write callbacks on session DBs. Callback fires → notify channel → processing loop checks latest entry → if non-agent Message or Directive, spawns agent task. Agent writes Ack → runs ReAct loop (emitting ToolCall/ToolResult events to session) → writes response. Global Semaphore(10) caps concurrent LLM calls.
- **Session messaging primitive**: All agent invocation goes through session entries. spawn_agent writes a Directive entry to a child session and awaits completion via the server's callback path (register_child_session + mpsc completion channel). Supports sync and async modes.
- **Entry types**: Message (chat), Directive (instructions to agent — included in LLM context), Summary (compacted context boundary), ToolCall/ToolResult (audit trail — excluded from LLM context), Ack (thinking indicator), Error. Only Message, Directive, and Summary enter the LLM context window.
- **Context budgeting**: `ContextBuilder` assembles `RuntimeMessage`s within a token budget (`max_context_tokens - reserved_output_tokens`). Uses tiktoken (cl100k_base) for accurate token counting. Accounts for system prompt and tool definition overhead first, then fills messages from newest backward. The most recent `Summary` entry acts as a start boundary — older entries are excluded. Configurable globally and per-agent. The `compact` tool and `/compact` TUI command write Summary entries to trigger compaction.
- **Eidetica sync**: HTTP transport enabled at startup. `/share` generates DatabaseTicket URLs, `/sync <ticket>` syncs remote sessions. Writes propagate bidirectionally via on_local_write callbacks.
- **Per-session eidetica DBs**: Each conversation gets its own eidetica Database (entries + meta stores). Sessions are identified globally by their eidetica DB root ID. `SessionRegistry` (central "chaz-registry" DB) holds indices only: `sessions` (all known session IDs + origin tag), `matrix_channels` (Matrix room_id → session_db_id — fan-out supported), `session_names` (name → session_db_id).
- **Matrix channels**: Explicit attachment records. A Matrix room's first message auto-creates a session + channel. `!chaz attach <session>` / `!chaz detach` / `!chaz channels` manage the binding. At gateway startup, every persisted channel for a joined room gets both server-processing and response-delivery callbacks installed — this is what makes scheduled-session responses reach Matrix even without an active user.
- **Named sessions**: Sessions can be given human-friendly names via `/name <alias>`. Names are unique, persisted in the registry's `session_names` index and mirrored into the session's `meta` doc. `SessionRegistry::resolve_session()` tries name → DB ID.
- **Memory (Memory Banks)**: Everything is a DB with a `memory` subtree. (1) **Self memory** — the running agent's own `AgentDb::memory` (Table<MemoryEntry>). Always accessible: the agent owns its DB key. `remember`/`recall` with no `bank` argument operate here. (2) **Shared banks** — standalone `MemoryBankDb`s (or other Agent DBs, since they have the same `memory` subtree). Agents gain access via `/memory grant <bank> <agent> <read|write>`, which writes the agent's pubkey to the bank's `AuthSettings` (authoritative) and mirrors a `MemoryBankRef { name, db_id, permission }` into the agent's own `memory_banks` Table store (view). `remember`/`recall` with a `bank` arg look the name up there and open the bank via the agent's key. `list_memory_banks` surfaces what the agent can see. No "global" scope, no `MemoryGrant` capability type — authorization is purely key possession, enforced by eidetica.
- **Agent registry**: YAML-configurable agents with per-agent tool visibility (ScopedTools with transitive narrowing). Bridged to Living Agent DBs by display name. Entries are _live-hydrated_ from `AgentDb::config` per-message (Stage 8) via `Server::hydrate_agent_from_db` → `AgentRegistry::build_from_db_config` — AgentDb is source of truth; yaml seeds it at bootstrap.
- **Living Agents**: Each agent is its own eidetica Database signed by a per-agent PrivateKey. yaml `agents:` is a template — `agent_db::bootstrap_from_config` creates one Agent DB per entry on first boot (named `agent:<display_name>`) with `config`/`memory`/`meta`/`history`/`memory_banks` stores, and on subsequent boots leaves existing DB config alone (AgentDb is runtime source of truth). Whoever holds the key hosts the agent. Stage 3+ routing is key-possession-based.
- **Memory Banks**: Memory bank = any eidetica DB with a `memory` Table<MemoryEntry> subtree. Standalone `MemoryBankDb`s live under `memory:<display_name>` settings names; an Agent DB doubles as a bank of its own memory for anyone with Read on it. Grants mirror Stage-3 session participation: add the agent's pubkey to the bank's `AuthSettings` with `Read`/`Write(10)` (authoritative) and write a `MemoryBankRef { name, db_id, permission }` row into the agent's `memory_banks` Table (view cache). `/memory grant`/`revoke` keep the two sides consistent via auth-first ordering with best-effort rollback. `MemoryBankIndex` in the central DB tracks banks this peer hosts (same shape/rationale as `AgentIndex`).
- **Agents-I-host index**: Peer-local `hosted_agents` DocStore in central DB (keyed by Agent DB root ID, value = `{db_id, display_name, pubkey}`). Populated at startup via `AgentIndex::sync_from_bootstrap`. Supplies the pubkey→agent lookup for routing (eidetica has no inverse "DBs where key K has permission P" query).
- **Session participation**: Session's AuthSettings is the authoritative participant list — an agent is authorized iff its pubkey has `Permission::Write(_)` on the session DB. `SessionMeta.agents: Vec<AgentRef>` mirrors it as a readable cache; `attach_agent_to_session` writes both at once and appends to the agent DB's history. Detach revokes the auth key and removes the AgentRef; history is append-only.
- **Turn-taking** (`resolve_agent_for_entry`): override > `@mention` in message text (exact case-insensitive display_name match, trailing punctuation trimmed, `a@b.com`-style tokens ignored) > `SessionMeta.host_agent_db_id` (if authorized) > first authorized agent > legacy `SessionMeta.agent_name` > default. Server processes messages through this; UI/display sites use the plain `resolve_agent` (no content).
- **Heartbeat rules**: `HeartbeatRule { id, cron (6 fields), task, target_agent_db_id }` stored inside each session DB as a Table. `HeartbeatRunner` (background task, 30s poll) scans all sessions the peer knows about, fires rules whose target agent is in `agent_index` and whose cron is due vs peer-local `last_fired` (central DB `heartbeat_last_fired` DocStore). Firing writes a `Directive` entry; the mention-aware router handles turn-taking from there.
- **Backend abstraction**: LLMBackend trait with tool support; runtime dispatches through BackendManager. BackendManager carries SecretStore for host-boundary key injection. Backend methods return `Result<_, LlmError>` with structured error classification (retryable/permanent/auth/config). Runtime converts to `String` at the server boundary. All LLM HTTP calls wrapped in `tokio::time::timeout` (configurable per backend, default 120s). Transient errors (429, 5xx, timeout, network) retried with exponential backoff (1s–30s, honors `Retry-After`, configurable `max_retries` default 3).
- **Secret store**: SecretStore backed by eidetica DocStore ("secrets" subtree) with in-memory HashMap cache. API keys extracted from config at startup, persisted to DocStore, only rewritten if changed. Backend structs carry opaque `api_key_ref` IDs, never raw keys. Secrets resolved at HTTP client boundary (`OpenAI::build_client`). Supports env var references: `"${VAR_NAME}"` in config.
- **Command dispatch**: User-facing session commands are parsed to a transport-neutral `commands::Command`, run through `commands::dispatch(cmd, ctx) -> CommandOutcome`, and rendered by the gateway. Both TUI (`/<cmd>`) and Matrix (`!chaz <cmd>`) share the same command set: `sessions`, `info`, `name`, `share`, `sync`, `compact`, `print`, `channels`, `schedules`, `run`, `model`, `role`, `backend`, `backends`, `agent add|remove|list|host|hosted|new|set|delete|share|import` (+ `agents`), `heartbeat add|remove|list`, `memory new|list|delete|grant|revoke|share|import`. `agent new <name> [k=v ...]` accepts `role`/`model`/`tools`/`can_spawn`/`allowed_callers`/`autonomous`/`max_iterations`/`tool_profile`/`max_context_tokens`. `agent set <ref> <field> <value>` edits one field on a running agent's AgentDb (Stage 8 hydration picks it up next message). `memory grant <bank> <agent> <read|write>` writes auth first (bank AuthSettings) then mirrors a MemoryBankRef into the agent's `memory_banks` Table — rolling back the auth on view-write failure. `CommandContext` carries `{server, scheduler, secrets, backend, session_db_id, session_db, current_agent, session_name, config_roles, default_role}`. `CommandOutcome` variants: `Text` / `Error` / `SessionsList` / `SessionSwitched(Box<…>)` / `Quit`. Transport-specific extras stay in the gateway: TUI has `/debug`, `/raw`, `/clear` (display), picker modal, `/quit`; Matrix has `party`, `rename` (room), `send` (no-context), `clear` (history marker), `approve`/`deny`, plus `attach`/`detach` (which need to install the response callback on mutation).
- **Security context**: Built from SecurityConfig, threaded through server to runtime per-session. Contains leak detector, auto-approved tool set, and approval channel from gateway.
- **TUI (ratatui)**: Elm architecture — `App` state struct, `Action` enum, `tokio::select!` event loop over crossterm `EventStream` + session notify + approval channel. Parses slash commands into `Command` and routes through `commands::dispatch`; `SessionsList` outcome opens the picker modal, `SessionSwitched` re-registers session with approval/notify channels. Debug mode (Ctrl+D), inline tool approval (y/n/a).
- **Tool policy**: Tools provide `default_policy()` (risk, approval, timeout, grants). Config `security.tool_policies` overrides per tool. `ToolPolicyRegistry` resolves effective policy. Runtime checks against resolved policy, sends ApprovalExchange to gateway via mpsc channel. Approval decisions: Approve/Deny/ApproveAll. Matrix surfaces approval requests as room messages with reaction support (✅❌⏭) and text commands (!chaz approve/deny).
- **Grants**: Typed capability grants attached to each `ToolPolicy` (`shell`, `network`, `fs` — extensible). Tools read them via `ctx.grants()` at execute time; no tool is constructed with capability-specific args. Config shape: `security.tool_policies.<tool>.grants.<kind>`. Per-agent overlay via `agents[].grants.<tool>`; runtime merges per-kind (`agent > config > default`, replacement not union). Legacy `security.shell_allowlist` / `shell_denylist` / `allowed_endpoints` still parse but are migrated at startup by `grants::merge_legacy_security` with a deprecation `warn!`.
- **ToolContext**: agent_name, call_depth, max_call_depth, tools (ScopedTools), profile (ToolProfile), grants (resolved per call), agent_grants (per-tool overlays from the running agent). The `tools` field carries the transitively-narrowed tool set for this agent — each spawn level intersects the parent's scope with the child's allowed_tools. The `profile` controls how tool definitions are presented to the LLM. `grants` is populated per tool call by merging the tool's resolved-policy grants with `agent_grants.get(tool_name)`.
- **Tool profiles**: Named configurations (in `tool_profiles:` config) controlling tool definition presentation. PresentationMode: Full (default), Brief (first sentence, no param descriptions), Summary (name only), Hidden. Supports glob prefix matching (`"filesystem.*": summary`). Configured per agent (`tool_profile:`), per preset, or per session. Applied at `ScopedTools::definitions()` call.
- **MCP tools**: External tool servers via subprocess JSON-RPC (stdin/stdout) or Streamable HTTP (POST + SSE). Config: `mcp_servers:` with name, command/url, args, env, default_policy. Also `mcp_server_dir:` for manifest directory scanning (.yaml/.json files). Transport selected by config: `url` → HTTP, `command` → stdio. Tools namespaced as `server.tool` (e.g., `filesystem.read_file`). Eagerly started at boot, tools registered in ToolRegistry alongside built-ins. Default policy: Medium/UnlessAutoApproved/60s. `describe_tool` enables discovery when profiles hide details. Auto-restart with exponential backoff (1s–16s, max 5 attempts) on subprocess crash. Dynamic re-discovery: `notifications/tools/list_changed` detected in JSON-RPC exchanges, triggers lazy `tools/list` refresh before next call. McpTool metadata stored in shared `RwLock` map — `descriptor()` returns live data.
- **Tool allowlist globs**: Agent tool allowlists support `"namespace.*"` glob patterns (e.g., `"filesystem.*"` matches all tools from that MCP server). Works in ScopedTools filtering, access checks, and transitive narrowing.
- **Per-session serialization**: Server tracks which sessions have active agent tasks. Concurrent writes to the same session are skipped while an agent is running, preventing duplicate responses.
- **XML tool output wrapping**: Tool results fed back to the LLM are wrapped in `<tool_output tool="name">` delimiters with angle-bracket escaping, preventing injection attacks through tool output.
- **Tool rate limiting**: Per-tool `rate_limit` (calls/minute) in policy config. Sliding-window RateLimiter enforced in the ReAct loop before tool execution.
- **Leak detection**: All tool outputs scanned for 12 secret patterns before entering LLM context. Policy: redact (default) or block.
- **Network policy**: WebFetch builds a `NetworkPolicy` per call from `ctx.grants().network`: endpoint allowlist (empty = allow-all) + SSRF blocking (on by default; `allow_private: true` in the grant opts out).
- **Retry loop**: MatrixGateway retries on all `bot.run()` errors with 5s backoff
- **Config**: Immutable after load, threaded via `Arc<Config>` in Matrix gateway
- **Session config**: Per-session name, agent, model, role, and backend overrides stored as `SessionMeta` in each session's own eidetica DB (under a "meta" DocStore). Config travels with the session via eidetica sync — sharing a session also shares its config.
- **New-session detection**: SessionRegistry emits `NewSessionEvent` on session creation (local and sync'd). Server logs new sessions; consumers can subscribe via `subscribe_new_sessions()`.

## Logging

All logging uses the `tracing` crate (`tracing-subscriber` with default fmt subscriber). Control output via `RUST_LOG`:

```bash
# Default (info+)
chaz --config config.yaml --tui

# Debug — includes tool results, LLM requests, model resolution
RUST_LOG=debug chaz --config config.yaml --tui

# Module-specific
RUST_LOG=chaz::runtime=debug,chaz::security=debug chaz --config config.yaml --tui

# Quiet — errors only
RUST_LOG=error chaz --config config.yaml --tui
```

### Log levels by category

| Level     | What                                                                                                                                                                                                                       |
| --------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **error** | Login/session DB failures, gateway crashes, agent execution errors                                                                                                                                                         |
| **warn**  | Secret leak detection (redact/block), SSRF blocks, network policy denials, shell command denials, approval channel failures, tool execution errors/timeouts, unknown tools, injection patterns detected                    |
| **info**  | Startup sequence (config load, agent/tool registry init, gateway mode), session creation, shell command execution, file writes, web fetches, approval decisions, ReAct loop lifecycle, eidetica sync, MCP server lifecycle |
| **debug** | LLM request/response details, individual tool results, model resolution, file reads, memory store/recall, HTTP response status/size, shell exit codes, MCP JSON-RPC traffic                                                |

### Adding logging to new code

- Import the levels you need: `use tracing::{debug, info, warn, error};`
- Use structured fields: `info!(tool = %name, bytes = len, "File written")`
- Security operations should always log at warn+ when denying/blocking
- Tool execution: info for start, debug for result, warn for errors
- LLM traffic: debug (it's verbose)

## Adding a New Tool

### Built-in (Rust)

1. Create `src/tools/my_tool.rs` implementing the `Tool` trait
2. Add `mod my_tool;` and `pub use` to `src/tools/mod.rs`
3. Register in `main.rs`: `tools.register(tools::MyTool);`

The `Tool` trait requires: `descriptor()` (returns `ToolDescriptor` with name, description, parameters JSON Schema) and `execute()` (returns boxed future for async support). Optional `default_policy()` returns `ToolPolicy` (risk, approval, timeout, sensitive_params, grants) — config overrides via `security.tool_policies` take precedence. `ToolPolicyRegistry` resolves effective policy per tool.

Tools that need capability data (shell commands to allow, network endpoints, fs paths) should read them from `ctx.grants()` at execute time — never take capability-specific constructor args. Add new grant kinds in `src/grants.rs` (`ShellGrant`, `NetworkGrant`, `FsGrant`, …) and reference them via `ctx.grants().<kind>`. Config shape:

```yaml
security:
  tool_policies:
    my_tool:
      risk: high
      approval: always
      grants:
        shell:
          allow: ["git", "ls"]
          deny: ["rm -rf"]
        network:
          endpoints:
            - host: "*.example.com"
              methods: ["GET"]
          allow_private: false
```

Agents can overlay grants per tool (merged per-kind, most specific wins):

```yaml
agents:
  - name: researcher
    grants:
      web_fetch:
        network:
          endpoints:
            - host: "*.wikipedia.org"
```

### External (MCP)

Add to config — no code changes needed:

```yaml
mcp_servers:
  # Subprocess (stdio) transport
  - name: filesystem
    command: npx
    args: ["-y", "@modelcontextprotocol/server-filesystem", "/home/user"]
    env:
      SOME_VAR: "value"
    default_policy:
      risk: medium
      approval: unless_auto_approved
      timeout: 30
  # Streamable HTTP transport
  - name: remote-tools
    url: "http://localhost:8080/mcp"

# Or scan a directory for manifest files (.yaml/.json)
mcp_server_dir: "/etc/chaz/mcp.d"
```

Tools are discovered via MCP `tools/list` at startup and registered as `server_name.tool_name` (e.g., `filesystem.read_file`). Policy overrides work the same as built-in tools via `security.tool_policies`. Use `tool_profiles` to control context usage when many MCP tools are present. Tool schemas update dynamically when servers send `notifications/tools/list_changed`.

## CI

GitHub Actions on push to main and PRs:

- `ci.yml`: nix-fast-build — lint, test, build, doc
- `security-audit.yml`: daily cargo-deny
- Dependency update workflows: cargo-update, flake-update, actions-update (monthly)

## Formatting & Linting

treefmt enforces: `rustfmt` (Rust), `alejandra` (Nix), `prettier` (Markdown/YAML). Clippy denies all warnings in CI.

## Test Instance

- Bot: `@chaz-dev:jackson.dev`
- Config: `~/chaz-test/config.yaml`
- Logs: `~/chaz-test/chaz.log`
- Start: `~/chaz-test/run.sh`
- Backend: OpenRouter (`minimax/minimax-m2.7` default)

### Running the bot

The test instance runs from a release build in the workspace:

```bash
# Build
nix develop .# -c cargo build --release

# Start (foreground)
~/chaz-test/run.sh

# Start (background)
nohup ~/chaz-test/run.sh > ~/chaz-test/chaz.log 2>&1 &

# Check if running
ps aux | grep "chaz.*config" | grep -v grep

# Stop
kill $(pgrep -f "chaz.*config.yaml")

# Check logs
tail -f ~/chaz-test/chaz.log
grep -E "ERROR|Response:|Batching" ~/chaz-test/chaz.log
```

After code changes, rebuild and restart — the bot persists sessions in eidetica SQLite (`~/chaz-test/` state dir), so conversation history survives restarts. The sync token is persisted by headjack, so the bot resumes from where it left off (the router's message batching prevents duplicate responses from the catch-up sync).

### TUI mode

```bash
# Run TUI against the same state dir as the Matrix bot
nix develop .# -c cargo run -- --config ~/chaz-test/config.yaml --tui

# Or against a separate state dir for isolated testing
nix develop .# -c cargo run -- --config ~/chaz-test/config-tui.yaml --tui
```

TUI commands: `/help` for full list. Key ones: `/sessions` (picker), `/name <alias>` (name session), `/join <name|id>` (switch), `/share` (generate ticket), `/sync <ticket>` (sync remote session), `/compact` (summarize and compact context), `/model`, `/role`, `/backend`, `/backends`, `/schedules`, `/run`, `/info`, `/print`, `/debug` (toggle timestamps/types), `/raw` (dump entries).

Matrix has the same set of session commands under `!chaz <cmd>` (sessions, info, name, share, sync, compact, print, schedules, run, model, role, backend, backends/list). Matrix-only: `!chaz party/rename/send/clear/approve/deny`. TUI-only: `/debug`, `/raw`, `/clear` (display), `/quit`, `/new`, `/join`, picker modal.

### Session sharing between instances

Eidetica sync is enabled automatically with an HTTP transport. The server address is logged at startup.

```bash
# On instance A (e.g., the Matrix bot), get a session ticket:
# In TUI: /share
# Output: eidetica:?db=sha256:abc...&pr=http:127.0.0.1:12345

# On instance B (e.g., a local TUI), sync the session:
# In TUI: /sync eidetica:?db=sha256:abc...&pr=http:192.168.1.10:12345
# Then: /sessions to find and open it
```

Both instances must be network-reachable. Writes propagate bidirectionally via eidetica's on_local_write callbacks.
