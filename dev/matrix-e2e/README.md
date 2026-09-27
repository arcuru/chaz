# Matrix bridge end-to-end test

Stands up a throwaway Matrix homeserver, drives a real conversation through the
bridge, and checks ordinary Matrix-origin finals, terminal `no_reply({})`, and
explicit `matrix__send` delivery to the attached room.

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
agent peer, and syncs an addressed outbox event back to the bridge. An
ordinary final from a Matrix-origin turn is saved locally and posted through
the same outbox; a local-origin final never posts. That round trip spans four
processes and the sync layer between them, which unit tests cannot reach.

It does not use a paid model. The stub deterministically uses `matrix__send`
for explicit-send cases (with a later local-only final), calls terminal
`no_reply({})` on ambient chatter, and produces an ordinary final for a
separate ambient reply case. A transport pass checks exact outbound counts,
not merely whether final text was generated.

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
                                              addressed send syncs out
```

Four things have to hold for a run to pass, and each is a distinct failure:

1. The bridge bootstraps into the agent's DB with the key it was granted.
2. An inbound room message becomes an entry in a session DB.
3. That session DB reaches the daemon, which notices and runs a turn.
4. The addressed send syncs back; only the matching bridge delivers it to the room.

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
  a turn. An attached room records it as observation; the next mention
  exposes it in model context. Asserted on the bridge's observation log,
  the stub turn count, and that later context. The bridge
  runs with `chaz_matrix_bridge=debug` so that line is in its log.
- **@-mention (Case 2):** the same text with the agent mentioned must be
  answered.
- **`!chaz` prefix (Case 1b):** the command channel is answered without a
  mention.
- **allow_list (Case 3):** the stranger sends `!chaz`, which clears the
  addressing gate, so `allow_list` is the only thing left that can produce
  silence. A bare message here would prove nothing Case 1a does not.

### Encrypted rooms

A cross-signed puppet device opens an encrypted DM with the agent. A second,
unsigned puppet device shares the room:

- **E1/E2** — a plain message and a `!chaz` command are decrypted and answered,
  and the probe decrypts the replies.
- **E3** — the unsigned device receives the same reply as undecryptable, with
  the key withheld as `m.unverified`. That proves the bridge knew about the
  device and withheld the key on purpose, rather than missing it.
- **E4** — a message from the unsigned device is logged as undecryptable and
  gets no answer. A later message from the signed device is the barrier.
- **E5** — after a bridge restart the device id and device key are unchanged,
  no new cross-signing identity is created, and the room still works.
- **E6** — the server's copy of the room holds only `m.room.encrypted` events
  from the agent, never a plaintext `m.room.message`.

### Room reset

The `chaz-matrix rooms` maintenance command runs last, against the same
throwaway homeserver: `rooms list` lists every room the agent is joined to,
`rooms leave-all` previews the reset, `rooms leave-all --execute` leaves them
all, and a final list reports nothing to do — the reset is idempotent, and the
agent's password never appears in the command's output.

## Transport

The harness supports two transport modes for eidetica sync between the daemon
and bridge peers.

| Flag               | Behavior                                                                                                                                                                                                                                                                                                                   |
| ------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--transport http` | **Default.** Both peers set `eidetica.sync.http_listen` to loopback. Neither registers an Iroh endpoint, so sync stays local.                                                                                                                                                                                              |
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

- **The main rooms are unencrypted; one room is encrypted.** The plaintext
  cases use plain HTTP against the client-server API, with no client library.
  Encryption needs a real Olm/Megolm device, so the encrypted cases drive
  `e2ee_probe` (`crates/matrix-bridge/examples/e2ee_probe.rs`), a small
  matrix-sdk client that keeps each device's store in the workspace. `just e2e`
  builds it; override its path with `E2EE_PROBE_BIN`.
- **The bridge starts from a pre-encryption session.** The harness logs the
  agent in over HTTP and writes a session file with no crypto store, like every
  bridge deployed before encryption support. The first start must upgrade that
  device in place. Creating a fresh device would pass the other cases and miss
  the path a real upgrade takes.
- **HTTP mode registers only loopback transports.** It also strips any
  `iroh:` ticket hints defensively; `--transport iroh` keeps them.
- **`XDG_CONFIG_HOME` is redirected into the workspace**, because
  `/agent share` writes a copy of every ticket under the config directory.
- **Ports are requested from the kernel**, not hardcoded, so a run does not
  collide with a daemon already running on the machine.
- **Every `curl` carries `--max-time`.** A poll loop is only bounded if each
  attempt is, and a server that accepts a connection and then stops answering
  is otherwise an attempt that never returns.

## Unpublished dependency: local acceptance only

The E2EE dependency pin is Eidetica
`94c2624c7793c798847240c3287ff06280420eed` (sqlx 0.9). It is not yet
published at the public Git URL. The manifest and lock name that final revision;
**a fresh public checkout cannot build until it is published**. Do not substitute
the old pin with a sqlx backport: that tests a different API and source tree.

With a local Git repository containing that exact commit, create an immutable
copy and a disposable consumer snapshot. Only the snapshot's dependency URL is
rewritten; no path patch or file URL belongs in the committed consumer:

```bash
# Run from a committed Chaz checkout; set this to the local Eidetica repository.
EIDETICA_REPO=/path/to/eidetica
REV=94c2624c7793c798847240c3287ff06280420eed
WORK=$(mktemp -d -p /tmp)
git clone --no-hardlinks "$EIDETICA_REPO" "$WORK/eidetica"
git -C "$WORK/eidetica" checkout --detach "$REV"
test "$(git -C "$WORK/eidetica" rev-parse HEAD)" = "$REV"
test -z "$(git -C "$WORK/eidetica" status --porcelain)"
chmod -R a-w "$WORK/eidetica"
mkdir "$WORK/chaz"
git archive HEAD | tar -x -C "$WORK/chaz"
sed -i "s|https://github.com/arcuru/eidetica|file://$WORK/eidetica|g" \
    "$WORK/chaz/Cargo.toml" "$WORK/chaz/Cargo.lock"
cd "$WORK/chaz"
export CARGO_TARGET_DIR="$WORK/target"
export CHAZ_BIN="$CARGO_TARGET_DIR/debug/chaz"
export CHAZ_MATRIX_BIN="$CARGO_TARGET_DIR/debug/chaz-matrix"
export E2EE_PROBE_BIN="$CARGO_TARGET_DIR/debug/examples/e2ee_probe"
export EIDETICA_FLAKE="git+file://$WORK/eidetica?rev=$REV"
CI=1 nix develop .# -c just nix full
nix develop .# -c just e2e --keep --timeout 60
```

`EIDETICA_FLAKE` controls the CLI that provisions both disposable peer stores;
it must point to the same revision as the consumer. Without the override the
harness derives the public pin from `Cargo.toml`. Keep the logs and both source
revisions when reporting results. An overlay pass establishes local integration,
not publication, deployment, or compatibility of an existing production database.

When upgrading an existing Eidetica database across verification-rule changes,
follow Eidetica's [offline trust-reset procedure](https://github.com/arcuru/eidetica/blob/main/docs/src/design/verification.md#explicit-trust-reset-on-verification-rule-upgrades)
with every owner stopped and a backup retained. This is an operator step, not
an automatic startup reset. The fixture uses fresh Eidetica databases and upgrades
only the legacy Matrix session/device state.

### Supervised live smoke test without production state

A local pass does not establish live-homeserver compatibility or a safe upgrade of
existing Eidetica verification labels. Keep those as separate operator gates:

1. With operator approval, create disposable bot and sender accounts on the target
   homeserver. Use a new, isolated Chaz daemon/bridge configuration, fresh Eidetica
   databases, separate Matrix stores, and the local stub model. Do not copy production
   tokens, databases, login directories, or account cross-signing secrets into them.
2. Cross-sign the sender's device, then test an encrypted DM: ordinary text and
   `!chaz help` must get decryptable replies. Inspect raw room history for
   `m.room.encrypted`, not only the client's decrypted display. No manual trust
   ceremony with the bot should be required for these fresh identities.
3. Add an unsigned sender device. Confirm deliberate reply-key withholding and
   ignored incoming messages; also check an unencrypted room and allow-list rejection.
   Do not weaken policy or reset an identity to make a failing case work.
4. Stop and restart only the isolated bridge, keeping its session and store together.
   Confirm unchanged device/key fingerprints, no replacement identity, and another
   encrypted reply. Retain redacted logs and revision/binary fingerprints, then stop
   the test processes and retire the disposable accounts with operator approval.
5. Before a later production upgrade, separately assess the old Eidetica revision's
   verification rules and inventory all owners. Rehearse the documented offline
   trust reset and re-verification on an isolated backup copy with network sync
   disabled. Reset only disposable local verification labels, not Matrix identities
   or immutable Entries. A successful fresh-state smoke test is not migration proof;
   do not start the new version against production data until the operator approves
   the backup, offline procedure, and rollback plan.

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

## Key-maintenance acceptance

Run `just e2e-keys` for the actual local `chaz-matrix keys` CLI against a separate
disposable Synapse. This exercises staged reset UIAA cancellation/rejection,
process death with real replacement keys, server commit with a lost query
response, safe resume, fresh-store same-identity/standard-backup restore, wrong
secrets, private recovery files and own-account SAS with negative controls.
A test-only Untrusted diagnostic proves imported key material without changing
the bridge's CrossSigned policy. No model is called.

`CHAZ_MATRIX_BIN` and `KEY_PROBE_BIN` can select previously built binaries.
`KEY_TEST_WORKSPACE` selects a retained private fixture directory; use a new one
for each run. Otherwise the harness creates one in the temporary directory.
Account credentials and recovery material stay in that private fixture tree.
Only public fingerprints, test names and the final counted summary are output.
The whole transport suite (`just e2e`) separately verifies that a live bridge
rejects concurrent key maintenance.
