# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Chaz is an AI agent orchestrator for Matrix written in Rust. It connects to Matrix rooms via headjack/matrix-sdk and responds using OpenAI-compatible LLM backends (e.g., OpenRouter). Features a ReAct tool-calling loop, session-based conversation history (via eidetica), and a TUI mode for testing without Matrix.

**Status**: Active development — Phase 1 (architecture + ReAct loop) and Phase 2 (tools) complete. Working on memory, persistence, and extensibility.

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

No unit tests yet — `cargo test` passes with no meaningful coverage. Build deps: `pkg-config`, `openssl`, `sqlite`.

## Architecture

```
main.rs              CLI args, config, eidetica init, secret store, security context, tool registry, gateway dispatch
config.rs            Config, Backend (api_key_ref → SecretStore), AgentConfig, SecurityConfig types
types.rs             ConversationId (gateway-agnostic)
agent.rs             Agent (with spawn perms, presets), AgentRegistry (Arc-shared, YAML-configurable)
session.rs           SessionRegistry (central DB with bindings) + Session (per-conversation eidetica DB) + EntryType (Message, Directive, ToolCall, ToolResult, Ack, Error)
tool.rs              Tool trait (descriptor + execute + default_policy), ToolDescriptor, ToolPolicy, ToolPolicyRegistry, ToolRegistry, ScopedTools
tools/
  mod.rs             Re-exports all tools
  agent.rs           spawn_agent — delegate to another agent via server's session messaging (sync/async modes)
  time.rs            get_time — current UTC time (Low risk)
  calculate.rs       calculate — math expressions (Low risk)
  shell.rs           shell — execute commands (High risk, approval required, command allow/denylist)
  file.rs            read_file (Low), write_file (Medium, approval unless auto-approved)
  web.rs             web_fetch — HTTP GET/POST (Medium risk, network policy enforced, SSRF protection)
  memory.rs          remember, recall — persistent key-value memory (Low risk)
security/
  mod.rs             SecurityContext (leak detector, auto-approved tools, approval channel)
  secrets.rs         SecretStore — eidetica DocStore-backed secret storage, HashMap cache, env var resolution
  leak_detector.rs   LeakDetector — 12 secret patterns, redact/block policy
  network.rs         NetworkPolicy — endpoint allowlisting, SSRF protection
  sanitizer.rs       Sanitizer — prompt injection detection (warning-only)
runtime.rs           ReAct loop with security: approval gate, timeouts, leak scanning, injection warnings; RuntimeEventSink for audit trail
server.rs            Callback-driven Server: registers on_local_write on session DBs, processing loop, agent task spawning, response delivery, child session management
gateway/
  mod.rs             Gateway trait, ApprovalExchange/ApprovalDecision
  matrix/
    mod.rs           MatrixGateway — lifecycle, sync, retry, text handler
    commands.rs      Matrix-specific commands (!chaz model/role/backend/list/etc.)
    history.rs       Room history reading for backfill
  tui.rs             TuiGateway — ratatui async terminal app, Elm architecture (App/Action/ui)
backends.rs          BackendManager, LLMBackend trait (with tool support), ChatContext, Message
openai.rs            OpenAI-compatible backend implementing LLMBackend
role.rs              Role/system prompt management
defaults.rs          Built-in default config and roles
```

### Key flows

**Message flow (Matrix):** Matrix sync → text handler → writes SessionEntry to session DB → eidetica on_local_write callback fires → server processing loop detects user message → spawns agent task → agent runs full ReAct loop → writes response SessionEntry to session DB → callback fires → server detects agent response → delivers via ResponseDelivery channel → Matrix response task sends to room. Different rooms run in parallel.

**Message flow (TUI):** Input box → writes SessionEntry to session DB → callback fires → server runs agent → writes response → on_local_write sends `()` notify → event loop re-reads session from eidetica → renders updated entries in ratatui terminal.

**ReAct loop:** Build context from session → call LLM with tool definitions → if tool_calls: check approval requirement → if approved: execute with timeout → emit RuntimeEvent → scan output for leaks → scan for injection (warn) → feed results back, loop → if text: return final response. Falls back to simple execution if backend doesn't support tools. Forces a summary if iteration cap (10) is reached. Runtime emits ToolCall/ToolResult events via optional RuntimeEventSink; server writes these to the session DB as audit trail entries.

**spawn_agent:** Writes a Directive entry to a child session → server's on_local_write callback fires → process_session detects Directive → spawns agent task → agent runs ReAct loop → writes response → completion channel signals caller. Supports sync (default) and async (`"async": true`) modes.

### Key patterns

- **Gateway = bridge**: Gateways translate platform events ↔ session DB entries. Each registers its own on_local_write callback to detect agent responses and deliver to its transport. Server is transport-agnostic.
- **Callback-driven server**: Server registers on_local_write callbacks on session DBs. Callback fires → notify channel → processing loop checks latest entry → if non-agent Message or Directive, spawns agent task. Agent writes Ack → runs ReAct loop (emitting ToolCall/ToolResult events to session) → writes response. Global Semaphore(10) caps concurrent LLM calls.
- **Session messaging primitive**: All agent invocation goes through session entries. spawn_agent writes a Directive entry to a child session and awaits completion via the server's callback path (register_child_session + mpsc completion channel). Supports sync and async modes.
- **Entry types**: Message (chat), Directive (instructions to agent — included in LLM context), ToolCall/ToolResult (audit trail — excluded from LLM context), Ack (thinking indicator), Error. Only Message and Directive enter the LLM context window.
- **Eidetica sync**: HTTP transport enabled at startup. `/share` generates DatabaseTicket URLs, `/sync <ticket>` syncs remote sessions. Writes propagate bidirectionally via on_local_write callbacks.
- **Per-session eidetica DBs**: Each conversation gets its own eidetica Database. SessionRegistry (central "chaz-registry" DB) persists transport_id → session DB root ID bindings across restarts.
- **Memory**: eidetica Table store for key-value facts in central "chaz-central" DB (shared, not per-session)
- **Agent registry**: YAML-configurable agents with per-agent tool visibility (ScopedTools with transitive narrowing)
- **Backend abstraction**: LLMBackend trait with tool support; runtime dispatches through BackendManager. BackendManager carries SecretStore for host-boundary key injection.
- **Secret store**: SecretStore backed by eidetica DocStore ("secrets" subtree) with in-memory HashMap cache. API keys extracted from config at startup, persisted to DocStore, only rewritten if changed. Backend structs carry opaque `api_key_ref` IDs, never raw keys. Secrets resolved at HTTP client boundary (`OpenAI::build_client`). Supports env var references: `"${VAR_NAME}"` in config.
- **Matrix commands**: `!chaz model/role/backend/list/clear/rename/send/print` handled directly in MatrixGateway, bypass the server
- **Security context**: Built from SecurityConfig, threaded through server to runtime per-session. Contains leak detector, auto-approved tool set, and approval channel from gateway.
- **TUI (ratatui)**: Elm architecture — `App` state struct, `Action` enum, `tokio::select!` event loop over crossterm `EventStream` + session notify + approval channel. Supports session picker, debug mode (Ctrl+D), session sharing (/share, /sync), and slash commands (/sessions, /new, /join, /info, /raw, /clear). Renders all entry types with distinct styles. Tool approval inline with y/n/a keys.
- **Tool policy**: Tools provide `default_policy()` (risk, approval, timeout). Config `security.tool_policies` overrides per tool. `ToolPolicyRegistry` resolves effective policy. Runtime checks against resolved policy, sends ApprovalExchange to gateway via mpsc channel. Approval decisions: Approve/Deny/ApproveAll.
- **ToolContext**: agent_name, call_depth, max_call_depth, tools (ScopedTools). The `tools` field carries the transitively-narrowed tool set for this agent — each spawn level intersects the parent's scope with the child's allowed_tools.
- **Leak detection**: All tool outputs scanned for 12 secret patterns before entering LLM context. Policy: redact (default) or block.
- **Network policy**: WebFetch enforces endpoint allowlisting and SSRF protection. Private IPs always blocked.
- **Retry loop**: MatrixGateway retries on all `bot.run()` errors with 5s backoff
- **Config**: Immutable after load, threaded via `Arc<Config>` in Matrix gateway
- **Room tags**: `is.chaz.*` namespace for per-room model/role/backend persistence

## Adding a New Tool

1. Create `src/tools/my_tool.rs` implementing the `Tool` trait
2. Add `mod my_tool;` and `pub use` to `src/tools/mod.rs`
3. Register in `main.rs`: `tools.register(tools::MyTool);`

The `Tool` trait requires: `descriptor()` (returns `ToolDescriptor` with name, description, parameters JSON Schema) and `execute()` (returns boxed future for async support). Optional `default_policy()` returns `ToolPolicy` (risk, approval, timeout, sensitive_params) — config overrides via `security.tool_policies` take precedence. `ToolPolicyRegistry` resolves effective policy per tool.

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

TUI commands: `/help` for full list. Key ones: `/sessions` (picker), `/share` (generate ticket), `/sync <ticket>` (sync remote session), `/debug` (toggle timestamps/types), `/raw` (dump entries).

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
