# Matrix bridge end-to-end test

Stands up a throwaway Matrix homeserver, drives a real conversation through the
bridge, and asserts the reply comes back out of the room.

```bash
just e2e                        # from inside `nix develop`
just e2e --keep                 # leave the workspace behind to poke at
just e2e --verbose              # stream component logs while it runs
just e2e -- --transport iroh    # exercise P2P transport path
```

Exit status is the result: `0` passed, `1` failed, `2` the harness could not
start.

CI runs the default `http` transport as the `Matrix E2E` job in
`.github/workflows/ci.yml`, separate from the fast checks because it pulls
Synapse into the closure. It needs no secrets.

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

### Restart and reconnection

Two restart cases follow the cold-boot conversation to catch regressions in
persistence and reconnection:

- **Bridge restart (Case A):** The bridge is killed and restarted
  mid-conversation. The bridge key persists on disk, so it must reconnect
  without asking for re-authorization — a pending-approval line in the
  restarted bridge log is a hard failure. A third message is sent through the
  restarted bridge and the reply is asserted.
- **Daemon restart (Case B):** The daemon is killed and restarted
  mid-conversation. The daemon must come back online and resume answering
  messages in the same room. A fourth message is sent and the reply is
  asserted.

Both cases use `replies_at_least <n>` rather than `reply_arrived`, because
`reply_arrived` checks `length > 0` and would pass instantly on the first
reply without testing the restart.

### Group rooms and allow_list

A third room holds the cases about who the bridge answers, with the puppet, the
stranger, and the agent all joined:

- **Bare message (Case 1a):** unaddressed text in a group room must not become
  a turn. Asserted on the line the bridge writes when it drops a message for
  not being addressed to it, then on the stub's request count: the first says
  the bridge made the decision, the second says no turn ran anyway. The bridge
  runs with `chaz_matrix_bridge=debug` so that line is in its log.
- **@-mention (Case 2):** the same text with the agent mentioned must be
  answered.
- **`!chaz` prefix (Case 1b):** the command channel is answered without a
  mention.
- **allow_list (Case 3):** the stranger sends `!chaz`, which clears the
  addressing gate, so `allow_list` is the only thing left that can produce
  silence. A bare message here would prove nothing Case 1a does not.

## Transport

The harness supports two transport modes for eidetica sync between the daemon
and bridge peers.

| Flag               | Behavior                                                                                                                                                                                                                                                                                                                   |
| ------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--transport http` | **Default.** Both peers listen on loopback HTTP. The daemon's `sync_listen` is set and iroh hints are stripped from the ticket. Fast, reliable, good for CI iteration.                                                                                                                                                     |
| `--transport iroh` | Neither peer binds a sync port. They discover each other through iroh's DHT/relay mechanism instead. Expected to be slower and less reliable — that's the point, it exercises the production transport path that unit tests and the default http mode can't reach. If it doesn't reliably connect, that's actionable data. |

`--transport iroh` is the one mode that is **not hermetic**: iroh discovery
reaches n0's public relay and DHT infrastructure, so the run needs the internet
and can fail for reasons outside this repository. Everything else — the
homeserver, the accounts, the model — stays on loopback in both modes, and CI
runs http only.

Pass transport flags after `--` so `just` forwards them to the script rather
than consuming them:

```bash
just e2e -- --transport iroh
```

## What it stands up

| Process       | Role                                                          |
| ------------- | ------------------------------------------------------------- |
| Synapse       | Throwaway homeserver on a free port, loopback only            |
| `stub_llm`    | OpenAI-compatible endpoint returning one fixed reply          |
| `chaz`        | The agent peer, in `daemon` mode                              |
| `chaz-matrix` | The bridge, logged in as `@agent:e2e.test`                    |
| puppet        | `curl` against the client-server API, standing in for a human |

Three accounts are registered per run — `@agent:e2e.test` for the bridge,
`@puppet:e2e.test` for the human side, and `@stranger:e2e.test` for a sender
the bridge must refuse. Passwords are generated per run. Everything lives in
one `mktemp -d` workspace and is removed on exit, including after a failure or
a Ctrl-C.

In the default `http` transport nothing here reaches off the machine. Under
`--transport iroh` the two peers find each other through public relay
infrastructure, so that mode alone needs the internet.

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
- **The ticket's `iroh:` hints are stripped** (in `--transport http` mode only;
  `--transport iroh` keeps them). The daemon mints a fresh iroh
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
| `replies_at_least <n>`            | Assert at least `n` replies with the stub marker (multi-turn); see below |
| `fail <message>`                  | Abort with a red message and exit 1                                      |
| `mx <METHOD> <path> [tok] [body]` | One client-server API call against the throwaway homeserver              |

Assert on an observable, never on a sleep. Every wait is bounded, because a
harness that hangs is worse than one that fails — CI will sit on it until the
job timeout, and locally it looks like a wedge rather than a bug.

Prefer an observable the component writes down itself over one inferred from
what did not happen. A case that asserts nothing happened passes on a system
that is merely slow, and passes just as well on one where the feature is gone.

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

**A tool call** is driven by the message body. `stub_llm.py` answers with a
`tool_calls` response when a user message asks for one, and with the
fixed reply otherwise, so the ReAct case owns its own turn and every other turn
stays on the plain path. Branch on content, never on a request counter — a
counter attaches the special response to whichever turn arrives first, which is
the cold-boot turn.

**A case that asserts silence** needs a barrier, not a wait. Send the message
that must be ignored, then one that must be answered, wait for the second
answer, and only then assert the first produced nothing. A fixed window asserts
only that the bridge is slower than the window, and it costs that window on
every green run.

The barrier's answer has to be distinguishable from the forbidden one, or the
assertion fires on whichever landed first and passes for the wrong reason.
Every model reply carries the same fixed string, so two model replies cannot be
told apart in the room. For the model path, wait on `stub-llm.log` — it logs
one `request:` line per turn with the user messages that turn was given — and
assert on how many turns ran. For the `!chaz` path, pick two commands whose
replies differ.

`stub-llm.log` also answers a question the room cannot: whether a message
became a turn at all. The bridge backfills room history into the session, so an
ignored message still appears in a later turn's context; the count of `request:`
lines is what distinguishes context from a turn.

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
