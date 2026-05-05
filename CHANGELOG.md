# Changelog

All notable changes to this project will be documented in this file.

## [0.3.1] - 2026-05-05

### AGENTS.md

- Trim to orientation, point at ./docs

### Bug Fixes

- Replace removed nodePackages.prettier with prettier
- Use published headjack version instead of path dependency
- Use fenix rustfmt in treefmt to match crane CI
- Resolve rustfmt conflict and add nix cache
- Correct magic-nix-cache-action SHA pin
- Retry on transient Matrix sync errors instead of crashing
- Retry on all Matrix sync errors, not just timeouts
- Force summary response when tool iteration cap is reached
- Batch concurrent messages to prevent duplicate LLM responses
- SSH key redaction ordering and IPv6 SSRF bracket parsing
- Annotate code blocks for mdbook test, add mdbook to devShell
- CI clean — clippy, cargo-deny, prettier
- Persist scheduler last_run to eidetica, use it for dedup
- No-tools fallback path uses RuntimeMessages instead of empty ChatContext
- Per-session serialization prevents duplicate agent responses
- Switch DDG to POST and use browser UA
- Resolve clippy warnings blocking CI

### Docs

- Agent lifecycle walkthrough + dedicated memory page

### Documentation

- Add baibot link
- Add Docker logging config to prevent huge log files
- Update CLAUDE.md for current architecture
- Update architecture docs for eidetica persistence
- Update architecture docs for Phase 3.5 completion
- Update architecture docs with message batching
- Document how to run the test bot instance
- Update CLAUDE.md for Phase 3.8 security foundations
- Update CLAUDE.md for DocStore-backed SecretStore
- Note that SecretStore is not encrypted at rest
- Update architecture docs for parallel session model (Phase 4.4)
- Update architecture for callback-driven server (Phase 5.2+5.4)
- Update plans for fully callback-driven architecture
- Comprehensive README rewrite, CLAUDE.md updates
- Add mdbook documentation site
- Update architecture docs for ContextBuilder and compact
- Update CLAUDE.md for recent features
- Comprehensive update for recent features
- ToolError section in architecture/tools.md
- Refresh architecture overview + source layout for Phase 18
- Add agent + memory share, co-owner bootstrap, troubleshooting
- Document the request/approve flow
- Update sharing/sync docs and TUI help for new commands
- Document tags, BM25, and embedding-based recall
- Explain where embedding settings live + link from setup
- Cap docker log size in compose example
- Reframe project as eidetica-based agent framework
- Document text-only ingestion limitation

### Features

- Phase 1 agent orchestrator architecture
- Phase 2 tools — shell, file, and web
- Phase 3 — session persistence, context truncation, memory tools
- Unify storage in eidetica with disk persistence
- Switch to SQLite-backed eidetica for persistent storage
- Backfill session history from Matrix room messages
- Add security foundations (Phase 3.8)
- Add SecretStore for host-boundary API key injection (Phase 3.8.2)
- Back SecretStore with eidetica DocStore for persistence
- Extend agent definitions for multi-agent spawning (Phase 4.1)
- Per-conversation agent selection (Phase 4.1)
- Add ToolContext to Tool::execute for spawn_agent support (Phase 4.2)
- Implement spawn_agent tool for multi-agent delegation (Phase 4.2)
- Add depth limiting to spawn_agent, update docs
- Parallel conversation processing with per-session eidetica DBs (Phase 4.4)
- Participant-agnostic SessionEntry model (Phase 5.1)
- Callback-driven server replaces router (Phase 5.2+5.4)
- Ratatui TUI, tool system unification, transitive narrowing (Phase 5.3+5.5+5.6)
- Session messaging primitive — unified agent invocation
- TUI session picker, status bar, and commands
- TUI debug mode, /raw dump, and improved commands
- TUI polish — status bar agent name, /clear, PageUp/PageDown
- Eidetica sync + shareable session tickets
- Add scheduled runs — cron-driven directive injection into sessions
- MCP subprocess tools, tool profiles, and context compaction
- Named sessions — human-friendly aliases for session IDs
- MCP server auto-restart with exponential backoff
- Replace chars/4 token heuristic with tiktoken (cl100k_base)
- Glob patterns in agent tool allowlists
- XML delimiter wrapping for tool outputs (injection defense)
- Per-agent memory isolation
- Per-tool rate limiting with sliding window
- Add sync_listen field for optional HTTP sync transport
- Add /sharing status and /unshare commands
- Tags + BM25 ranked recall
- Per-model embedding subtrees + hybrid recall
- Add ToolHost trait for sandboxed capability boundary
- Add BubblewrapToolHost and WasmEngine for sandboxed execution
- Add --cli mode for single-shot prompt/response
- Auto-approve shell and write_file tools in --cli mode
- Add edit_file tool for precise text replacement
- Trigger agent execution on remote peer writes

### Miscellaneous Tasks

- Modernize infrastructure with flake-parts, treefmt, and eidetica-style CI
- Relicense from MIT to AGPL-3.0-or-later
- Multi-session tabs with mouse + keyboard nav
- Apply treefmt formatting sweep
- Add SearxNG backend
- Point headjack git dep at arcuru/headjack branch=vibe
- Restore codeberg mirror workflow
- Pin eidetica to a rev instead of tracking default branch
- Pin headjack to a rev for cutover decoupling

### README

- Memory Banks tools in the built-in table

### Refactor

- Eliminate GLOBAL_CONFIG and GLOBAL_MESSAGES
- Define Gateway trait and split matrix.rs into focused modules
- Decouple transport IDs from ConversationId
- Move tool support into LLMBackend trait
- Agent definitions in config with per-agent tool filtering
- Fully callback-driven — remove response delivery channel
- Remove dead code — build_context, context_to_messages, MAX_CONTEXT_MESSAGES
- Decouple runtime from ChatContext — route by model name
- Remove unused model field from ContextBuilder
- Derive Default on Config, eliminate blank_config boilerplate
- Drop legacy fallback in AgentRegistry::from_config

### Styling

- Reformat imports for rustfmt 1.8
- Apply formatter fixes from CI run
- Replace .err().expect() with .expect_err()

### TUI

- Enable mouse capture + scroll-wheel scrolling
- Overlay infrastructure + mouse-clickable help popup
- Move tool approval into a fixed panel with clickable buttons
- Clickable session picker

### Testing

- Cover fallback paths and OpenAI HTTP shape

### Build

- Don't pin openapi-api-rs version
- Run cargo update
- Bump all the deps
- Adding an equivalent justfile

### Deps

- Bump eidetica to 93f8d4c0, use User::enable_sync API

### Web_search

- Ordered backend list with failover
- Add Kagi backend

<!-- generated by git-cliff -->

## [0.3.0](https://github.com/arcuru/chaz/compare/v0.2.0...v0.3.0) - 2024-10-25

### Other

- set headjack dep to the released version

## [0.2.0] - 2024-04-07

### Bug Fixes

- Update the prompts
- Improve the summarizations
- Only use the most recent model command
- Expose the aichat-git package for any consumers
- Hide the party command as an easter egg
- Handle tilde safely

### Features

- Support a separate aichat config directory
- Add room renaming
- Add a clear command to reset the session
- Parse the default model from aichat --info
- Allow passing files to the models
- Adding a systemd service config for home-manager
- Checking in aichat-git for the nix build
- Make the state directory configurable
- Send the userid to the callbacks

### Miscellaneous Tasks

- Bump the version to v0.2.0
- Split out headjack

### Styling

- Adjust some of the responses in listing/setting models
- Cleanup some comments/error messages
- Formatting
- Major refactor into a matrix bot framework
- Refactoring out the list command for reuse

## [0.1.0] - 2024-03-23

### Bug Fixes

- Formatting

### Documentation

- Add basic README

### Features

- Initialization from examples
- Add ollama backend
- Use a config file for login info
- Add context to the ollama requests
- Add model configuration to config file
- Use aichat as the chat backend

### Miscellaneous Tasks

- Release v0.1.0

### Eat

- Add in session persistence from the examples

<!-- generated by git-cliff -->
