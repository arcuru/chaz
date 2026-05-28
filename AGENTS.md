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

Tests: `CARGO_TARGET_DIR=target-test cargo test` (separate target dir avoids contention with `just build`; runs both workspace members). Add `-p chaz-core` or `-p chaz` to scope to one crate. rustls-based — no openssl/sqlite system deps; `pkg-config` comes from the Nix shell.

## Workspace

Two-crate Cargo workspace.

- **`crates/lib/`** — `chaz-core` library. Runtime, tools, extensions, session model, security, MCP, backends, commands, sandbox hosts, gateway trait + approval types. The testable surface (~10k lines).
- **`crates/bin/`** — `chaz` binary. Entrypoint (`main.rs`) and the concrete gateway implementations (Matrix, TUI, CLI). Structurally hard to test without mocking matrix-sdk / ratatui (~3k lines).

Shared dependency versions live in the workspace-root `Cargo.toml` `[workspace.dependencies]` block; each crate pulls them with `workspace = true`.

## Source Map

Paths below are relative to the crate's `src/` (e.g. `agent.rs` → `crates/lib/src/agent.rs`).

### `chaz-core` (crates/lib/src/)

```
commands.rs          Transport-neutral session commands: Command, CommandContext, CommandOutcome, dispatch()
config.rs            Config, Backend, AgentConfig, SecurityConfig
types.rs             ConversationId
agent.rs             Agent + AgentRegistry (runtime view; bridged to Agent DBs by display_name)
agent_db.rs          Living Agents — AgentDb (config/memory/meta/history/memory_banks stores)
db_kind.rs           meta.kind + display_name markers on entity DBs (agent/bank/session classification)
hosted_index.rs      In-memory peer-local pubkey/name → DB index, built at startup from user.databases()
memory_bank_db.rs    Standalone memory bank DBs (parallel to agent_db)
extensions/schedule.rs  /schedule command + schedule_* tools (agent-owned Schedules, agent_db `schedules` store)
extensions/agent_schedule.rs  RoutineHandler that runs the standalone agent-owned schedule fire path
routine/             RoutineEngine — sleep-until-next driver for cron + one-shot Routines (global + per-agent Schedules)
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
gateway.rs           Gateway trait + ApprovalExchange + ApprovalDecision — concrete impls live in the bin crate
error.rs             Error + LlmError (retryable/permanent classification)
backends.rs          BackendManager, LLMBackend trait, ChatContext, Message
openai.rs            OpenAI-compatible backend
extension/           Extension framework: Scope, ScopeCtx, PeerHandles, HookKind, ExtensionHub (per-agent instance model)
extensions/          Built-in extensions: schedule, agent_schedule, memory, skills, mcp, agent_state, …
defaults.rs          Built-in default config and built-in agents (chaz, chazmina, bash, fish, zsh, nu)
test_support/        #[cfg(test)] harness: MockBackend, MockHost, fresh_session, fresh_session_registry, permissive_security
```

### `chaz` binary (crates/bin/src/)

```
main.rs              CLI args, config, eidetica init, secret store, security context, tool registry, gateway dispatch
gateway/cli.rs       CliGateway — one-shot --cli prompt runner
gateway/matrix/      MatrixGateway — matrix-sdk login/sync + session bridging
gateway/tui/         TuiGateway — ratatui-based local interactive surface
```

## Key Invariants

- **AgentDb is the runtime source of truth.** YAML `agents:` is a first-boot template only; `bootstrap_from_config` does not overwrite existing AgentDb config. Use `/agent set` for live edits.
- **Two peer-local DBs, both never sync.** `chaz_group` holds group-level routing/metadata (`sessions`, `matrix_channels`, `session_names`); `chaz_peer` holds peer-runtime state (`credentials`, `schedule_state`). Sync-ful state lives in per-entity DBs.
- **Hosted-agent / hosted-bank lookups are in-memory only.** `hosted_index::HostedIndex` is built at startup by walking eidetica's `user.databases()` and reading each DB's `meta.kind` marker. No persistent mirror — eidetica's key store is the single source of truth for "which DBs does this peer host."
- **Authorization = key possession.** Session participation is gated by AuthSettings on the session DB; memory bank access is gated by AuthSettings on the bank DB. No capability flags.
- **Per-session serialization.** Concurrent writes to the same session while an agent is running are skipped (prevents duplicate responses).
- **Tools access system resources through `ToolHost`.** The host (`ctx.host()`) enforces grants at the capability boundary. Tools request capabilities (Shell, FileRead, FileWrite, HttpRequest) rather than calling OS APIs directly. New capability types go in `tool_host.rs`.
- **Context entries**: only `Message`, `Directive`, and `Summary` enter the LLM context window. `ToolCall`/`ToolResult`/`Ack`/`Error` are audit-only.
- **System prompts rebuild every turn from `AgentDbConfig`.** `system_prompt` + `system_prompt_files` live on the agent's DB config; ContextBuilder assembles them fresh on each turn, plus any `PromptAugmentation` contributions from the extension hub (skills, memory recall, …). Disk edits to a `system_prompt_files` path require a re-write via `/agent set` to be re-read. No per-session snapshot layer — the previous `PersonaSnapshot` entry type, `persona.rs`, and `role.rs` were all deleted; legacy `role:` configs surface a deprecation message pointing to `/agent set <name> system_prompt <text>`.

## Test Instance

Live Matrix bot for end-to-end testing:

- Bot: `@chaz-dev:jackson.dev`
- Config: `~/code/chaz-test/config.yaml`
- Logs: `~/code/chaz-test/chaz.log`
- Start: `~/code/chaz-test/run.sh` (foreground) or `nohup ... &` for background
- Stop: `kill $(pgrep -f "chaz.*config.yaml")`
- Check: `ps aux | grep "chaz.*config" | grep -v grep`
- Backend: OpenRouter (`minimax/minimax-m2.7` default)

After code changes, rebuild (`nix develop .# -c cargo build --release`) and restart. Eidetica state and the Matrix sync token persist across restarts.

TUI against the same state dir:

```bash
nix develop .# -c cargo run -- --config ~/code/chaz-test/config.yaml --tui
```

## Conventions

- Logging via `tracing` (`use tracing::{debug, info, warn, error}`); structured fields preferred. See `docs/src/user_guide/logging.md` for level conventions.
- Adding a tool: see `docs/src/user_guide/tools.md` and `docs/src/architecture/tools.md`.
- Adding an MCP server: config-only, no code. See `docs/src/user_guide/mcp.md`.
- Commit author: "Patrick Jackson <patrick@jackson.dev>"; add `Co-Authored-By` trailer for the AI tool used.

## User-facing feature docs checklist

Every new user-facing feature lands with three docs pieces — not one or two, three. Reference _and_ example _and_ model. The feature isn't done without them; surfacing complex moving parts is a top-level project goal.

1. **Reference** (`docs/src/user_guide/<area>.md`) — terse command table or settings list. What each knob does in one line. Existing tables under "Lifecycle, sharing, and co-ownership" in `agents.md` are the shape.
2. **Conceptual section** (`docs/src/user_guide/<area>.md`) — a `##` section that explains _the model_ the user has to hold in their head: why it exists, what's automatic vs. what they touch, where the state lives, the failure modes. One per feature, headed memorably so cross-links work.
3. **Walkthrough** (usually `docs/src/user_guide/session_sharing.md` or the area's own guide) — a numbered scenario that exercises the happy path AND at least one failure/recovery path with verbatim sample output. The example is what makes the feature discoverable to someone scanning for "can chaz do X?"

Architecture-level notes go in `docs/src/architecture/<area>.md` (the mental model for someone reading the code); design rationale and alternatives-considered go in `docs/src/design/<feature>.md`. Those are separate from the user-guide triad above.

Worked example: home-peer execution ownership ships as the `Execution ownership (home peer)` section in `agents.md` (reference + conceptual), the `Co-owning an Agent Across Two Peers` walkthrough in `session_sharing.md`, and the `Home Peer (Per-Session, with Agent-Level Fresh-Timer Default)` section in `architecture/sessions.md`.

Verify with `just doc build` before commit — mdbook warns on broken cross-links and unclosed tags.
