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
main.rs              CLI args, config, eidetica init, tool registry, gateway selection
config.rs            Config, Backend types, GLOBAL_CONFIG/GLOBAL_MESSAGES
types.rs             ConversationId
agent.rs             Agent config (role, model defaults)
session.rs           SessionManager + Session (eidetica-backed message history)
tool.rs              Tool trait, ToolDefinition, ToolRegistry
tools/
  mod.rs             Re-exports all tools
  time.rs            get_time — current UTC time
  calculate.rs       calculate — math expressions (meval)
  shell.rs           shell — execute commands (FIXME: unsandboxed)
  file.rs            read_file, write_file — filesystem access
  web.rs             web_fetch — HTTP GET/POST
runtime.rs           ReAct loop: context → LLM → parse tool calls → execute → loop
router.rs            Dispatches ChatRequests, manages sessions, calls runtime
gateway/
  mod.rs             ChatRequest, ChatResponse types
  matrix.rs          MatrixGateway — headjack integration, commands, text handler
  tui.rs             TuiGateway — stdin/stdout for testing (--tui flag)
backends.rs          BackendManager, LLMBackend trait, ChatContext, Message
openai.rs            OpenAI-compatible backend + chat_with_tools for ReAct loop
role.rs              Role/system prompt management
defaults.rs          Built-in default config and roles
```

### Key flows

**Message flow (Matrix):** Matrix sync → text handler → read room tags for model/role → send ChatRequest via channel → router adds to session → runtime runs ReAct loop → response sent back via oneshot → gateway sends to room.

**Message flow (TUI):** stdin → ChatRequest → router → runtime → stdout.

**ReAct loop:** Build context from session → call LLM with tool definitions → if tool_calls: execute tools, feed results back, loop → if text: return final response. Falls back to simple execution if backend doesn't support tools. Forces a summary if iteration cap (10) is reached.

### Key patterns

- **Channel-based dispatch**: Gateway → Router via mpsc, responses via oneshot per request
- **Sequential router**: One request at a time to prevent session state races
- **Session history**: eidetica InMemory backend (SQLite blocked by libsqlite3-sys version conflict)
- **Matrix commands**: `!chaz model/role/backend/list/clear/rename/send/print` handled directly in MatrixGateway, bypass the router
- **Retry loop**: MatrixGateway retries on all `bot.run()` errors with 5s backoff
- **Global state**: `lazy_static` for GLOBAL_CONFIG and GLOBAL_MESSAGES (rate limiting)
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
