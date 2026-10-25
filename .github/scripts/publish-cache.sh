#!/usr/bin/env bash
set -euo pipefail

# Defense in depth behind the protected job's exact main/event guard.
[[ "$GITHUB_REPOSITORY" == arcuru/chaz && "$GITHUB_REPOSITORY_OWNER" == arcuru &&
  "$GITHUB_REF" == refs/heads/main &&
  ("$GITHUB_EVENT_NAME" == push || "$GITHUB_EVENT_NAME" == workflow_dispatch) ]] || {
  echo 'Cache publication refused for this repository/event/ref' >&2
  exit 1
}

# Native path listings come only from this job's successful checks/builds.
[[ -s "$RUNNER_TEMP/chaz-checks-paths" && -s "$RUNNER_TEMP/chaz-final-paths" ]] || exit 1
sort -u "$RUNNER_TEMP/chaz-checks-paths" "$RUNNER_TEMP/chaz-final-paths" > "$RUNNER_TEMP/chaz-publish-paths"
mapfile -t paths < "$RUNNER_TEMP/chaz-publish-paths"
((${#paths[@]} > 0)) || exit 1
for path in "${paths[@]}"; do
  [[ "$path" =~ ^/nix/store/[a-z0-9]{32}-[a-zA-Z0-9+._?=-]+$ ]] || {
    echo 'Invalid publication store path' >&2
    exit 1
  }
done
# Explicit store paths cannot trigger evaluation or a second workspace build.
nix path-info "${paths[@]}" >/dev/null
umask 077
key=$(mktemp "$RUNNER_TEMP/chaz-cache-key.XXXXXX")
trap 'rm -f "$key"' EXIT
printf '%s\n' "${NIX_CACHE_SIGNING_KEY:?missing signing key}" > "$key"
unset NIX_CACHE_SIGNING_KEY
nix store sign --recursive --key-file "$key" "${paths[@]}"
# Cargo artifacts are already compressed; avoid expensive outer XZ.
cache='s3://cache-eidetica-dev?region=auto&endpoint=https://ab01ccd7fce6ab984e4c956734085680.r2.cloudflarestorage.com&compression=zstd'
nix copy --to "$cache" "${paths[@]}"
