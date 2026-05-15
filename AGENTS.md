# CLAUDE.md

Guidance for Claude Code when working in this repository.

## Project Overview

Chaz is an AI agent orchestrator for Matrix written in Rust. It connects to Matrix rooms via headjack/matrix-sdk and responds using OpenAI-compatible LLM backends. Features a ReAct tool-calling loop, session-based conversation history (eidetica), Living Agents (per-agent eidetica DBs), Memory Banks, and a TUI mode.

**For deep detail, query `./docs/src/`** — it is the canonical reference:

- `user_guide/` — getting started, config, TUI, Matrix, tools, MCP, agents, memory, security, logging, session sharing
- `architecture/` — overview, session model, ReAct runtime, tool system
- `design/` — session messaging primitive

This file is just orientation. Don't duplicate doc content here.

## Build & Test

Nix flakes + treefmt. Enter dev shell with `direnv allow` or `nix develop .#` (the trailing `.#` is needed because eidetica's flake gets picked up otherwise). `justfile` is the task runner:

```bash
just build          # cargo build
just test           # cargo test (alias: just t)
just lint           # clippy (denies all warnings in CI)
just fmt            # treefmt (rustfmt, alejandra, prettier)
just ci             # lint + fmt + test + nix build
```

Tests: `CARGO_TARGET_DIR=target-test cargo test --bin chaz` (separate target dir avoids contention with `just build`). rustls-based — no openssl/sqlite system deps; `pkg-config` comes from the Nix shell.

## Source Map

```
main.rs              CLI args, config, eidetica init, secret store, security context, tool registry, gateway dispatch
commands.rs          Transport-neutral session commands: Command, CommandContext, CommandOutcome, dispatch()
config.rs            Config, Backend, AgentConfig, SecurityConfig
types.rs             ConversationId
agent.rs             Agent + AgentRegistry (runtime view; bridged to Agent DBs by display_name)
agent_db.rs          Living Agents — AgentDb (config/memory/meta/history/memory_banks stores)
db_kind.rs           meta.kind + display_name markers on entity DBs (agent/bank/session classification)
hosted_index.rs      In-memory peer-local pubkey/name → DB index, built at startup from user.databases()
memory_bank_db.rs    Standalone memory bank DBs (parallel to agent_db)
heartbeat.rs         sweep_for_agent helper — per-session heartbeats are Routine rows fired by routine/
routine/             RoutineEngine — sleep-until-next driver for cron + one-shot Routines (global + per-session)
session.rs           SessionRegistry, Session, EntryType, SessionMeta, attach/detach, resolve_agent
context.rs           ContextBuilder — token-budgeted context assembly (tiktoken)
tool.rs              Tool trait, ToolPolicy, ToolRegistry, ScopedTools, ToolProfile, ToolError
tool_host.rs         ToolHost trait — sandboxed capability boundary (native, future WASM/bwrap)
grants.rs            Typed capability grants (shell/network/fs)
mcp/                 MCP integration (parse, transport, server) — stdio + Streamable HTTP
tools/               Built-in tools: agent, task, compact, describe, time, calculate, shell, file, web, search, memory
security/            SecurityContext, SecretStore, LeakDetector, NetworkPolicy, Sanitizer
runtime.rs           ReAct loop: approval, timeouts, leak/injection scanning, retry, loop detection
server.rs            Callback-driven Server: on_local_write → process_session → spawn agent → deliver response
gateway/             matrix/ + tui/ — translate platform events ↔ session DB entries
error.rs             Error + LlmError (retryable/permanent classification)
backends.rs          BackendManager, LLMBackend trait, ChatContext, Message
openai.rs            OpenAI-compatible backend
persona.rs           Persona + ResolvedPersona + PersonaSnapshotPayload — file-include + inline system prompts
role.rs              Deprecated role lookup (one-release migration window for legacy `roles:` configs)
defaults.rs          Built-in default config and built-in agents (chaz, chazmina, bash, fish, zsh, nu)
```

## Key Invariants

- **AgentDb is the runtime source of truth.** YAML `agents:` is a first-boot template only; `bootstrap_from_config` does not overwrite existing AgentDb config. Use `/agent set` for live edits.
- **Two peer-local DBs, both never sync.** `chaz_group` holds group-level routing/metadata (`sessions`, `matrix_channels`, `session_names`); `chaz_peer` holds peer-runtime state (`credentials`, `heartbeat_last_fired`, `schedule_state`). Sync-ful state lives in per-entity DBs.
- **Hosted-agent / hosted-bank lookups are in-memory only.** `hosted_index::HostedIndex` is built at startup by walking eidetica's `user.databases()` and reading each DB's `meta.kind` marker. No persistent mirror — eidetica's key store is the single source of truth for "which DBs does this peer host."
- **Authorization = key possession.** Session participation is gated by AuthSettings on the session DB; memory bank access is gated by AuthSettings on the bank DB. No capability flags.
- **Per-session serialization.** Concurrent writes to the same session while an agent is running are skipped (prevents duplicate responses).
- **Tools access system resources through `ToolHost`.** The host (`ctx.host()`) enforces grants at the capability boundary. Tools request capabilities (Shell, FileRead, FileWrite, HttpRequest) rather than calling OS APIs directly. New capability types go in `tool_host.rs`.
- **Context entries**: only `Message`, `Directive`, and `Summary` enter the LLM context window. `ToolCall`/`ToolResult`/`Ack`/`Error` are audit-only.
- **PersonaSnapshot is the system-prompt source of truth.** Once written (at agent attach, on `/agent persona bump`, or on `/agent set <ref> persona.*`), the snapshot's `text` is what ContextBuilder injects as the LLM system message — disk edits to a persona's source files do **not** silently mutate ongoing sessions. The legacy `default_role`/`role:` flow only applies to sessions that predate any snapshot.

## Test Instance

Live Matrix bot for end-to-end testing:

- Bot: `@chaz-dev:jackson.dev`
- Config: `~/chaz-test/config.yaml`
- Logs: `~/chaz-test/chaz.log`
- Start: `~/chaz-test/run.sh` (foreground) or `nohup ... &` for background
- Stop: `kill $(pgrep -f "chaz.*config.yaml")`
- Check: `ps aux | grep "chaz.*config" | grep -v grep`
- Backend: OpenRouter (`minimax/minimax-m2.7` default)

After code changes, rebuild (`nix develop .# -c cargo build --release`) and restart. Eidetica state and the Matrix sync token persist across restarts.

TUI against the same state dir:

```bash
nix develop .# -c cargo run -- --config ~/chaz-test/config.yaml --tui
```

## Conventions

- Logging via `tracing` (`use tracing::{debug, info, warn, error}`); structured fields preferred. See `docs/src/user_guide/logging.md` for level conventions.
- Adding a tool: see `docs/src/user_guide/tools.md` and `docs/src/architecture/tools.md`.
- Adding an MCP server: config-only, no code. See `docs/src/user_guide/mcp.md`.
- Commit author: "Patrick Jackson <patrick@jackson.dev>"; add `Co-Authored-By` trailer for the AI tool used.
