# Matrix bridge end-to-end test

Stands up a throwaway Matrix homeserver, drives a real conversation through the
bridge, and asserts the reply comes back out of the room.

```bash
just e2e                 # from inside `nix develop`
just e2e --keep          # leave the workspace behind to poke at
just e2e --verbose       # stream component logs while it runs
```

Exit status is the result: `0` passed, `1` failed, `2` the harness could not
start.

## What it covers

A message enters over Matrix, crosses into a session database, syncs to the
agent peer, gets answered, and syncs back out to the room. That round trip
spans four processes and the sync layer between them, and it is the part that
unit tests cannot reach.

It is deliberately not a test of the model, the prompt, or the tools. The LLM
is a stub that returns one fixed string, so a pass means the transport carried
it and a failure is never someone else's outage.

### The split it exercises

The bridge and the agent are two separate peers, each with its own backend file
and its own key. They share no process and no database handle; everything
between them moves through eidetica sync.

```
@puppet ──Matrix──▶ Synapse ──▶ chaz-matrix ──┐
                                  (bridge)     │  eidetica sync
                                  no agents    │  over loopback HTTP
                                  no LLM       ▼
                                              chaz daemon
                                              runs the agent ──▶ stub LLM
                                              reply syncs back out
```

Four things have to hold for a run to pass, and each is a distinct failure:

1. The bridge bootstraps into the agent's DB with the key it was granted.
2. An inbound room message becomes an entry in a session DB.
3. That session DB reaches the daemon, which notices and runs a turn.
4. The reply syncs back and the bridge delivers it to the room.

Because the harness waits on a specific observable at each step — the daemon's
readiness line, the bridge's Matrix login, the agent's join, then the reply —
the step that times out tells you which of the four broke, without reading a
log first.

Two constraints worth holding onto, because both have caused confusing
failures:

- **`chaz_group` and `chaz_peer` never sync.** Routing metadata and credentials
  are peer-local by design. If a test seems to need one of those to cross
  between the bridge and the daemon, the test is wrong, not the sync layer.
- **A login belongs to exactly one agent.** There is no shared-login gateway,
  so a second agent in a test needs its own Matrix account.

## What it stands up

| Process       | Role                                                          |
| ------------- | ------------------------------------------------------------- |
| Synapse       | Throwaway homeserver on a free port, loopback only            |
| `stub_llm`    | OpenAI-compatible endpoint returning one fixed reply          |
| `chaz`        | The agent peer, in `daemon` mode                              |
| `chaz-matrix` | The bridge, logged in as `@agent:e2e.test`                    |
| puppet        | `curl` against the client-server API, standing in for a human |

Two accounts are registered per run — `@agent:e2e.test` for the bridge and
`@puppet:e2e.test` for the human side. Passwords are generated per run.
Everything lives in one `mktemp -d` workspace and is removed on exit, including
after a failure or a Ctrl-C.

## Bring-up without a human

The bridge normally needs its access request approved by hand. The harness
avoids that entirely:

1. `chaz-matrix --print-pubkey` reports the key the bridge will authenticate as.
2. `chaz cmd '/agent invite chaz <key> write'` pre-authorizes it.
3. `chaz cmd '/agent share chaz'` mints the ticket.

Pre-authorized access bootstraps straight through, and the harness fails loudly
if the bridge logs a pending request instead — that would mean the
pre-authorization silently stopped working, which is exactly the regression this
sequence exists to protect.

## Notes

- **The room is unencrypted, and it has to be.** chaz builds `matrix-sdk`
  without the `e2e-encryption` feature, so an encrypted room is one the bridge
  cannot read. The puppet is plain HTTP against the client-server API for the
  same reason — no Olm, no client library, nothing to keep in sync with the
  bridge's capabilities.
- **The ticket's `iroh:` hints are stripped.** The daemon mints a fresh iroh
  endpoint on every start, so a recorded address is stale as soon as it is
  written, and sync pays a full timeout per dead address. Both processes are on
  loopback, where the `http:` hint is what connects.
- **`XDG_CONFIG_HOME` is redirected into the workspace**, because
  `/agent share` writes a copy of every ticket under the config directory.
- **Ports are requested from the kernel**, not hardcoded, so a run does not
  collide with a daemon already running on the machine.

## Writing a new case

`run.sh` is deliberately a single linear script rather than a framework: it
reads top to bottom, and a new case is usually a few lines inserted where the
existing conversation happens. Four helpers carry most of the weight.

| Helper                            | Use                                                                      |
| --------------------------------- | ------------------------------------------------------------------------ |
| `spawn <name> <cmd...>`           | Start a process, log to `$WORKSPACE/<name>.log`, register it for cleanup |
| `wait_for <what> <secs> <cmd>`    | Poll until `cmd` succeeds, or fail naming `<what>`                       |
| `fail <message>`                  | Abort with a red message and exit 1                                      |
| `mx <METHOD> <path> [tok] [body]` | One client-server API call against the throwaway homeserver              |

Assert on an observable, never on a sleep. Every wait is bounded, because a
harness that hangs is worse than one that fails — CI will sit on it until the
job timeout, and locally it looks like a wedge rather than a bug.

**A second turn in the same room**, to cover context rather than first contact:

```bash
TXN="e2e-$(date +%s%N)"
mx PUT "/_matrix/client/v3/rooms/$(jq -rn --arg r "$ROOM_ID" '$r|@uri')/send/m.room.message/$TXN" \
	"$PUPPET_TOKEN" "$(jq -nc '{msgtype:"m.text",body:"second"}')" >/dev/null
wait_for "the second reply" 120 reply_arrived
```

Note that `reply_arrived` matches on the stub's fixed marker, so it is true the
moment the _first_ reply is present. A second-turn assertion needs its own
predicate — count matching messages and require two, rather than reusing this
one and passing instantly.

**A restart mid-conversation**, which is where the real bugs live:

```bash
kill -TERM "$BRIDGE_PID"          # set when the bridge was spawned
wait_for "bridge to exit" 30 sh -c "! kill -0 $BRIDGE_PID 2>/dev/null"
spawn bridge-restarted "$CHAZ_MATRIX_BIN" --config "$BRIDGE_CONFIG"
BRIDGE_PID="$SPAWNED_PID"
wait_for "bridge back online" 120 grep -q "Matrix login spawned" "$WORKSPACE/bridge-restarted.log"
```

`spawn` leaves the new pid in `$SPAWNED_PID` rather than printing it, because a
command substitution would run it in a subshell where the cleanup registration
is discarded and the process outlives the run. `$DAEMON_PID` and `$BRIDGE_PID`
are already captured that way.

The bridge keeps its key across restarts, so it should come straight back
without re-authorization. A restart that asks to be approved again is a
regression in exactly the identity handling this harness pre-authorizes.

**A different agent or model** means editing the daemon config heredoc. The
`agents:` block there is a first-boot template — the agent DB is the runtime
source of truth, so changing the YAML for an already-populated `state_dir`
changes nothing. Test workspaces are fresh every run, so this only bites when
reusing a `--keep` workspace.

**A tool call** needs the stub to return a `tool_calls` response rather than a
plain message; `stub_llm.py` currently answers every request identically and
would need to branch on the request body to drive a ReAct loop.

### Keep the stub boring

The stub exists so a failure is never ambiguous. If a case starts needing the
model to behave in a particular way, prefer asserting on what reached the stub
(`stub-llm.log` records every request) over teaching the stub to be clever. A
smart stub is a second implementation of the thing under test.

## When it fails

Every component logs into the workspace. Re-run with `--keep` and read:

| File           | What it holds                          |
| -------------- | -------------------------------------- |
| `bringup.log`  | pubkey, invite, and ticket minting     |
| `daemon.log`   | the agent peer                         |
| `bridge.log`   | Matrix login, bootstrap, room handling |
| `synapse.log`  | the homeserver                         |
| `stub-llm.log` | requests the agent actually made       |
| `register.log` | account creation                       |

The failure message names the step that timed out, and each step waits on a
specific observable — the daemon's readiness line, the bridge's Matrix login,
the agent's join, then the reply — so the step that fails is the one that
broke.
