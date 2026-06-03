# Sharing & Sync

Chaz instances can share sessions, agents, and memory banks over the network using [eidetica's sync protocol](https://github.com/arcuru/eidetica). Use cases include viewing a remote agent's conversation from a local TUI, multiple instances collaborating on the same session, sharing a curated knowledge base across deployments, and inviting a co-owner to an agent.

## How It Works

Each chaz instance starts an HTTP sync server automatically at startup. The server address is logged:

```text
INFO Eidetica sync listening on 127.0.0.1:12345
```

Shared databases are advertised via **database tickets** -- URLs that encode the database ID and the source peer's network address. Eidetica handles the sync protocol, entry replication, conflict resolution, and (for sessions) bidirectional propagation. A `/share` command flips eidetica's per-DB `sync_enabled` flag for the current peer so it actually serves the DB to ticket holders, then prints the ticket; an `/import` or `/sync` command on the receiver does the inverse.

```mermaid
graph LR
    A[Instance A<br/>Source peer] -->|"/share, /agent share, /memory share"| T["eidetica:?db=...&pr=http:..."]
    T -->|"/sync, /agent import, /memory import"| B[Instance B<br/>Receiver peer]
    A <-->|eidetica sync| B
```

Three things sync as separate database kinds, each with its own commands:

| Kind         | Source command        | Receiver command       | What syncs                              |
| ------------ | --------------------- | ---------------------- | --------------------------------------- |
| Session      | `/share`              | `/sync <ticket>`       | Conversation entries + session metadata |
| Living Agent | `/agent share <ref>`  | `/agent import <tkt>`  | Agent config + per-agent memory         |
| Memory Bank  | `/memory share <ref>` | `/memory import <tkt>` | Bank contents + grants                  |

Tickets and the on-the-wire protocol are identical across kinds; the source and receiver commands diverge because each kind has different post-import bookkeeping (registering with the local hosted index, etc.).

## Sharing a Session

On the instance that has the session:

```text
/share
```

Output:

```text
Share this ticket to sync the current session:

eidetica:?db=<database_id>&pr=http:192.168.1.10:12345
```

Paste the ticket on the receiving instance:

```text
/sync eidetica:?db=<database_id>&pr=http:192.168.1.10:12345
```

After syncing, the session appears in the session list. Use `/sessions` to find and open it.

## Sharing an Agent or Memory Bank

There are two paths to give another peer access to an agent or memory bank: a **request flow** (the receiver asks; the owner approves) and a **preseed flow** (the owner authorizes the receiver's pubkey out-of-band first). The request flow is the new default — fewer steps and read-only sharing works.

### Request flow (default)

```text
# On Source (peer A): generate the share ticket
/agent share my-agent
# Output: eidetica:?db=<agent_db_id>&pr=http:...

# On Receiver (peer B): request access using the ticket
/agent import eidetica:?db=<agent_db_id>&pr=http:... write
# Output: Bootstrap request <id> pending the owner's approval.
#         Re-run `/agent import <ticket>` after they run `/sharing approve <id>`.

# On Source (peer A): see and approve the request
/sharing requests
# Output: Pending bootstrap requests (1):
#           <id> — agent 'my-agent' requested by ed25519:... as write(10) at <ts>
/sharing approve <id>

# On Receiver: re-run the import — now it succeeds
/agent import eidetica:?db=<agent_db_id>&pr=http:... write
# Output: Imported agent 'my-agent' (DB ...). Attach with /agent add my-agent.
```

The trailing `[admin|write|read]` token on `/agent import` (and `/memory import`) controls the permission the requester asks for. Default: `write`. `read` works for spectators; `admin` for co-owners.

### Preseed flow (still supported)

If the source peer authorizes the receiver's pubkey ahead of time via `/agent invite`, the receiver's `/agent import` succeeds immediately — eidetica's handler returns the existing-key fast path without queueing a request.

```text
# On Receiver: copy your default pubkey
/pubkey
# Output: ed25519:abc123...

# On Source: preseed
/agent invite my-agent ed25519:abc123... write
/agent share my-agent

# On Receiver: import — no approval needed
/agent import <ticket>
```

This is the right shape when the two peers can communicate out-of-band before the import (e.g. you control both, or you've exchanged pubkeys via Matrix). When they can't, use the request flow.

### Memory banks and sessions

Memory banks use the same `/memory share` + `/memory import [perm]` shape. Sessions use `/share` + `/sync <ticket>` — sessions don't have a permission flag because session sharing today implies write access.

The `/sharing` command surface is unified across kinds. Bare `/sharing` (or `/sharing status`) lists every database this peer is currently sharing, grouped by kind with DB root IDs. `/sharing requests`, `/sharing approve <id>`, `/sharing reject <id>` operate on the single eidetica request queue regardless of whether the request is for an agent, bank, or session DB. The list output names the resource when the target DB is hosted on this peer.

To stop sharing a database — without revoking keys already held by peers who imported it — use `/unshare` for the current session, `/agent unshare <ref>` for an agent, or `/memory unshare <bank>` for a memory bank.

### After approval

Eidetica doesn't push approved requests back to the requester — the request_id lives on the **source** peer's `_sync` DB only. So after `/sharing approve <id>` the receiver has to re-run their original import command. Re-runs are cheap: tickets just carry the DB id and the source peer's sync addresses.

## Example: Co-owning an Agent Across Two Peers

A walkthrough of the full co-ownership lifecycle — invite, share, attach, observe the home-peer gate firing, hand off execution, recover from an offline home. Two TUIs against separate state dirs. The home-peer reference is in [Agents → Execution ownership](./agents.md#execution-ownership-home-peer).

### Setup

Two state dirs, two configs:

```bash
# Peer A
chaz --config ~/peer-a.yaml

# Peer B (separate terminal, separate state_dir in its config)
chaz --config ~/peer-b.yaml
```

### 1. Create and share the agent (Peer A)

```text
> /agent new alpha model=opus
Created agent 'alpha' …

> /agent home-status alpha
agent: alpha (db_id: sha256:abc…)
  agent-level home: ed25519:PK_A_alpha… ← (me)
  sessions (0):
```

`alpha` is solo. The agent-level home is Peer A automatically (the creator).

### 2. Invite Peer B

```text
# On Peer B:
> /pubkey
ed25519:PK_B_default…

# On Peer A:
> /agent invite alpha ed25519:PK_B_default… admin
Invited ed25519:PK_B_default… as Admin on agent 'alpha' …

> /agent share alpha
eidetica:?db=sha256:abc…&pr=…

# On Peer B:
> /agent import <ticket>
Imported agent 'alpha' (DB sha256:abc…). Attach with /agent add alpha.
```

Now `alpha` is co-owned: both peers hold an authorized key.

### 3. Attach to a shared session

Create a session, share it, attach `alpha` on both sides:

```text
# On Peer A:
> /new          # or just send a message in the current session
> /agent add alpha
Attached agent 'alpha' to this session.

> /share
eidetica:?db=sha256:def…&pr=…

# On Peer B:
> /sync eidetica:?db=sha256:def…&pr=…
> /sessions             # select the synced session
> /agent add alpha
```

Verify state from either side:

```text
> /agent home-status alpha
agent: alpha (db_id: sha256:abc…)
  agent-level home: ed25519:PK_A_alpha… ← (me)        # on Peer A — Peer B sees no ← (me) here
  sessions (1):
    sha256:def…  this room      ed25519:PK_A_alpha…   # ← (me) on A, plain on B
```

`alpha`'s per-session home is **Peer A** — the peer that attached first wins. Peer B is a keyholder but not the home.

### 4. Watch the gate work

Send a human message in the shared session.

- **Peer A** runs `alpha`'s turn and writes the response.
- **Peer B**'s log shows `Not home peer for this session/agent; skipping turn` and writes nothing.

Single agent response. No fork. This is the whole point.

### 5. Hand off execution to Peer B

```text
# On Peer B:
> /agent rehost alpha
Set session-level home_pubkey for agent 'alpha' to ed25519:PK_B_alpha…
```

After eidetica syncs the meta write back to A, the next human message is run by Peer B. Confirm with `/agent home-status alpha` from either side — the session row now shows `PK_B_alpha`.

### 6. Recover from an offline home

Stop Peer B. From Peer A, send three messages in the shared session. After the third, the chaz log on Peer A shows (structured fields `session_db_id`, `agent`, `skipped_wakes` accompany the message):

```text
WARN session_db_id="sha256:def…" agent="alpha" skipped_wakes=3
     Home peer has missed 3 consecutive wakes for this session/agent.
     If this is a stuck home, run `/agent rehost alpha` from a surviving peer to take over.
```

Take over:

```text
# On Peer A:
> /agent home-status alpha    # confirm what's silent
> /agent rehost alpha
```

The next message wakes Peer A. When Peer B comes back online, its gate sees `home_pubkey = PK_A_alpha` and silently skips that session until you hand it back.

### 7. Revoke with awareness

If you ever revoke a co-owner whose key is still the home for one or more sessions, the success message names what needs rehosting:

```text
> /agent revoke-peer alpha ed25519:PK_B_alpha…
Revoked ed25519:PK_B_alpha… from agent 'alpha'. They retain read access to history but cannot write.

WARNING: revoked key was the home peer for 1 session(s): sha256:def…. Their next turn will be silent until you run `/agent rehost alpha` from a surviving peer.
```

Revoke doesn't block on this — but the warning surfaces what's about to go silent so you can act before users notice.

## Example: Watching a Matrix Bot's Session

1. Start the Matrix bot on a server:

   ```bash
   chaz --config /etc/chaz/config.yaml
   # Logs: Eidetica sync listening on 0.0.0.0:12345
   ```

2. Start a local TUI:

   ```bash
   chaz --config ~/chaz-local.yaml
   ```

3. On the server (via a second TUI or programmatically), get the session ticket:

   ```text
   /join !roomid:matrix.org
   /share
   # Output: eidetica:?db=<id>&pr=http:myserver.com:12345
   ```

4. On the local TUI, sync and open:

   ```text
   /sync eidetica:?db=<id>&pr=http:myserver.com:12345
   /sessions
   ```

5. Select the synced session. You'll see the full conversation history. New messages from Matrix should appear in real time — see _Troubleshooting_ if they don't.

## Requirements

- Both instances must be able to reach each other over the network
- The sync server binds to a random port by default
- Firewalls must allow the sync port (check the startup log for the address)
- Both instances use separate eidetica databases (separate `state_dir` paths)
- Sharing a database flips its `sync_enabled` flag on the source peer; this persists across restarts. To stop sharing a DB, use `/unshare` (session), `/agent unshare <ref>`, or `/memory unshare <bank>`.

## Ticket Format

Tickets use a magnet-style URI format:

```text
eidetica:?db=<database_id>&pr=<transport>:<address>
```

Multiple peer addresses can be included (the receiver tries them concurrently and succeeds on the first):

```text
eidetica:?db=<id>&pr=http:192.168.1.10:8080&pr=http:10.0.0.1:8080
```

See the [eidetica documentation](https://github.com/arcuru/eidetica) for details on the sync protocol, transport types, and ticket format.

## Troubleshooting

**The receiver synced but new messages from the source don't appear.**
This was a real bug fixed in 2026-04 — `/share`/`/agent share`/`/memory share` were generating valid-looking tickets but the source peer wasn't actually advertising the DB. If you see this on a build older than the Database Layout Refactor Stage 1 fix, upgrade. On current builds, also check that:

- Both peers are on the same eidetica protocol version (chaz pins eidetica to a specific revision; mismatched peers won't handshake).
- The source peer's sync server log shows incoming connections from the receiver's address.

**A co-owner's edits to a synced session don't trigger an agent run on the host.**
Known limitation: chaz only listens to `on_local_write`, but remote-write callbacks were dead code in eidetica until a recent fix. Until that fix is merged and chaz subscribes to `on_remote_write`, remote pushes land in the database silently — they're visible if you re-render the session, but agents won't react to them. Tracked as the "Remote-write callback subscription" item in the followups.

**`/agent import` reports `Bootstrap request <id> pending` instead of importing.**
Expected when the receiver's pubkey isn't preseeded. The owner needs to run `/sharing requests` to see the queue, then `/sharing approve <id>` to grant the requested permission. After approval, the receiver re-runs `/agent import <ticket>` and the import completes. See "Request flow" above.

**`/sharing approve` errors with "this peer holds no key for DB ... — only an admin on that DB can approve".**
You ran `/sharing approve` on a peer that doesn't own the target DB. Only the original creator (Admin(0)) or a co-admin (Admin(1)) can approve. Run it on the peer that hosts the agent/bank/session.

**Tickets stop working after a restart.**
With the default iroh P2P transport, peer identity is stable across restarts — tickets continue to work. If you're using the optional HTTP transport (`sync_listen` config), tickets carry the listen address and will go stale if the address changes. Either use a fixed address or switch to iroh-only (omit `sync_listen`).

## Limitations

- The sync server address/port is configurable via `sync_listen` for the HTTP transport; the default iroh P2P transport doesn't need one.
- No authentication on the sync connection beyond the ticket capability and per-DB auth keys (any peer with the ticket can reach the source's sync server; whether they can read/write a specific DB is gated by eidetica's `AuthSettings`).
- Registry index entries (Matrix channel bindings, name index) are local to each peer — only the database contents (entries + meta) sync.
- To make a synced session reachable from a specific Matrix room on the receiver, run `!chaz attach <name-or-id>` in that room after syncing.
