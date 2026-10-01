# Matrix Bot

Chaz connects to Matrix as a bot, responding to messages in rooms it's invited
to. The Matrix bridge runs as its own process (`chaz-matrix`), separate from
the `chaz` daemon — read [Transport Bridges](bridges.md) first for the
architecture, the connection settings, and the one-time approval flow. This page covers Matrix-specific
configuration and behavior.

## Live typing during a turn

| Signal                    | Starts                                                                | Ends                                                                    |
| ------------------------- | --------------------------------------------------------------------- | ----------------------------------------------------------------------- |
| Matrix room typing notice | The bridge sees a fresh executor per-turn start in the shared session | Completion, error, silent release, or a heartbeat older than 45 seconds |

The bridge only observes the per-turn claim; it does not run the agent or infer
activity from a room binding, an old acknowledgement, or session runtime
ownership. Typing is renewed every three seconds while the claim remains
fresh, with a ten-second server timeout, avoiding gaps if a renewal is delayed. On release it sends
a cancellation. A missing executor heartbeat times out instead of showing
permanent typing after a crash; network or sync lag can delay observation.
Discord does not publish this signal.

Example with a shared room:

1. Send `@chaz:example summarize this` in the room. When the executor starts,
   the Matrix client displays `chaz is typing` while the turn runs.
2. On a reply, error, or silent completion, the typing indicator stops even if
   the bridge remains connected to the room. A second message starts a new turn.
3. If the executor crashes, the typing notice expires and the bridge stops
   renewing it when the last heartbeat ages out (within 45 seconds plus sync
   and polling delay). The turn is interrupted; inspect `/interrupted` from a
   client and use `/retry <request_id>` only after checking for external effects.

## Setup

1. **Create a Matrix account** for the bot on any homeserver.

2. **Share the owning agent from the daemon** and copy the ticket:

   ```text
   /agent share chaz
   # eidetica:?db=<agent_db_id>&pr=iroh:<addr>
   ```

3. **Write the bridge config** (default `$XDG_CONFIG_HOME/chaz/matrix-bridge.yaml`,
   or pass `--config`). Each `logins:` entry pairs a Matrix account with its
   owning agent and the ticket from step 2:

   ```yaml
   unlock_password: ${CHAZ_BRIDGE_UNLOCK}

   logins:
     - agent: chaz
       ticket: "eidetica:?db=<agent_db_id>&pr=iroh:<addr>"
       type: matrix
       homeserver_url: https://matrix.example
       username: "@chaz:example"
       password: ${MATRIX_PASSWORD}
       allow_list: "@you:example" # optional; falls back to a top-level allow_list
       # id: <stable-login-id>         # optional; defaults to the MXID
       # room_size_limit: 100          # optional per-login cap

   # chaz runtime keys the embedded server needs:
   state_dir: /var/lib/chaz-matrix
   execution: client
   eidetica:
     connection: "sqlite:///var/lib/chaz-matrix/eidetica.db"
     login: { username: chaz-matrix, passwordless: true }
     sync: { iroh: true }
   backends:
     - name: openai
       api_key: ${OPENAI_API_KEY}
   agents:
     - name: chaz
   ```

   `execution:` and `eidetica:` are the connector settings every Chaz process
   carries — see [Connection settings](bridges.md#connection-settings) for the
   direct-owner and shared-login forms and what each one needs.
   `homeserver_url`, `username`, and `password` are the bot's login; `password`
   (and `unlock_password`) accept `${ENV}` references so no secret has to live in
   the file. `allow_list` / `room_size_limit` may be set per-login or as global
   fallbacks at the top level.

4. **Run the bridge** (with the daemon already running):

   ```bash
   chaz-matrix --config /etc/chaz/matrix-bridge.yaml
   ```

5. **Approve the access request** on the daemon (`/sharing requests` →
   `/sharing approve <id>`) and restart the bridge — see
   [Transport Bridges → Setup](bridges.md#setup).

The bot then logs in, accepts invites from allowed users, and starts responding.

On joining a room, the bridge creates or reuses its session and imports only
messages before the bot's join as context; that history never triggers an agent
turn. On rejoin it imports only messages from the time the bot was away.
`!chaz clear` stops the historical import at that marker. Restarting or
attaching a room to an existing session does not replay history as live input.
A new addressed message is handled once.

### Scripted bring-up

The approval round-trip in step 5 exists because the bridge's key is unknown
until it first asks. Ask it directly instead, and the whole sequence runs
unattended — useful for provisioning and required for automated tests.

Run these with both processes stopped: each opens a state directory, and two
processes on one backend do not observe each other's writes.

```bash
# 1. The bridge's identity, generated on first call and stable thereafter.
KEY=$(chaz-matrix --config matrix-bridge.yaml --print-pubkey)

# 2. Pre-authorize it on the daemon, so there is nothing left to approve.
chaz --config config.yaml cmd "/agent invite chaz $KEY write"

# 3. Mint the ticket.
chaz --config config.yaml cmd '/agent share chaz'
```

Put that ticket in the bridge config, then start the daemon and the bridge.
The bridge logs `Access was pre-authorized` and comes straight up.

Two things worth knowing:

- **Strip `&pr=iroh:` hints from the ticket when the two processes share a
  host.** The daemon mints a fresh iroh endpoint on every restart and nothing
  revises what was already recorded, so a pasted iroh address outlives the run
  that published it. Sync tries every recorded address, and each dead one costs
  a full timeout on every pass. A `pr=http:` hint pointing at the daemon's
  `eidetica.sync.http_listen` is what actually connects host-local.
- **`/agent share` also writes the ticket to `$XDG_CONFIG_HOME/chaz/shares/`.**
  Point `XDG_CONFIG_HOME` somewhere disposable if you do not want that.

## Maintenance: listing and leaving rooms

`chaz-matrix rooms` provides separate list and reset commands. `rooms list`
enumerates every room a login is joined to. `rooms leave-all` previews the same
set without changing membership; add `--execute` to leave every listed room:

```bash
chaz-matrix --config matrix-bridge.yaml rooms list
chaz-matrix --config matrix-bridge.yaml rooms leave-all            # preview
chaz-matrix --config matrix-bridge.yaml rooms leave-all --execute  # leave all
```

With a single `logins:` entry the login is implicit; with several, name one by
MXID with `--login` on either command:

```bash
chaz-matrix --config matrix-bridge.yaml rooms leave-all \
  --login @chaz:example --execute
```

`--retries` and `--delay-ms` tune only `leave-all`; listing has no leave policy
to tune.

Exit codes: `0` for a successful run, an empty room list, or a dry-run; `1`
when one or more rooms failed to leave; `2` for a setup failure — no or
ambiguous login, an unresolved password, a failed login, or an authenticated
account that does not match the configured username.

Safety notes:

- Each run signs in with a **fresh throwaway device** that is logged out at the
  end, so it never disturbs the bridge's own persisted session or device.
- **Pending invitations are untouched** — only rooms the account is already
  joined to are enumerated and left.
- Run it **with the bridge stopped** for a quiescent reset: a running bridge
  auto-joins allowed invitations it sees, so a room it re-joins mid-run is not
  part of the enumeration.

## Message Handling

- **DMs**: The bot responds to every message
- **Group rooms**: The bot responds to messages prefixed with `!chaz` or that mention the bot

To send a message with room context:

```text
!chaz summarize the discussion so far
```

To send without the `!chaz` prefix in a DM, just type normally.

## Commands

Commands are sent as Matrix messages. Session ops go through the same transport-neutral dispatch as the TUI — both bridges stay in sync. Most TUI slash commands have a `!chaz` equivalent; the table below covers the common surface. Extension-registered commands (e.g. `!chaz schedule`, `!chaz memory`) are auto-registered for whichever extensions are installed — see the relevant page for syntax.

### Session

| Command                  | Description                                          |
| ------------------------ | ---------------------------------------------------- |
| `!chaz sessions`         | List every session known to the registry             |
| `!chaz info`             | Show details for the session attached to this room   |
| `!chaz name [<alias>]`   | Set (or clear, with no arg) a human-friendly alias   |
| `!chaz attach <session>` | Bind this room to a specific session (name or DB ID) |
| `!chaz detach`           | Detach this room from its session                    |
| `!chaz channels`         | List Matrix rooms currently attached to this session |
| `!chaz share`            | Generate a shareable ticket URL for this session     |
| `!chaz unshare`          | Stop sharing the current session                     |
| `!chaz sync <ticket>`    | Sync a remote session via ticket URL                 |
| `!chaz compact`          | Summarize and compact conversation history           |
| `!chaz print`            | Print the current conversation context               |

### Living Agents

`!chaz agent <sub> [...]` — `add`, `remove`, `host`, `list`, `room`, `hosted`, `new`, `delete`, `share`, `unshare`, `import`, `set`, `invite`, `revoke-peer`, `rehost`, `home-status`. Mirrors `/agent ...` in the TUI; see [Agents](agents.md). `!chaz agents` lists the agents attached to this session. `!chaz pubkey` prints this peer's default pubkey (for `!chaz agent invite` from another peer).

### Sharing queue

`!chaz sharing [status | requests | approve <id> | reject <id>]` — inspect shared DBs and manage bootstrap requests across agent/bank/session DBs.

### Extensions

`!chaz extensions [list | add <name> [agent] | remove <name> [agent] | settings <name> | set <name> <key> <value>]` — per-session/per-agent extension control. See [Extensions](extensions.md).

### LLM config

| Command                                     | Description                                |
| ------------------------------------------- | ------------------------------------------ |
| `!chaz model [<model>]`                     | Show or set the model for this session     |
| `!chaz role [<name> [<prompt>]]`            | Show, select, or define a role             |
| `!chaz backend <name> <api_base> <api_key>` | Register a custom backend for this session |
| `!chaz backends`, `!chaz list`              | List known backends and models             |

### Approval & misc

| Command                        | Description                                                                 |
| ------------------------------ | --------------------------------------------------------------------------- |
| `!chaz approve` / `!chaz deny` | Decide the pending tool approval (or react to the notice with ✅ / ❌ / ⏭) |
| `!chaz send <msg>`             | One-shot message with no conversation context                               |
| `!chaz clear`                  | Ignore all messages before this point                                       |
| `!chaz rename`                 | Rename the Matrix room based on conversation content                        |
| `!chaz party`                  | 🎉                                                                          |

## Session Attachment

A Matrix room is connected to a session through an explicit _channel_ record (`room_id → session_db_id`). Joining a new room creates the session and attaches the room before an addressed message arrives.

Use `!chaz attach <session>` to rebind the room to a different session (e.g., to resume a synced session, or to route a scheduled-task session into a specific room). Multiple rooms can attach to the same session — responses fan out to every attached room. `!chaz detach` removes the binding; the next addressed message in the room creates a fresh session.

At bridge startup, the bot re-installs response-delivery callbacks for every persisted channel whose room it's joined to. This is what makes scheduled-task responses reach a Matrix room even when no user is currently active there.

## Per-Session Settings

Model, role, and backend selections live in the session's own eidetica database (under a `meta` DocStore), not on the room. That means a session's config travels with it across eidetica sync — sharing a session shares its name, agent, model, role, and backend reference.

## Session Persistence

Conversation history lives in per-session eidetica databases and survives bot restarts. The Matrix sync token is persisted by headjack, so the bot resumes from where it left off. Message batching prevents duplicate responses after a restart: messages received during the catch-up sync that were already processed are skipped.

## Retry Behavior

If the Matrix connection drops, the bot retries with a 5-second backoff. The retry loop handles transient network errors and homeserver restarts.

## Tool Approval

The bot surfaces approval requests as markdown notices in the room. Respond either via reactions (✅ approve · ❌ deny · ⏭ approve all) or by sending `!chaz approve` / `!chaz deny`. To skip approval altogether for specific low-risk tools, add them to `security.auto_approved_tools`.

## Encrypted Rooms

The bridge reads and answers encrypted rooms. There is nothing to configure: every
Matrix login keeps a persistent encryption device, and the bridge decides which other
devices it trusts from Matrix cross-signing alone.

| What                   | Behavior                                                                                                             |
| ---------------------- | -------------------------------------------------------------------------------------------------------------------- |
| Device store           | `{state_dir}/matrix/{login_id}/store/`, a passphrase-encrypted SQLite store holding the device's keys and room state |
| Store passphrase       | Generated on first start, kept in `{state_dir}/matrix/{login_id}/session` (mode `0600`, directory `0700`)            |
| Bot device             | Signed with the account's cross-signing identity on first start; the identity is created if the account has none     |
| Outbound room keys     | Shared only with devices their owner has cross-signed (MSC4153)                                                      |
| Inbound messages       | Decrypted only from cross-signed devices; anything else is logged and ignored                                        |
| Command authorization  | Unchanged: `allow_list`, approvals, and command rules apply exactly as in plaintext rooms                            |
| Pre-encryption session | Upgraded in place on first start: same device, new store                                                             |

### How encrypted-room trust works

A Matrix client that is signed in on several devices publishes a _cross-signing identity_
and signs each of its devices with it; Element does this when you verify a new login.
The bridge relies on that and nothing else. It never asks you to verify the bot, and it
never trusts a device just because it appeared in the room.

- **Your side.** Messages you send from a verified (cross-signed) device are decrypted
  and handled like any other message. A reply is encrypted to every cross-signed device
  in the room. A device you never verified gets no key for the reply, and its own messages
  are not decrypted. The bridge logs each such message:
  `Could not decrypt a message; it is ignored.` with its room, sender, and event id.
- **The bot's side.** On first start the bridge signs its own device. If the account has no
  cross-signing identity, the bridge creates one. It uses the configured password if the
  homeserver asks for re-authentication. An interrupted upload is retried on restart with
  the same keys; an existing published identity is never reset. If the account has an identity that was
  created elsewhere, for example by signing the bot account into Element, the bridge cannot
  sign itself. It logs a warning, and it can read encrypted rooms but cannot reply in them
  until you verify its device from that other session. Plaintext rooms are not affected.
- **Keys live on disk.** Moving or restoring a bridge means moving the `session` file and the
  `store/` directory together. The bridge refuses to start rather than silently becoming a new
  device when they do not match. This covers a missing store, a store it did not record, half
  of the store metadata, or a device key that differs from the recorded or published one.
  The error says which. A new device would lose every room key and would need your trust again.
- **A late key is not retried.** A message that arrives before its room key is logged as
  undecryptable and is not processed when the key turns up later. Send it again.

**Where encryption ends.** Matrix encryption protects the message between your device and
the bridge. Once the bridge accepts a message, the plaintext is written to the chaz session
database and handled like a message from a plaintext room: it syncs to the peers that host
the agent, and it is sent to the configured model backend. Neither the session database nor
the model provider is covered by Matrix encryption. An encrypted room keeps the homeserver
out of the conversation. It does not keep the conversation out of chaz or the model.

### Walkthrough: moving a conversation into an encrypted room

1. Upgrade `chaz-matrix` and restart it. A bridge deployed before encryption support upgrades
   its existing device and creates the bot's cross-signing identity:

   ```text
   INFO chaz_matrix_bridge::bridge::client: Previous session found in '/var/lib/chaz-matrix/matrix/_chaz_example/session'
   INFO chaz_matrix_bridge::bridge::client: Upgrading pre-encryption Matrix session with a persistent crypto store
   INFO chaz_matrix_bridge::bridge::client: Restoring session for @chaz:example…
   INFO chaz_matrix_bridge::bridge::client: Creating a cross-signing identity for @chaz:example
   INFO chaz_matrix_bridge::bridge: The client is ready! Listening to new messages…
   ```

2. From a verified Element session, start an encrypted direct message with the bot. It joins,
   and `!chaz help` or a plain message is answered in the room like anywhere else.
3. Restart the bridge. The upgrade and identity lines do not appear again. The device and its
   keys are the same, so the room keeps working without re-verification.
4. Failure path: send a message from a device you never verified, such as a new login you
   skipped verification for. The bot does not answer, and its log names the message:

   ```text
   WARN matrix_sdk_crypto::machine: Failed to decrypt a room event: decryption failed because trust requirement not satisfied: The sending device was not signed by the user's identity
   WARN chaz_matrix_bridge::bridge: Could not decrypt a message; it is ignored. The sender's device must be cross-signed and must share its room key with this device room_id=!room:example sender=@you:example event_id=$event session_id="…"
   ```

   Verify that device from another of your sessions, then send the message again.

## Limitations

- **Text only.** The Matrix bridge currently ingests only text messages. Image, file, and other non-text Matrix events are skipped on both the live path and during history backfill. Multimodal models will not see attached images sent in the room. Restoring multimodal ingestion is tracked as a TODO in `crates/matrix-bridge/src/bridge/commands.rs` and `crates/matrix-bridge/src/bridge/history.rs`.

## Key maintenance: one store, one owner

Stop the bridge before using `chaz-matrix keys`. These commands use the configured
Matrix login and its encrypted store; they do not open chaz's database, call a
model, or accept commands from a room. A process lock rejects a second bridge or
maintenance writer. With several logins, put `--login <login-id>` immediately
following `keys`.

| Command                                                                    | Purpose                                                                                                                      |
| -------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `keys status`                                                              | Public device/master fingerprints, private-secret availability, recovery and own-device status                               |
| `keys init --account <MXID>`                                               | Create an identity only when no identity is published; requires typing `INIT <MXID>`                                         |
| `keys reset --account <MXID>`                                              | Replace this account's identity explicitly; requires typing `RESET <MXID>` and, for password UIAA, `AUTHORIZE <MXID>`        |
| `keys resume`                                                              | Resolve a pending reset against the actual published identity; never generates new keys                                      |
| `keys resume --authorize`                                                  | Explicitly re-authorize uploading the **same** pending identity when the server still has the previous identity              |
| `keys abort`                                                               | Archive a positively cancelled/rejected or never-generated transition; refuses an ambiguous or committed outcome             |
| `keys recovery-setup --output <file>`                                      | Save a new private recovery passphrase, enable standard Matrix secret storage and encrypted room-key backup                  |
| `keys recovery-setup --output <new-file> --replace-existing`               | Reconfigure recovery after typing `REPLACE RECOVERY <MXID>`; retains the existing encrypted backup                           |
| `keys recovery-restore --key-file <file>`                                  | Restore the same published identity secrets and standard backup keys; accepts a standard recovery key or recovery passphrase |
| `keys verify --user <own-MXID> --device <device-id> [--timeout <seconds>]` | Interactive SAS verification of exactly one other device of the configured account                                           |

A missing session is first provisioned as a new persistent device. The command
reports `New Matrix device saved. Run the maintenance command again against this
persisted login.` and exits; repeat the intended command. This separates first
login from a staged snapshot. `status` does not initialize or reset cross-signing.
Configured passwords (including `${ENV}` references) supply password UIAA. Other
homeserver authentication flows are refused without printing their parameters or
credentials. Any failed operation exits nonzero; do not interpret a diagnostic as
a committed transition.

### Identity replacement is a transaction, not SDK cancellation

Reset changes which identity other clients must trust. It is never a startup
repair. The SDK writes replacement private keys before password authorization,
so cancelling its UIAA handle alone is not a rollback.

Maintenance copies the encrypted SQLite store while no client has it open,
records a durable pending journal, and modifies only the staged generation. It
commits by atomically changing the saved session's store pointer **only** when a
fresh server query matches the staged private master fingerprint. The device
fingerprint stays the same. Previous encrypted generations and archived journals
remain in the login's private state directory; they are not deleted automatically.
The session file carries the store passphrase and must stay with these stores.

After interruption, the bridge refuses to open a pending transition. `resume`
can complete a server-committed transition, including retrying the device
signature. If the server still has the previous identity after an uncertain
network outcome, both generations remain pending: this is not proof of rejection.
Use `resume --authorize` to authorize the same replacement, or wait and query
again. `abort` accepts a positively cancelled/rejected operation whose server identity
is still unchanged. It also handles interruption before generation: the recorded
private baseline must still match the staged store and the server must still
have the old identity. Missing baseline evidence cannot authorize this abort. An identity matching neither endpoint requires
explicit account recovery; keep both stores and the journal intact.

### Recovery saves keys, not authenticated history

Recovery is standard Matrix secret storage plus encrypted homeserver-side key
backup. The SDK's standard recovery-passphrase support protects identity secrets
and the backup key. A random, high-entropy passphrase is fsynced to your selected
file **before** setup contacts the server, so a process death cannot lose the only
recovery secret. The file contains the passphrase itself, usable in other Matrix
clients' recovery-passphrase flow. Restore also accepts standard encoded recovery
keys produced by another client.

The destination directory must already be private (`0700`); files are created
with `0600`. Existing files, symlinks (including parent symlinks) and public
output directories are refused. There is no overwrite option. Keys/passphrases
are never accepted on the command line or printed, and maintenance suppresses SDK
tracing even with `RUST_LOG=trace`. Save the recovery file separately from the
bridge state, offline if appropriate. Keep earlier recovery files until you have
verified the replacement in a fresh store. An interrupted setup retains its
output file; inspect status and attempt restore with it before replacing recovery.
Reconfiguration reuses the existing backup and never deletes it. If its private
backup key is unavailable, restore it first; the command refuses replacement.
Retiring a confirmed pre-cutover backup is a separate operator cutover action,
not permission for maintenance to delete post-cutover keys.

Restoring into a fresh login recovers the **same** identity, signs that device,
and downloads/imports actual standard backup keys. It does not replay a room's
history into chaz. Imported backup keys lack original sender provenance in this
SDK; the bridge's incoming CrossSigned requirement still rejects such events.
There is no untrusted-history viewer or production fallback. Pre-cutover Matrix
history can be retired separately; these commands never delete chaz session or
agent databases. New post-cutover keys are retained. Accepted **new** messages
still become plaintext in chaz's session database and configured model, as
explained in [Where encryption ends](#encrypted-rooms).

### Walkthrough: recover the only login, then verify another device

1. Stop the bridge and inspect `chaz-matrix --config matrix-bridge.yaml keys status`.
   For a new login repeat the command, then initialize explicitly:

   ```bash
   chaz-matrix --config matrix-bridge.yaml keys init --account @chaz:example
   ```

   ```text
   Type INIT @chaz:example to continue; anything else cancels:
   Identity transition committed; previous encrypted store retained.
   ```

   If an identity already exists, init refuses it. Recover its secrets instead;
   only use reset after deliberately deciding to replace that account's identity.

2. Create a private destination and enable recovery:

   ```bash
   mkdir -m 700 recovery
   chaz-matrix --config matrix-bridge.yaml keys recovery-setup --output recovery/account.key
   ```

   ```text
   Recovery enabled; private recovery passphrase saved. Standard encrypted backup uploaded.
   ```

3. For a recovery drill, use an isolated bridge config with the same Matrix
   account and a **new** state directory. Do not delete the original. With both
   bridges stopped, provision the fresh login using `keys status`, then restore:

   ```bash
   chaz-matrix --config fresh-bridge.yaml keys recovery-restore --key-file recovery/account.key
   ```

   ```text
   Same identity restored; standard encrypted room keys imported. Imported history is NOT authenticated bridge/model input.
   ```

   Compare the published/private master fingerprints in `keys status` with the
   original. The fresh device has its own device fingerprint, stable on restart.
   Failure/recovery: a wrong file reports `recovery failed; incorrect secret,
incomplete recovery data or unreachable homeserver; identity was not reset`.
   Supply the correct private file and retry; never reset as a recovery fallback.

4. Start SAS with another device signed into **the bot's own account**, not a
   room participant's account:

   ```bash
   chaz-matrix --config matrix-bridge.yaml keys verify --user @chaz:example --device OTHERDEVICE
   ```

   ```text
   SAS account: @chaz:example
   SAS device: OTHERDEVICE
   SAS transaction: <transaction>
   Compare SAS decimals on BOTH devices: <three numbers>
   Type MATCH @chaz:example OTHERDEVICE <transaction> only if they match; type mismatch or cancel otherwise:
   ```

   Compare both screens and type the full displayed `MATCH` line only when they
   agree. The other device must also confirm. A bare `yes` or `match`, a different
   device/transaction, mismatch, peer rejection/cancellation, EOF or timeout
   never counts as success. On success: `SAS verified selected own-account device
after explicit match.` Restart the bridge after maintenance exits. No live
   control socket or automatic verification approval exists.

5. Interruption recovery: after an authorized reset loses its network response,
   leave the bridge stopped and run `keys resume`. A confirmed commit reports
   `Identity transition committed; previous encrypted store retained.` An
   ambiguous old-server result explicitly says both stores are retained; use
   `keys resume --authorize` only if you still intend that same replacement.
