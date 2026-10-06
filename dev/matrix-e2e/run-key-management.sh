#!/usr/bin/env bash
# Disposable protocol acceptance for the actual maintenance CLI, no models.
set -euo pipefail
umask 077
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
WORK="${KEY_TEST_WORKSPACE:-$(mktemp -d -t chaz-key-management-XXXXXX)}"
mkdir -p "$WORK"
PORT="$(uv run --no-project python -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')"
export KEY_TEST_HOMESERVER="http://127.0.0.1:$PORT" KEY_TEST_WORKSPACE="$WORK"
export CHAZ_MATRIX_BIN="${CHAZ_MATRIX_BIN:-$ROOT/target/debug/chaz-matrix}"
export KEY_PROBE_BIN="${KEY_PROBE_BIN:-$ROOT/target/debug/examples/key_management_probe}"
cd "$WORK"
synapse_homeserver --server-name keys.test --config-path homeserver.yaml --generate-config --report-stats no > generate.log 2>&1
uv run --no-project python - "$PORT" <<'PY' > local.yaml
import json, sys
print(json.dumps({"enable_registration": True, "enable_registration_without_verification": True,
    "federation_domain_whitelist": [], "send_federation": False,
    "listeners": [{"port": int(sys.argv[1]), "bind_addresses": ["127.0.0.1"], "type": "http", "tls": False,
    "resources": [{"names": ["client"], "compress": False}]}],
    "rc_message": {"per_second":1000, "burst_count":1000}, "rc_registration": {"per_second":1000, "burst_count":1000},
    "rc_login": {k:{"per_second":1000, "burst_count":1000} for k in ["address","account","failed_attempts"]}}))
PY
synapse_homeserver --config-path homeserver.yaml --config-path local.yaml > synapse.log 2>&1 &
PID=$!
cleanup() { kill -TERM "$PID" 2>/dev/null || true; wait "$PID" 2>/dev/null || true; }
trap cleanup EXIT INT TERM
for _ in $(seq 1 120); do
    if curl -sf --max-time 2 "$KEY_TEST_HOMESERVER/_matrix/client/versions" >/dev/null; then break; fi
    sleep 0.5
done
uv run --no-project python "$ROOT/dev/matrix-e2e/key-management.py"
