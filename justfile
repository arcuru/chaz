# Chaz Development Commands
# Run `just` to see available recipes

alias b := build
alias t := test

[private]
default:
    @just --list

# =============================================================================
# Development Workflows
# =============================================================================

# Quick development feedback (build + test + lint)
dev:
    just build
    just test
    just lint clippy

# Run automatic fixes (clippy fix + nix fixes + format)
fix:
    cargo clippy --fix --allow-dirty --all-targets --allow-no-vcs -- -D warnings
    statix fix .
    deadnix --edit .
    just fmt

# =============================================================================
# Building
# =============================================================================

# Build the project (debug or release)
build mode='debug':
    cargo build --all-targets {{ if mode == "release" { "--release" } else { "" } }} --quiet

# =============================================================================
# Testing
# =============================================================================

# Run tests: [filter] or bare
test *args='':
    cargo test {{ args }}

# =============================================================================
# Coverage
# =============================================================================

# Generate code coverage report (lcov + html into ./coverage/)
coverage:
    cargo tarpaulin --workspace --output-dir coverage --out lcov --out html --engine llvm --skip-clean \
        --exclude-files 'target/*' --exclude-files 'target-test/*'

# =============================================================================
# Linting (Static Analysis)
# =============================================================================

# Run linter(s): clippy, audit, statix, deadnix, all
lint +tools='clippy audit statix deadnix':
    #!/usr/bin/env bash
    set -e
    for tool in {{ tools }}; do
        case "$tool" in
            clippy)
                echo "=== Running clippy ==="
                cargo clippy --all-targets -- -D warnings
                ;;
            audit)
                echo "=== Running audit (cargo-deny) ==="
                cargo deny check --config .config/deny.toml
                ;;
            statix)
                echo "=== Running statix ==="
                statix check .
                ;;
            deadnix)
                echo "=== Running deadnix ==="
                deadnix --fail .
                ;;
            all)
                just lint clippy audit statix deadnix
                ;;
            *)
                echo "Unknown linter: $tool"
                echo "Options: clippy, audit, statix, deadnix, all"
                exit 1
                ;;
        esac
    done

# =============================================================================
# Formatting
# =============================================================================

# Run formatters: (default), check
fmt mode='':
    #!/usr/bin/env bash
    set -e
    case "{{ mode }}" in
        check)
            cargo fmt --all -- --check
            alejandra . --check --quiet
            prettier --check . --log-level warn
            ;;
        *)
            cargo fmt --all
            alejandra . --quiet
            prettier --write . --log-level warn
            ;;
    esac

# =============================================================================
# Documentation
# =============================================================================

# Documentation commands: build, serve, test
doc action='build':
    #!/usr/bin/env bash
    set -e
    case "{{ action }}" in
        build)
            mdbook build docs
            ;;
        serve)
            mdbook serve docs --open
            ;;
        test)
            mdbook test docs
            ;;
        *)
            echo "Unknown action: {{ action }}"
            echo "Options: build, serve, test"
            exit 1
            ;;
    esac

# =============================================================================
# CI
# =============================================================================

# Run CI locally: local (default), nix
ci mode='local':
    #!/usr/bin/env bash
    set -e
    case "{{ mode }}" in
        local)
            just fix
            just lint
            just build
            just test
            ;;
        nix)
            just nix full
            ;;
        *)
            echo "Unknown mode: {{ mode }}"
            echo "Options: local, nix"
            exit 1
            ;;
    esac

# =============================================================================
# Nix
# =============================================================================

# Nix commands: build, check, full
nix action='check':
    #!/usr/bin/env bash
    set -e
    case "{{ action }}" in
        build)
            nix build
            ;;
        check)
            nix-fast-build --no-link --skip-cached ${CI:+--no-nom}
            ;;
        full)
            just nix check
            nix build --no-link
            ;;
        *)
            echo "Unknown action: {{ action }}"
            echo "Options: build, check, full"
            exit 1
            ;;
    esac
