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
main.rs              CLI args, config, eidetica init, tool registry, gateway dispatch
config.rs            Config, Backend, AgentConfig types (immutable after load, no globals)
types.rs             ConversationId (gateway-agnostic)
agent.rs             Agent, AgentRegistry (YAML-configurable, per-agent tool visibility)
session.rs           SessionManager + Session (eidetica SQLite, transport_id binding registry)
tool.rs              Tool trait, ToolRegistry, FilteredTools (per-agent view)
tools/
  mod.rs             Re-exports all tools
  time.rs            get_time — current UTC time
  calculate.rs       calculate — math expressions (meval)
  shell.rs           shell — execute commands (FIXME: unsandboxed)
  file.rs            read_file, write_file — filesystem access
  web.rs             web_fetch — HTTP GET/POST
  memory.rs          remember, recall — persistent key-value memory (eidetica-backed)
runtime.rs           ReAct loop: context → LLM → parse tool calls → execute → loop
router.rs            Resolves transport_id → conversation, selects agent, dispatches to runtime
gateway/
  mod.rs             Gateway trait, ChatRequest/ChatResponse types
  matrix/
    mod.rs           MatrixGateway — lifecycle, sync, retry, text handler
    commands.rs      Matrix-specific commands (!chaz model/role/backend/list/etc.)
    history.rs       Room history reading for backfill
  tui.rs             TuiGateway — stdin/stdout for testing (--tui flag)
backends.rs          BackendManager, LLMBackend trait (with tool support), ChatContext, Message
openai.rs            OpenAI-compatible backend implementing LLMBackend
role.rs              Role/system prompt management
defaults.rs          Built-in default config and roles
```

### Key flows

**Message flow (Matrix):** Matrix sync → text handler → read room tags for model/role → send ChatRequest (with transport_id) via channel → router resolves transport_id → ConversationId → selects agent → adds to session → runtime runs ReAct loop with filtered tools → response sent back via oneshot → gateway sends to room.

**Message flow (TUI):** stdin → ChatRequest → router → runtime → stdout.

**ReAct loop:** Build context from session → call LLM with tool definitions → if tool_calls: execute tools, feed results back, loop → if text: return final response. Falls back to simple execution if backend doesn't support tools. Forces a summary if iteration cap (10) is reached.

### Key patterns

- **Gateway trait**: Both MatrixGateway and TuiGateway implement `Gateway` trait with `run()` method
- **Channel-based dispatch**: Gateway → Router via mpsc, responses via oneshot per request
- **Sequential router**: One request at a time to prevent session state races
- **Transport ID binding**: Gateways send native transport IDs, SessionManager resolves to ConversationId
- **Session history**: eidetica SQLite backend for persistent storage
- **Memory**: eidetica Table store for key-value facts, shared database with sessions
- **Agent registry**: YAML-configurable agents with per-agent tool visibility (FilteredTools)
- **Backend abstraction**: LLMBackend trait with tool support; runtime dispatches through BackendManager
- **Matrix commands**: `!chaz model/role/backend/list/clear/rename/send/print` handled directly in MatrixGateway, bypass the router
- **Retry loop**: MatrixGateway retries on all `bot.run()` errors with 5s backoff
- **Config**: Immutable after load, threaded via `Arc<Config>` in Matrix gateway
- **Room tags**: `is.chaz.*` namespace for per-room model/role/backend persistence

## Adding a New Tool

1. Create `src/tools/my_tool.rs` implementing the `Tool` trait
2. Add `mod my_tool;` and `pub use` to `src/tools/mod.rs`
3. Register in `main.rs`: `tools.register(tools::MyTool);`

The `Tool` trait requires: `name()`, `description()`, `parameters()` (JSON Schema), and `execute()` (returns boxed future for async support).

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
