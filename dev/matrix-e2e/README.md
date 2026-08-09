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
