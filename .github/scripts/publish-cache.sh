#!/usr/bin/env bash
set -euo pipefail

# Defense in depth behind the job's exact event/ref and protected environment.
[[ "$GITHUB_REPOSITORY" == arcuru/chaz && "$GITHUB_REPOSITORY_OWNER" == arcuru &&
  "$GITHUB_REF" == refs/heads/main &&
  ("$GITHUB_EVENT_NAME" == push || "$GITHUB_EVENT_NAME" == workflow_dispatch) ]] || {
  echo 'Cache publication refused for this repository/event/ref' >&2
  exit 1
}

case "${1:?expected deps or final}" in
  deps) jq -e '[.[] | .outputs[]]' "$RUNNER_TEMP/chaz-deps.json" > "$RUNNER_TEMP/chaz-publish.json" ;;
  final) jq -es '[.[][] | .outputs[]] | unique' "$RUNNER_TEMP/chaz-checks.json" "$RUNNER_TEMP/chaz-final.json" > "$RUNNER_TEMP/chaz-publish.json" ;;
  *) exit 2 ;;
esac
jq -e 'length > 0 and all(.[]; test("^/nix/store/[a-z0-9]{32}-[a-zA-Z0-9+._?=-]+$"))' "$RUNNER_TEMP/chaz-publish.json" >/dev/null
mapfile -t paths < <(jq -r '.[]' "$RUNNER_TEMP/chaz-publish.json")
# Publication only copies realized outputs. Never evaluate/build with keys present.
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
