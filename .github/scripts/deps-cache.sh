#!/usr/bin/env bash
set -euo pipefail

# A selected dependency closure, never the runner store or its database.
state="$RUNNER_TEMP/chaz-deps"
payload="$RUNNER_TEMP/chaz-deps-cache"
mkdir -p "$state"
pr_cache_role() {
  [[ "$GITHUB_EVENT_NAME" == pull_request && "$GITHUB_REF" =~ ^refs/pull/[0-9]+/merge$ ]]
}

case "${1:?expected resolve, trusted, build, export or import}" in
  resolve)
    system=$(nix eval --raw --impure --expr builtins.currentSystem)
    [[ "$system" == x86_64-linux ]]
    root=$(nix eval --raw ".#legacyPackages.$system.test.deps.outPath")
    drv=$(nix eval --raw ".#legacyPackages.$system.test.deps.drvPath")
    [[ "$root" =~ ^/nix/store/[a-z0-9]{32}-[a-zA-Z0-9+._?=-]+$ ]]
    [[ "$drv" =~ ^/nix/store/[a-z0-9]{32}-[a-zA-Z0-9+._?=-]+\.drv$ ]]
    jq -cn --arg system "$system" --arg root "$root" --arg drv "$drv" \
      '{schema:1, system:$system, root:$root, drv:$drv}' > "$state/identity.json"
    key="chaz-deps-v1-$system-$(sha256sum "$state/identity.json" | cut -d' ' -f1)"
    echo "key=$key" >> "$GITHUB_OUTPUT"
    ;;
  trusted)
    root=$(jq -er .root "$state/identity.json")
    if nix path-info "$root" >/dev/null 2>&1; then
      echo 'available=true' >> "$GITHUB_OUTPUT"
    elif nix path-info --store https://cache.eidetica.dev "$root" >/dev/null; then
      # A known signed-cache entry must import successfully. Do not turn a bad
      # signature or a broken closure into an unsigned-cache fallback.
      nix copy --from https://cache.eidetica.dev "$root"
      echo 'available=true' >> "$GITHUB_OUTPUT"
    else
      echo '::notice::Trusted dependency cache unavailable; resolve dependencies normally.'
      echo 'available=false' >> "$GITHUB_OUTPUT"
    fi
    ;;
  build)
    root=$(jq -er .root "$state/identity.json")
    nix build --fallback --no-link --max-jobs 2 --cores 0 '.#test.deps'
    nix path-info "$root" >/dev/null
    ;;
  export)
    pr_cache_role || { echo 'PR cache export refused for this event/ref' >&2; exit 1; }
    root=$(jq -er .root "$state/identity.json")
    rm -rf "$payload"
    mkdir -p "$payload"
    nix copy --to "file://$payload" "$root"
    cp "$state/identity.json" "$payload/identity.json"
    ;;
  import)
    pr_cache_role || { echo 'Unsigned cache import refused for this event/ref' >&2; exit 1; }
    # Compare before reading any store paths from the restored payload. Copy only
    # the expected root and its closure; unrelated cache entries are never used.
    if ! cmp -s "$state/identity.json" "$payload/identity.json"; then
      echo '::warning::Dependency cache identity mismatch; rebuilding.'
      exit 0
    fi
    root=$(jq -er .root "$state/identity.json")
    # The only unsigned operation: unprivileged PR store, exact-key local cache.
    if ! nix copy --no-check-sigs --from "file://$payload" "$root"; then
      echo '::warning::Dependency cache import failed; rebuilding.'
    fi
    ;;
  *) exit 2 ;;
esac
