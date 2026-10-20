#!/usr/bin/env bash
set -euo pipefail

# One graph shares dependency work and lets independent checks overlap.
checks=(
  '.#checks.x86_64-linux.lint'
  '.#checks.x86_64-linux.treefmt'
  '.#checks.x86_64-linux.ci-policy'
  '.#checks.x86_64-linux.test'
  '.#checks.x86_64-linux.build'
)
# PRs and merge groups retain normal Cargo tests, including doctests.
if [[ "$GITHUB_EVENT_NAME" == push || "$GITHUB_EVENT_NAME" == workflow_dispatch ]]; then
  checks+=('.#checks.x86_64-linux.doc')
fi
nix build --fallback --no-link --keep-going --max-jobs 2 --cores 0 --json "${checks[@]}" > "$RUNNER_TEMP/chaz-checks.json"
