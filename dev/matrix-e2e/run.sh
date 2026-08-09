#!/usr/bin/env bash
#
# End-to-end test for the Matrix bridge, with nothing outside this machine
# involved.
#
# The test stands up a throwaway Synapse, registers two accounts on it, brings
# up a chaz daemon and a chaz-matrix bridge against a stub LLM, then sends a
# message from one account and asserts the stub's reply comes back out of the
# room. Everything it touches lives in one temporary directory and is deleted
# on the way out.
#
# What it actually proves: a message crosses Matrix into a session database,
# syncs to the agent peer, gets answered, and syncs back out to the room. That
# is the path that keeps breaking, and the one that used to need a human with a
# Matrix client to test.
#
# Usage:
#   dev/matrix-e2e/run.sh [--keep] [--timeout SECONDS] [--verbose]
#
#   --keep      leave the workspace in place and print its path
#   --timeout   seconds to wait for the reply (default 120)
#   --verbose   stream component logs to stderr as they are written
#
# Requires `synapse_homeserver`, `register_new_matrix_user`, `curl`, `jq`, and
# `python3` on PATH — `just e2e` supplies them. Binaries default to
# `target/debug`; override with CHAZ_BIN / CHAZ_MATRIX_BIN.

set -euo pipefail

KEEP=0
VERBOSE=0
REPLY_TIMEOUT=120
while [[ $# -gt 0 ]]; do
	case "$1" in
	--keep) KEEP=1 ;;
	--verbose) VERBOSE=1 ;;
	--timeout)
		REPLY_TIMEOUT="$2"
		shift
		;;
	*)
		echo "unknown argument: $1" >&2
		exit 2
		;;
	esac
	shift
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HERE="$REPO_ROOT/dev/matrix-e2e"
CHAZ_BIN="${CHAZ_BIN:-$REPO_ROOT/target/debug/chaz}"
CHAZ_MATRIX_BIN="${CHAZ_MATRIX_BIN:-$REPO_ROOT/target/debug/chaz-matrix}"

for bin in "$CHAZ_BIN" "$CHAZ_MATRIX_BIN"; do
	if [[ ! -x $bin ]]; then
		echo "missing binary: $bin (run 'just build', or set CHAZ_BIN / CHAZ_MATRIX_BIN)" >&2
		exit 2
	fi
done
for tool in synapse_homeserver register_new_matrix_user curl jq python3; do
	command -v "$tool" >/dev/null || {
		echo "missing tool: $tool (try 'just e2e', which provides them)" >&2
		exit 2
	}
done

WORKSPACE="$(mktemp -d -t chaz-matrix-e2e-XXXXXX)"
PIDS=()

log() { printf '\033[1;35m==>\033[0m %s\n' "$*" >&2; }
fail() {
	printf '\033[1;31mFAIL:\033[0m %s\n' "$*" >&2
	exit 1
}

cleanup() {
	local status=$?
	# Kill children before removing the tree they are writing into, or Synapse
	# spends its shutdown logging failures against deleted paths.
	for pid in "${PIDS[@]:-}"; do
		kill -TERM "$pid" 2>/dev/null || true
	done
	for pid in "${PIDS[@]:-}"; do
		for _ in $(seq 1 50); do
			kill -0 "$pid" 2>/dev/null || break
			sleep 0.1
		done
		kill -KILL "$pid" 2>/dev/null || true
	done
	if [[ $KEEP -eq 1 ]]; then
		log "workspace kept at $WORKSPACE"
	else
		rm -rf "$WORKSPACE"
	fi
	exit $status
}
trap cleanup EXIT INT TERM

# Ask the kernel for a port nobody is using. Racy in principle; the alternative
# is hardcoded ports that collide with a developer's running daemon, which is
# the failure that actually happens.
free_port() {
	python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}

# Poll until a command succeeds. Every wait in this script is bounded: a
# harness that hangs is worse than one that fails, because CI will sit on it
# until the job timeout.
wait_for() {
	local what="$1" timeout="$2"
	shift 2
	for _ in $(seq 1 "$((timeout * 2))"); do
		if "$@" >/dev/null 2>&1; then return 0; fi
		sleep 0.5
	done
	fail "timed out after ${timeout}s waiting for $what"
}

# Start a process, log it into the workspace, and register it for cleanup. The
# pid lands in SPAWNED_PID rather than on stdout: a command substitution would
# run this in a subshell, where the PIDS append is discarded and the process
# survives the run.
SPAWNED_PID=""
spawn() {
	local name="$1"
	shift
	"$@" >"$WORKSPACE/$name.log" 2>&1 &
	SPAWNED_PID=$!
	PIDS+=("$SPAWNED_PID")
	if [[ $VERBOSE -eq 1 ]]; then
		tail -f "$WORKSPACE/$name.log" | sed "s/^/[$name] /" >&2 &
		PIDS+=($!)
	fi
}

SERVER_NAME="e2e.test"
SYNAPSE_PORT="$(free_port)"
STUB_PORT="$(free_port)"
DAEMON_SYNC_PORT="$(free_port)"
BRIDGE_SYNC_PORT="$(free_port)"
HOMESERVER="http://127.0.0.1:$SYNAPSE_PORT"
MARKER="e2e-reply-$$-$(date +%s)"
PROMPT="ping from the puppet"

# Credentials are generated per run and never leave this directory. There is no
# account here worth protecting, but a fixed password in a repo is a habit
# worth not forming.
AGENT_PASSWORD="$(head -c 18 /dev/urandom | base64)"
PUPPET_PASSWORD="$(head -c 18 /dev/urandom | base64)"

export XDG_CONFIG_HOME="$WORKSPACE/xdg-config"
mkdir -p "$XDG_CONFIG_HOME"

log "workspace: $WORKSPACE"
log "synapse :$SYNAPSE_PORT  stub-llm :$STUB_PORT  daemon-sync :$DAEMON_SYNC_PORT  bridge-sync :$BRIDGE_SYNC_PORT"

# ---------------------------------------------------------------- stub LLM ---
log "starting stub LLM"
spawn stub-llm python3 "$HERE/stub_llm.py" "$STUB_PORT" "$MARKER"
wait_for "stub LLM" 30 curl -sf "http://127.0.0.1:$STUB_PORT/v1/models"

# ----------------------------------------------------------------- synapse ---
log "generating synapse config"
SYNAPSE_DIR="$WORKSPACE/synapse"
mkdir -p "$SYNAPSE_DIR"
synapse_homeserver \
	--server-name "$SERVER_NAME" \
	--config-path "$SYNAPSE_DIR/homeserver.yaml" \
	--generate-config \
	--report-stats=no \
	--data-directory "$SYNAPSE_DIR" >"$WORKSPACE/synapse-config.log" 2>&1

python3 - "$SYNAPSE_DIR/homeserver.yaml" "$SYNAPSE_PORT" "$SYNAPSE_DIR" <<'PY'
import os
import sys

import yaml

path, port, synapse_dir = sys.argv[1], int(sys.argv[2]), sys.argv[3]
with open(path) as fh:
    cfg = yaml.safe_load(fh)

# The generated log config names its file relative to the working directory, so
# Synapse drops a homeserver.log wherever the harness was invoked from — which
# for `just e2e` is the repo root. Pin it inside the workspace so a test run
# leaves nothing behind to be committed by accident.
log_config_path = cfg.get("log_config")
if log_config_path and os.path.exists(log_config_path):
    with open(log_config_path) as fh:
        log_cfg = yaml.safe_load(fh)
    for handler in (log_cfg.get("handlers") or {}).values():
        if isinstance(handler, dict) and "filename" in handler:
            handler["filename"] = os.path.join(
                synapse_dir, os.path.basename(handler["filename"])
            )
    with open(log_config_path, "w") as fh:
        yaml.safe_dump(log_cfg, fh)

# Registration is open because the harness mints its own accounts; this server
# is reachable only on loopback and is deleted when the run ends.
cfg["enable_registration"] = True
cfg["enable_registration_without_verification"] = True
cfg["registration_shared_secret"] = "e2e-registration-secret"

# No federation: nothing outside this machine should be contactable, and a test
# that can reach the public internet is a test that can fail because of it.
cfg["federation_domain_whitelist"] = []
cfg["send_federation"] = False

cfg["listeners"] = [
    {
        "port": port,
        "bind_addresses": ["127.0.0.1"],
        "type": "http",
        "tls": False,
        "x_forwarded": False,
        "resources": [{"names": ["client"], "compress": False}],
    }
]

# Rate limits exist to protect a public server. Here they only make a burst of
# test traffic flaky.
for key in (
    "rc_message",
    "rc_registration",
    "rc_login",
    "rc_joins",
    "rc_invites",
):
    cfg.pop(key, None)
cfg["rc_message"] = {"per_second": 1000, "burst_count": 1000}
cfg["rc_login"] = {
    "address": {"per_second": 1000, "burst_count": 1000},
    "account": {"per_second": 1000, "burst_count": 1000},
    "failed_attempts": {"per_second": 1000, "burst_count": 1000},
}

with open(path, "w") as fh:
    yaml.safe_dump(cfg, fh)
PY

log "starting synapse"
spawn synapse synapse_homeserver --config-path "$SYNAPSE_DIR/homeserver.yaml"
wait_for "synapse" 120 curl -sf "$HOMESERVER/_matrix/client/versions"

log "registering accounts"
for pair in "agent:$AGENT_PASSWORD" "puppet:$PUPPET_PASSWORD"; do
	user="${pair%%:*}"
	pass="${pair#*:}"
	register_new_matrix_user \
		-c "$SYNAPSE_DIR/homeserver.yaml" \
		-u "$user" -p "$pass" --no-admin \
		"$HOMESERVER" >>"$WORKSPACE/register.log" 2>&1 ||
		fail "could not register @$user:$SERVER_NAME (see $WORKSPACE/register.log)"
done

AGENT_MXID="@agent:$SERVER_NAME"
PUPPET_MXID="@puppet:$SERVER_NAME"

# ------------------------------------------------------------ chaz configs ---
DAEMON_CONFIG="$WORKSPACE/daemon.yaml"
BRIDGE_CONFIG="$WORKSPACE/bridge.yaml"

cat >"$DAEMON_CONFIG" <<EOF
state_dir: "$WORKSPACE/state-daemon"
sync_listen: "127.0.0.1:$DAEMON_SYNC_PORT"

backends:
  - name: stub
    type: openaicompatible
    api_base: "http://127.0.0.1:$STUB_PORT/v1"
    api_key: "not-a-real-key"
    models:
      - name: stub

agents:
  - name: chaz
    model: "stub"
    system_prompt: |
      You are a test fixture.

default_agents: [chaz]
EOF

# ------------------------------------------------------------- bring-up ------
# The sequence that removes the human: ask the bridge which key to trust,
# pre-authorize it, and hand over a ticket. Pre-authorized access bootstraps
# without the /sharing approve round-trip.
log "reading bridge identity"
cat >"$BRIDGE_CONFIG" <<EOF
unlock_password: e2e-bridge-unlock
label: e2e-matrix
state_dir: "$WORKSPACE/state-bridge"
sync_listen: "127.0.0.1:$BRIDGE_SYNC_PORT"
logins: []
agents:
  - name: chaz
EOF

BRIDGE_KEY="$("$CHAZ_MATRIX_BIN" --config "$BRIDGE_CONFIG" --print-pubkey 2>>"$WORKSPACE/bringup.log")"
[[ -n $BRIDGE_KEY ]] || fail "bridge did not report a public key (see $WORKSPACE/bringup.log)"
log "bridge key: $BRIDGE_KEY"

log "pre-authorizing the bridge on the daemon"
"$CHAZ_BIN" --config "$DAEMON_CONFIG" cmd "/agent invite chaz $BRIDGE_KEY write" \
	>>"$WORKSPACE/bringup.log" 2>&1 || fail "/agent invite failed (see $WORKSPACE/bringup.log)"

log "minting the access ticket"
TICKET="$("$CHAZ_BIN" --config "$DAEMON_CONFIG" cmd '/agent share chaz' 2>>"$WORKSPACE/bringup.log" |
	grep -o 'eidetica:[^[:space:]]*' | head -1)"
[[ -n $TICKET ]] || fail "/agent share produced no ticket (see $WORKSPACE/bringup.log)"

# Drop iroh hints: the daemon mints a fresh endpoint every start, so a recorded
# one is already stale, and sync pays a full timeout per dead address. Both
# processes are on loopback, where the http hint is what connects.
#
# Cut each hint at the next separator rather than with a shell glob. Globs are
# greedy, so `&pr=iroh:*` deletes everything after the first iroh hint —
# including the http hint, whenever the daemon happens to order iroh first.
TICKET="$(printf '%s' "$TICKET" | sed 's/&pr=iroh:[^&]*//g')"
case "$TICKET" in
*"pr=http:"*) ;;
*) fail "ticket carries no loopback address hint: $TICKET" ;;
esac

cat >"$BRIDGE_CONFIG" <<EOF
unlock_password: e2e-bridge-unlock
label: e2e-matrix
state_dir: "$WORKSPACE/state-bridge"
sync_listen: "127.0.0.1:$BRIDGE_SYNC_PORT"

logins:
  - agent: chaz
    ticket: "$TICKET"
    type: matrix
    homeserver_url: "$HOMESERVER"
    username: "$AGENT_MXID"
    password: "$AGENT_PASSWORD"
    allow_list: "$PUPPET_MXID"

agents:
  - name: chaz
EOF

# ------------------------------------------------------------- processes -----
log "starting daemon"
spawn daemon "$CHAZ_BIN" --config "$DAEMON_CONFIG" daemon
DAEMON_PID="$SPAWNED_PID"
wait_for "daemon" 90 grep -q "daemon ready" "$WORKSPACE/daemon.log"

log "starting bridge"
spawn bridge "$CHAZ_MATRIX_BIN" --config "$BRIDGE_CONFIG"
BRIDGE_PID="$SPAWNED_PID"
wait_for "bridge matrix login" 120 grep -q "Matrix login spawned" "$WORKSPACE/bridge.log"

if grep -q "Pending" "$WORKSPACE/bridge.log"; then
	fail "bridge is waiting on manual approval — pre-authorization did not take (see $WORKSPACE/bridge.log)"
fi

# --------------------------------------------------------------- puppet ------
mx() {
	local method="$1" path="$2" token="${3:-}" body="${4:-}"
	local args=(-sS -X "$method" -H 'Content-Type: application/json')
	[[ -n $token ]] && args+=(-H "Authorization: Bearer $token")
	[[ -n $body ]] && args+=(-d "$body")
	curl "${args[@]}" "$HOMESERVER$path"
}

log "puppet logging in"
PUPPET_TOKEN="$(mx POST /_matrix/client/v3/login "" "$(jq -nc \
	--arg u puppet --arg p "$PUPPET_PASSWORD" \
	'{type:"m.login.password",identifier:{type:"m.id.user",user:$u},password:$p}')" |
	jq -r '.access_token // empty')"
[[ -n $PUPPET_TOKEN ]] || fail "puppet could not log in"

log "creating the room"
# Unencrypted on purpose, and not a limitation of the harness: chaz builds
# matrix-sdk without the e2e-encryption feature, so an encrypted room is one
# the bridge could not read at all.
ROOM_ID="$(mx POST /_matrix/client/v3/createRoom "$PUPPET_TOKEN" "$(jq -nc \
	--arg agent "$AGENT_MXID" \
	'{preset:"trusted_private_chat",is_direct:true,invite:[$agent]}')" |
	jq -r '.room_id // empty')"
[[ -n $ROOM_ID ]] || fail "could not create a room"
log "room: $ROOM_ID"

log "waiting for the bridge to accept the invite"
agent_joined() {
	mx GET "/_matrix/client/v3/rooms/$(jq -rn --arg r "$ROOM_ID" '$r|@uri')/joined_members" \
		"$PUPPET_TOKEN" | jq -e --arg a "$AGENT_MXID" '.joined | has($a)'
}
wait_for "the agent to join the room" 90 agent_joined

log "sending the prompt"
TXN="e2e-$(date +%s%N)"
mx PUT "/_matrix/client/v3/rooms/$(jq -rn --arg r "$ROOM_ID" '$r|@uri')/send/m.room.message/$TXN" \
	"$PUPPET_TOKEN" "$(jq -nc --arg b "$PROMPT" '{msgtype:"m.text",body:$b}')" >/dev/null

log "waiting up to ${REPLY_TIMEOUT}s for the reply"
reply_arrived() {
	mx GET "/_matrix/client/v3/rooms/$(jq -rn --arg r "$ROOM_ID" '$r|@uri')/messages?dir=b&limit=50" \
		"$PUPPET_TOKEN" |
		jq -e --arg a "$AGENT_MXID" --arg m "$MARKER" \
			'[.chunk[] | select(.sender == $a) | select(.content.body // "" | contains($m))] | length > 0'
}
wait_for "the agent's reply in the room" "$REPLY_TIMEOUT" reply_arrived

printf '\033[1;32mPASS\033[0m — the reply crossed Matrix, sync, the agent, and came back\n' >&2
