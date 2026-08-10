#!/usr/bin/env bash
#
# End-to-end test for the Matrix bridge.
#
# In the default `--transport http` mode nothing outside this machine is
# involved. `--transport iroh` is the exception: iroh discovery reaches n0's
# public relay and DHT infrastructure, so that mode needs the internet and can
# fail for reasons that have nothing to do with chaz. CI runs http only.
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
#                         [--transport http|iroh]
#
#   --keep      leave the workspace in place and print its path
#   --timeout   seconds to wait for the reply (default 120)
#   --verbose   stream component logs to stderr as they are written
#   --transport eidetica sync transport between the two peers (default http).
#               `iroh` reaches public relay infrastructure — see above.
#
# Requires `synapse_homeserver`, `register_new_matrix_user`, `curl`, `jq`, and
# `python3` on PATH — `just e2e` supplies them. Binaries default to
# `target/debug`; override with CHAZ_BIN / CHAZ_MATRIX_BIN.

set -euo pipefail

KEEP=0
VERBOSE=0
REPLY_TIMEOUT=120
TRANSPORT="http"
while [[ $# -gt 0 ]]; do
	case "$1" in
	--) ;; # `just` passes -- to separate its flags from ours; fall through to outer shift
	--keep) KEEP=1 ;;
	--verbose) VERBOSE=1 ;;
	--timeout)
		REPLY_TIMEOUT="$2"
		shift
		;;
	--transport)
		TRANSPORT="${2:-http}"
		if [[ $TRANSPORT != "http" && $TRANSPORT != "iroh" ]]; then
			echo "invalid transport: $TRANSPORT (must be http or iroh)" >&2
			exit 2
		fi
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

# Drop a pid that has already exited from the cleanup list. A restart case
# leaves the old pid behind otherwise, and cleanup would send KILL to a number
# the kernel is free to have handed to something else by then.
retire_pid() {
	local target="$1" i
	for i in "${!PIDS[@]}"; do
		if [[ ${PIDS[$i]} == "$target" ]]; then
			unset 'PIDS[i]'
		fi
	done
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
STRANGER_PASSWORD="$(head -c 18 /dev/urandom | base64)"

export XDG_CONFIG_HOME="$WORKSPACE/xdg-config"
mkdir -p "$XDG_CONFIG_HOME"

log "workspace: $WORKSPACE"
log "synapse :$SYNAPSE_PORT  stub-llm :$STUB_PORT  daemon-sync :$DAEMON_SYNC_PORT  bridge-sync :$BRIDGE_SYNC_PORT"
log "transport: $TRANSPORT"

if [[ $TRANSPORT == "http" ]]; then
	SYNC_LISTEN_DAEMON="sync_listen: \"127.0.0.1:$DAEMON_SYNC_PORT\""
	SYNC_LISTEN_BRIDGE="sync_listen: \"127.0.0.1:$BRIDGE_SYNC_PORT\""
else
	SYNC_LISTEN_DAEMON=""
	SYNC_LISTEN_BRIDGE=""
fi

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
for pair in "agent:$AGENT_PASSWORD" "puppet:$PUPPET_PASSWORD" "stranger:$STRANGER_PASSWORD"; do
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
STRANGER_MXID="@stranger:$SERVER_NAME"

# ------------------------------------------------------------ chaz configs ---
DAEMON_CONFIG="$WORKSPACE/daemon.yaml"
BRIDGE_CONFIG="$WORKSPACE/bridge.yaml"

cat >"$DAEMON_CONFIG" <<EOF
state_dir: "$WORKSPACE/state-daemon"
$SYNC_LISTEN_DAEMON

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
$SYNC_LISTEN_BRIDGE
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

if [[ $TRANSPORT == "http" ]]; then
	# Drop iroh hints: the daemon mints a fresh endpoint every start, so a recorded
	# one is already stale, and sync pays a full timeout per dead address. Both
	# processes are on loopback, where the http hint is what connects.
	#
	# Cut each hint at the next separator rather than with a shell glob. Globs are
	# greedy, so `&pr=iroh:*` deletes everything after the first iroh hint —
	# including the http hint, whenever the daemon happens to order iroh first.
	#
	# Three expressions because the hint's separator depends on its position: a
	# hint in first query position is introduced by `?`, and the `?` has to
	# survive if another hint follows it.
	TICKET="$(printf '%s' "$TICKET" |
		sed -e 's/&pr=iroh:[^&]*//g' \
			-e 's/?pr=iroh:[^&]*&/?/' \
			-e 's/?pr=iroh:[^&]*$//')"
	case "$TICKET" in
	*"pr=http:"*) ;;
	*) fail "ticket carries no loopback address hint: $TICKET" ;;
	esac
else
	# Assert the run is actually on the P2P path. Today the daemon only binds
	# an http sync listener when `sync_listen` is set, which this mode leaves
	# empty — but a future default that reintroduces a bind would turn this
	# mode back into the http mode under a different name, silently.
	case "$TICKET" in
	*"pr=iroh:"*) ;;
	*) fail "ticket carries no iroh address hint: $TICKET" ;;
	esac
	case "$TICKET" in
	*"pr=http:"*) fail "ticket carries an http address hint in iroh mode: $TICKET" ;;
	esac
fi

cat >"$BRIDGE_CONFIG" <<EOF
unlock_password: e2e-bridge-unlock
label: e2e-matrix
state_dir: "$WORKSPACE/state-bridge"
$SYNC_LISTEN_BRIDGE

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

# The bridge's own crate runs at debug so its message-routing decisions are
# visible; everything else stays at info. A case about a message that must be
# ignored asserts on the line the bridge writes when it drops one, which is the
# only direct evidence that the drop happened — and the rest of the tree at
# debug would bury it under matrix-sdk's sync traffic.
BRIDGE_RUST_LOG="info,chaz_matrix_bridge=debug"

# Which log file the live bridge is writing to. The restart case respawns it
# under a second name, and the group-room cases run after that.
BRIDGE_LOG="$WORKSPACE/bridge.log"

log "starting bridge"
spawn bridge env RUST_LOG="$BRIDGE_RUST_LOG" "$CHAZ_MATRIX_BIN" --config "$BRIDGE_CONFIG"
BRIDGE_PID="$SPAWNED_PID"
wait_for "bridge matrix login" 120 grep -q "The client is ready" "$WORKSPACE/bridge.log"

if grep -qi "pending owner approval" "$WORKSPACE/bridge.log"; then
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
	mx GET "/_matrix/client/v3/rooms/$(jq -rn --arg r "$ROOM_ID" '$r|@uri')/messages?dir=b&limit=200" \
		"$PUPPET_TOKEN" |
		jq -e --arg a "$AGENT_MXID" --arg m "$MARKER" \
			'[.chunk[] | select(.sender == $a) | select(.content.body // "" | contains($m))] | length > 0'
}

# Counting predicate for multi-turn tests. reply_arrived uses length > 0,
# which is true the moment the first reply exists — every subsequent turn
# would pass instantly without testing anything. Use this instead.
replies_at_least() {
	local n="$1"
	mx GET "/_matrix/client/v3/rooms/$(jq -rn --arg r "$ROOM_ID" '$r|@uri')/messages?dir=b&limit=200" \
		"$PUPPET_TOKEN" |
		jq -e --arg a "$AGENT_MXID" --arg m "$MARKER" --argjson n "$n" \
			'[.chunk[] | select(.sender == $a) | select(.content.body // "" | contains($m))] | length >= $n'
}
wait_for "the agent's reply in the room" "$REPLY_TIMEOUT" reply_arrived

printf '\033[1;32mPASS\033[0m — cold boot: the reply crossed Matrix, sync, the agent, and came back\n' >&2

# --------------------------------------------------------- restart cases ---
# Case A — restart the bridge mid-conversation
log "Case A: sending second message"
TXN="e2e-$(date +%s%N)"
mx PUT "/_matrix/client/v3/rooms/$(jq -rn --arg r "$ROOM_ID" '$r|@uri')/send/m.room.message/$TXN" \
	"$PUPPET_TOKEN" "$(jq -nc --arg b "second turn" '{msgtype:"m.text",body:$b}')" >/dev/null

log "waiting for the second reply"
wait_for "second reply" "$REPLY_TIMEOUT" replies_at_least 2

log "restarting the bridge"
kill -TERM "$BRIDGE_PID"
wait_for "bridge to exit" 30 sh -c "! kill -0 $BRIDGE_PID 2>/dev/null"
retire_pid "$BRIDGE_PID"

spawn bridge-restarted env RUST_LOG="$BRIDGE_RUST_LOG" "$CHAZ_MATRIX_BIN" --config "$BRIDGE_CONFIG"
BRIDGE_PID="$SPAWNED_PID"
BRIDGE_LOG="$WORKSPACE/bridge-restarted.log"
wait_for "bridge to come back online" 120 grep -q "The client is ready" "$BRIDGE_LOG"

if grep -qi "pending owner approval" "$WORKSPACE/bridge-restarted.log"; then
	fail "bridge asked for re-approval after restart — key persistence regression"
fi

log "Case A: sending third message after bridge restart"
TXN="e2e-$(date +%s%N)"
mx PUT "/_matrix/client/v3/rooms/$(jq -rn --arg r "$ROOM_ID" '$r|@uri')/send/m.room.message/$TXN" \
	"$PUPPET_TOKEN" "$(jq -nc --arg b "third turn after bridge restart" '{msgtype:"m.text",body:$b}')" >/dev/null

wait_for "third reply" "$REPLY_TIMEOUT" replies_at_least 3

# Case B — restart the daemon mid-conversation
log "Case B: restarting the daemon"
kill -TERM "$DAEMON_PID"
wait_for "daemon to exit" 30 sh -c "! kill -0 $DAEMON_PID 2>/dev/null"
retire_pid "$DAEMON_PID"

spawn daemon-restarted "$CHAZ_BIN" --config "$DAEMON_CONFIG" daemon
DAEMON_PID="$SPAWNED_PID"
wait_for "daemon to come back online" 120 grep -q "daemon ready" "$WORKSPACE/daemon-restarted.log"

log "Case B: sending fourth message after daemon restart"
TXN="e2e-$(date +%s%N)"
mx PUT "/_matrix/client/v3/rooms/$(jq -rn --arg r "$ROOM_ID" '$r|@uri')/send/m.room.message/$TXN" \
	"$PUPPET_TOKEN" "$(jq -nc --arg b "fourth turn after daemon restart" '{msgtype:"m.text",body:$b}')" >/dev/null

wait_for "fourth reply" "$REPLY_TIMEOUT" replies_at_least 4

printf '\033[1;32mPASS\033[0m — all restart cases passed (bridge + daemon)\n' >&2

# -------------------------------------------------------- group-room cases ---
# Case 1a — a bare message in a group room is ignored
# Case 2  — an @-mention in a group room is answered
# Case 1b — the `!chaz` prefix is answered without a mention
# Case 3  — a sender outside allow_list is ignored even with the `!chaz` prefix
#
# Each "must be ignored" case is asserted with a barrier rather than a fixed
# wait: send the message that must not be answered, then one that must be, and
# wait for the second to be answered before requiring the first produced
# nothing. Both travel the same path in that order, so the second answer is
# proof the first had its chance. A fixed window instead asserts only that the
# bridge is slower than the window.
#
# The barrier's answer has to be distinguishable from the forbidden one, or the
# assertion fires on whichever landed first and passes for the wrong reason. So
# each pair is asserted on a different observable: the stub's request log for
# the model path, and two different command replies for the `!chaz` path.

log "group room: stranger logging in"
STRANGER_TOKEN="$(mx POST /_matrix/client/v3/login "" "$(jq -nc \
	--arg u stranger --arg p "$STRANGER_PASSWORD" \
	'{type:"m.login.password",identifier:{type:"m.id.user",user:$u},password:$p}')" |
	jq -r '.access_token // empty')"
[[ -n $STRANGER_TOKEN ]] || fail "stranger could not log in"

# The stranger joins before the agent is invited. The bridge treats a room as a
# group room on `joined_members_count() >= 3`, which is its own synced view — so
# the third member has to already be there when the bridge first sees the room.
# Inviting everyone at once instead leaves a race the harness cannot observe,
# and losing it makes the @-mention case pass as a direct chat.
GROUP_ROOM_ID="$(mx POST /_matrix/client/v3/createRoom "$PUPPET_TOKEN" "$(jq -nc \
	--arg stranger "$STRANGER_MXID" \
	'{preset:"trusted_private_chat",invite:[$stranger]}')" |
	jq -r '.room_id // empty')"
[[ -n $GROUP_ROOM_ID ]] || fail "could not create group room"
log "group room: $GROUP_ROOM_ID"

GROUP_URI="$(jq -rn --arg r "$GROUP_ROOM_ID" '$r|@uri')"

mx POST "/_matrix/client/v3/rooms/$GROUP_URI/join" "$STRANGER_TOKEN" "" >/dev/null

group_member_joined() {
	mx GET "/_matrix/client/v3/rooms/$GROUP_URI/joined_members" "$PUPPET_TOKEN" |
		jq -e --arg u "$1" '.joined | has($u)'
}
wait_for "the stranger to join the group room" 90 group_member_joined "$STRANGER_MXID"

# The bridge auto-joins when invited by a user in its allow_list.
mx POST "/_matrix/client/v3/rooms/$GROUP_URI/invite" "$PUPPET_TOKEN" \
	"$(jq -nc --arg a "$AGENT_MXID" '{user_id:$a}')" >/dev/null
wait_for "the agent to join the group room" 90 group_member_joined "$AGENT_MXID"

# Count what the agent actually said in the group room. Restricted to
# `m.room.message`: /messages returns state events too, and the agent's own
# join is one of them, so a sender-only filter is already true before the agent
# has said anything at all.
group_agent_said() {
	mx GET "/_matrix/client/v3/rooms/$GROUP_URI/messages?dir=b&limit=200" \
		"$PUPPET_TOKEN" |
		jq --arg a "$AGENT_MXID" --arg s "$1" \
			'[.chunk[]
			  | select(.type == "m.room.message")
			  | select(.sender == $a)
			  | select(.content.body // "" | contains($s))] | length'
}

# `group_agent_said` returns empty if the request fails, and an empty string is
# 0 to `-gt`. Require a number before comparing, so a broken request reads as
# "not yet" rather than as a passing negative assertion.
group_said_count_is() {
	local got
	got="$(group_agent_said "$1")"
	[[ $got =~ ^[0-9]+$ ]] || fail "could not count agent messages in the group room"
	[[ $got -eq $2 ]] || fail "$3 (expected $2 matching messages, found $got)"
}

group_said_at_least() {
	local got
	got="$(group_agent_said "$1")"
	[[ $got =~ ^[0-9]+$ ]] && [[ $got -ge $2 ]]
}

group_send() {
	local token="$1" body="$2"
	TXN="e2e-$(date +%s%N)"
	mx PUT "/_matrix/client/v3/rooms/$GROUP_URI/send/m.room.message/$TXN" \
		"$token" "$body" >/dev/null
}

# Both `!chaz` commands used below are answered by the bridge itself, so their
# replies do not depend on the daemon or the stub, and each reply carries a
# string the other does not.
BACKENDS_MARKER="Known Models"
HELP_MARKER="chaz commands"

MENTION_BODY="mentioning the agent"

# How many turns the model has been asked to run. The bridge backfills room
# history into the session, so the text of an ignored message still shows up in
# a later turn's context — the count of requests is what says whether it became
# a turn of its own.
stub_requests() { grep -c "^stub_llm: request:" "$WORKSPACE/stub-llm.log" || true; }

# The bridge writes a line for every message it drops for not being addressed
# to it. That line is the observable for a message that must be ignored: the
# absence of a reply is not, because absence is also what a reply that has not
# arrived yet looks like, and it stays true with the gate deleted.
gate_drops() { grep -c "not addressed to the bot" "$BRIDGE_LOG" || true; }
gate_dropped_more_than() { [[ "$(gate_drops)" -gt "$1" ]]; }

# Case 1a + Case 2 — the bare message must be ignored, the mention answered.
#
# The bare message is asserted on the bridge's drop line, and waiting for it
# also orders the pair: the mention is not sent until the bridge has finished
# with the message before it. The bridge decides this per event, before
# anything crosses into a session, so neither the assertion nor the ordering
# depends on how the daemon batches the turns it is handed.
#
# The turn count is the second half of the pair, and it is what catches a drop
# that is logged but not performed. Counting requests rather than replies in
# the room is deliberate: every reply this stub sends is the same string, so a
# wrongly sent first reply reads exactly like the mention's.
log "Case 1a: bare message in the group room (must be ignored)"
TURNS_BEFORE="$(stub_requests)"
DROPS_BEFORE="$(gate_drops)"
group_send "$PUPPET_TOKEN" \
	"$(jq -nc '{msgtype:"m.text",body:"bare message with no prefix and no mention"}')"
wait_for "the bridge to drop the bare group message as unaddressed" 60 \
	gate_dropped_more_than "$DROPS_BEFORE"

log "Case 2: @-mention in the group room (must be answered)"
group_send "$PUPPET_TOKEN" "$(jq -nc --arg a "$AGENT_MXID" --arg b "$MENTION_BODY" \
	'{msgtype:"m.text",body:$b} + {"m.mentions":{user_ids:[$a]}}')"

mention_reached_the_model() {
	grep "^stub_llm: request:" "$WORKSPACE/stub-llm.log" | grep -qF "$MENTION_BODY"
}
wait_for "the mention to reach the model" "$REPLY_TIMEOUT" mention_reached_the_model

TURNS_AFTER="$(stub_requests)"
if [[ $((TURNS_AFTER - TURNS_BEFORE)) -ne 1 ]]; then
	fail "bridge ran $((TURNS_AFTER - TURNS_BEFORE)) turns for the group room, expected 1 — a bare message was answered"
fi

wait_for "the mention reply in the group room" "$REPLY_TIMEOUT" \
	group_said_at_least "$MARKER" 1

# Case 1b — the `!chaz` prefix is answered without a mention.
log "Case 1b: !chaz command in the group room (must be answered)"
group_send "$PUPPET_TOKEN" "$(jq -nc '{msgtype:"m.text",body:"!chaz backends"}')"
wait_for "the !chaz reply in the group room" "$REPLY_TIMEOUT" \
	group_said_at_least "$BACKENDS_MARKER" 1

# Case 3 — allow_list rejection. The stranger uses the `!chaz` prefix on
# purpose: the prefix clears the "only engage when addressed" gate that Case 1a
# already covers, which leaves allow_list as the only thing that can produce
# silence. A bare message here would be dropped by the addressing gate, and the
# case would still pass with allow_list deleted entirely.
log "Case 3: stranger sends !chaz (must be ignored — not in allow_list)"
group_send "$STRANGER_TOKEN" "$(jq -nc '{msgtype:"m.text",body:"!chaz backends"}')"
group_send "$PUPPET_TOKEN" "$(jq -nc '{msgtype:"m.text",body:"!chaz help"}')"
wait_for "the !chaz help reply in the group room" "$REPLY_TIMEOUT" \
	group_said_at_least "$HELP_MARKER" 1
group_said_count_is "$BACKENDS_MARKER" 1 \
	"bridge answered a sender outside allow_list"

printf '\033[1;32mPASS\033[0m — group room cases passed (bare ignored, !chaz prefix, @-mention, allow_list rejection)\n' >&2

# -------------------------------------------------------- ReAct case ---
# The stub answers a request whose user messages ask for a tool call with a
# tool_calls response, and returns text only once a follow-up request carries
# the result. If chaz's ReAct loop is broken — it doesn't process tool calls,
# doesn't send results back, or hits an iteration limit — the stub never
# returns the MARKER and this case times out.
#
# The trigger is the message body, not a request counter: a counter would hand
# the tool call to the first turn of the run, which is the cold-boot case, and
# leave this assertion matching a log line written before this case ran.

log "ReAct case: sending message to trigger tool-call cycle"
TXN="e2e-$(date +%s%N)"
mx PUT "/_matrix/client/v3/rooms/$(jq -rn --arg r "$ROOM_ID" '$r|@uri')/send/m.room.message/$TXN" \
	"$PUPPET_TOKEN" "$(jq -nc '{msgtype:"m.text",body:"react test"}')" >/dev/null

wait_for "fifth reply (ReAct cycle)" "$REPLY_TIMEOUT" replies_at_least 5

if ! grep -q "detected tool result" "$WORKSPACE/stub-llm.log"; then
	fail "ReAct loop did not complete: stub never received a tool result in a follow-up request"
fi

printf '\033[1;32mPASS\033[0m — ReAct case passed (tool-call cycle completed)\n' >&2
