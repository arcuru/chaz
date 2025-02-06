# default recipe to display help information
default:
    @just --list

# Run CI locally in containers
ci-full:
    act

# Run CI locally
ci: audit fmt test nix-check nix-build clippy pre-commit build
    @echo "Running CI checks"

# Run Nix CI checks 
nix-check:
    nix flake check

# Run Nix Build
nix-build:
    nix build

# Run clippy
clippy:
    cargo clippy

# Run clippy fixes
clippy-fix:
    cargo clippy --fix --allow-dirty

# Run pre-commit
pre-commit:
    pre-commit run --all-files --show-diff-on-failure

# Run all formatters
fmt:
    cargo fmt --all
    alejandra .

# Run all tests
alias t := test
test:
    cargo test

# Run cargo security audit
audit:
    cargo audit

# Build the project
alias b := build
build:
    cargo build