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
session.rs           SessionRegistry (central DB with bindings) + Session (per-conversation eidetica DB)
tool.rs              Tool trait (with ToolContext), ToolRegistry, FilteredTools, RiskLevel, ApprovalRequirement
tools/
  mod.rs             Re-exports all tools
  agent.rs           spawn_agent — delegate to another agent in a fresh session (Medium risk)
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
runtime.rs           ReAct loop with security: approval gate, timeouts, leak scanning, injection warnings; receives ToolContext
router.rs            Resolves transport_id → conversation, selects agent per-conversation, builds ToolContext
gateway/
  mod.rs             Gateway trait, ChatRequest/ChatResponse, ApprovalExchange/ApprovalDecision
  matrix/
    mod.rs           MatrixGateway — lifecycle, sync, retry, text handler
    commands.rs      Matrix-specific commands (!chaz model/role/backend/list/etc.)
    history.rs       Room history reading for backfill
  tui.rs             TuiGateway — stdin/stdout, interactive tool approval (y/n/all)
backends.rs          BackendManager, LLMBackend trait (with tool support), ChatContext, Message
openai.rs            OpenAI-compatible backend implementing LLMBackend
role.rs              Role/system prompt management
defaults.rs          Built-in default config and roles
```

### Key flows

**Message flow (Matrix):** Matrix sync → text handler → read room tags for model/role → send ChatRequest (with transport_id) via channel → router spawns tokio task → task opens/creates session DB from registry → loads session → resolves agent → acquires semaphore → runtime runs full ReAct loop with filtered tools → writes response to session → response sent back via oneshot → gateway sends to room. Different rooms run in parallel.

**Message flow (TUI):** stdin → ChatRequest → router → spawned task → runtime → stdout.

**ReAct loop:** Build context from session → call LLM with tool definitions → if tool_calls: check approval requirement → if approved: execute with timeout → scan output for leaks → scan for injection (warn) → feed results back, loop → if text: return final response. Falls back to simple execution if backend doesn't support tools. Forces a summary if iteration cap (10) is reached.

### Key patterns

- **Gateway trait**: Both MatrixGateway and TuiGateway implement `Gateway` trait with `run()` method
- **Channel-based dispatch**: Gateway → Router via mpsc, responses via oneshot per request
- **Task-per-message router**: Each incoming message spawns a tokio task. Task opens session DB from registry, loads messages, runs full ReAct loop, writes back. Global Semaphore(10) caps concurrent LLM calls. No persistent workers.
- **Per-session eidetica DBs**: Each conversation gets its own eidetica Database. SessionRegistry (central "chaz-registry" DB) persists transport_id → session DB root ID bindings across restarts.
- **Memory**: eidetica Table store for key-value facts in central "chaz-central" DB (shared, not per-session)
- **Agent registry**: YAML-configurable agents with per-agent tool visibility (FilteredTools)
- **Backend abstraction**: LLMBackend trait with tool support; runtime dispatches through BackendManager. BackendManager carries SecretStore for host-boundary key injection.
- **Secret store**: SecretStore backed by eidetica DocStore ("secrets" subtree) with in-memory HashMap cache. API keys extracted from config at startup, persisted to DocStore, only rewritten if changed. Backend structs carry opaque `api_key_ref` IDs, never raw keys. Secrets resolved at HTTP client boundary (`OpenAI::build_client`). Supports env var references: `"${VAR_NAME}"` in config.
- **Matrix commands**: `!chaz model/role/backend/list/clear/rename/send/print` handled directly in MatrixGateway, bypass the router
- **Security context**: Built from SecurityConfig, threaded through router to runtime per-request. Contains leak detector, auto-approved tool set, and approval channel from gateway.
- **Tool approval flow**: Tools declare risk level and approval requirement. Runtime checks SecurityContext, sends ApprovalExchange to gateway via mpsc channel, gateway prompts user (TUI: stdin, Matrix: deferred). Approval decisions: Approve/Deny/ApproveAll.
- **Leak detection**: All tool outputs scanned for 12 secret patterns before entering LLM context. Policy: redact (default) or block.
- **Network policy**: WebFetch enforces endpoint allowlisting and SSRF protection. Private IPs always blocked.
- **Retry loop**: MatrixGateway retries on all `bot.run()` errors with 5s backoff
- **Config**: Immutable after load, threaded via `Arc<Config>` in Matrix gateway
- **Room tags**: `is.chaz.*` namespace for per-room model/role/backend persistence

## Adding a New Tool

1. Create `src/tools/my_tool.rs` implementing the `Tool` trait
2. Add `mod my_tool;` and `pub use` to `src/tools/mod.rs`
3. Register in `main.rs`: `tools.register(tools::MyTool);`

The `Tool` trait requires: `name()`, `description()`, `parameters()` (JSON Schema), and `execute()` (returns boxed future for async support). Optional security methods with defaults: `risk_level()` (Low), `requires_approval()` (Never), `execution_timeout()` (60s), `sensitive_params()` (none). Override these for tools with side effects or security implications.

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
