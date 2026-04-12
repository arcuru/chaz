# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Chaz is an AI agent orchestrator for Matrix written in Rust. It connects to Matrix rooms and responds using LLM backends (OpenAI-compatible APIs and AIChat subprocess). Single binary, no database — per-room config is stored in Matrix room state events (tags).

**Status**: Active development. Being expanded from a chatbot into a full agent orchestrator with tool use, memory, and autonomous capabilities.

## Build & Development Commands

Development environment uses Nix flakes with treefmt (enter via `direnv allow` or `nix develop`). The `justfile` is the primary task runner:

```bash
just build          # cargo build
just test           # cargo test (alias: just t)
just lint           # clippy + other lints
just fmt            # treefmt (rustfmt, alejandra, prettier)
just ci             # all checks: lint fmt test nix build
just nix build      # nix build
just nix check      # nix flake check
```

There are no unit tests yet — `cargo test` will pass but has no meaningful test coverage. Build dependencies require `pkg-config`, `openssl`, and `sqlite`.

## Architecture

Single Rust binary with six source modules:

- **`main.rs`** — Bot entry point, Matrix event handlers, all `!chaz` command implementations, context/history gathering, rate limiting
- **`backends.rs`** — `BackendManager` dispatcher and `LLMBackend` trait definition. `ChatContext` struct represents a generic chat request
- **`openai.rs`** — `LLMBackend` impl for OpenAI-compatible APIs via `openai-api-rs`
- **`aichat.rs`** — `LLMBackend` impl that spawns `aichat` as a subprocess
- **`role.rs`** — System prompt/role management. Roles define name, description, prompt, and example messages. Hierarchy: room-defined → config → built-in
- **`defaults.rs`** — Built-in default config and pre-defined roles (chaz, chazmina, cave-chaz, bash, fish, zsh, nu, etc.)

Key design patterns:

- Global state via `lazy_static` (`GLOBAL_CONFIG`, `GLOBAL_MESSAGES` for rate limiting)
- Matrix room tags for per-room model/role/backend persistence (namespace `is.chaz.*`)
- Bot framework: `headjack` crate wraps `matrix-sdk`
- Config: YAML (`config.yaml`) parsed with `serde_yaml`

## CI

GitHub Actions runs on push to main and PRs:

- `ci.yml`: nix-fast-build based — lint, test, build
- `security-audit.yml`: daily cargo-deny, auto-creates GitHub issues
- `release-plz.yml`: automated versioning and crates.io publishing
- Dependency update workflows: cargo-update, flake-update, actions-update (monthly with hold gates)

## Formatting & Linting

treefmt enforces: `rustfmt` (Rust), `alejandra` (Nix), `prettier` (Markdown/YAML). Clippy denies all warnings in CI.
