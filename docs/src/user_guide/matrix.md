# Matrix Bot

Chaz connects to Matrix as a bot. It records eligible room messages in a
session. An invoked Matrix turn normally posts its final to that room; `no_reply({})` ends the turn without a post, and `matrix__send` remains available for explicit or proactive messages. The Matrix bridge runs as its own process (`chaz-matrix`), separate from
the `chaz` daemon — read [Transport Bridges](bridges.md) first for the
architecture, the connection settings, and the one-time approval flow. This page covers Matrix-specific
configuration and behavior.

## Live typing during a turn

| Signal                    | Starts                                                                | Ends                                                    |
| ------------------------- | --------------------------------------------------------------------- | ------------------------------------------------------- |
| Matrix room typing notice | The bridge sees a fresh executor per-turn start in the shared session | Completion, error, or a heartbeat older than 45 seconds |

The bridge only observes the per-turn claim; it does not run the agent or infer
activity from a room binding, an old acknowledgement, or session runtime
ownership. Session writes refresh the activity evidence; the renewal timer
ages that evidence locally without rereading unchanged activity tables.
Typing is renewed every three seconds while the claim remains
fresh, with a ten-second server timeout, avoiding gaps if a renewal is delayed. On release it sends
a cancellation. A missing executor heartbeat times out instead of showing
permanent typing after a crash; network or sync lag can delay observation.
Discord does not publish this signal.

Example with a shared room:

1. Send `@chaz:example summarize this` in the room. When the executor starts,
   the Matrix client displays `chaz is typing` while the turn runs.
2. On a final or error, the typing indicator stops even if
   the bridge remains connected to the room. A second message starts a new turn.
3. If the executor crashes, the typing notice expires and the bridge stops
   renewing it when the last heartbeat ages out (within 45 seconds plus sync
   and renewal-timer delay). The turn is interrupted; inspect `/interrupted` from a
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
       # workspace_manager: "@you:example" # optional; enables the personal workspace

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
   fallbacks at the top level. `workspace_manager` is per-login only; see
   [Personal workspace](#personal-workspace-space-lobby-and-named-chats).

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

### True reset: `--reset`

Plain `leave-all` leaves joined rooms but keeps pending invitations and access
to left-room history. Add `--reset` to also reject pending invites and forget
each successfully left or rejected room. Like plain `leave-all`, it previews
without changing membership unless you pass `--execute`.

| Flag           | Effect                                                                                                       |
| -------------- | ------------------------------------------------------------------------------------------------------------ |
| `--reset`      | Include pending invites in the preview; with `--execute`, reject them and forget each room after leaving it. |
| `--execute`    | Perform the previewed membership changes, plus forgetting when `--reset` is set.                             |
| `--retries N`  | Attempt each leave or forget up to N times (default 3, range 1–10).                                          |
| `--delay-ms N` | Wait N milliseconds between rooms (default 250, range 100–60000).                                            |

Leaving an invited room rejects its invite. Forgetting a left room drops this
account's server-side history access; it does not erase other users' copies,
local history, or the bridge's Chaz sessions. The command handles only rooms
joined or invited at enumeration time, not rooms left by an earlier run. A new
invite can still join the account to the room after the bridge restarts.

```bash
chaz-matrix --config matrix-bridge.yaml rooms leave-all --reset            # preview
chaz-matrix --config matrix-bridge.yaml rooms leave-all --execute --reset  # true reset
```

**Stop the bridge before a reset and leave it stopped until the run exits.**
The steady-state bridge installs an invite auto-joiner at startup and joins
every allowed invite it sees. A reset raced against a live bridge can be
silently undone: an invite that arrives mid-run, or is still pending when the
bridge next starts, puts the account right back in the room that was just
left. There is no protocol way for the maintenance command to prevent this —
the only fix is ordering.

A forget that ultimately fails is reported as a warning and does not fail the
run: forget behavior varies by server — those that already forget on leave
(the `m.forget_forced_upon_leave` capability) may reject the redundant forget,
and forget support outside Synapse is unverified. The exit code reflects the
membership operations only.

#### Walkthrough: resetting a bot account

1. Stop the `chaz-matrix` bridge process so its auto-joiner cannot race the
   reset.
2. Preview. Nothing changes, and invites are listed before joined rooms:

   ```text
   $ chaz-matrix --config matrix-bridge.yaml rooms leave-all --reset
   config: matrix-bridge.yaml
   login: @chaz:example
   agent: chaz
   account: @chaz:example
   homeserver: https://matrix.example
   mode: dry-run (reset)
   pending invite !invited:example
   joined !general:example
   joined !ops:example
   1 pending invite(s) to reject, 2 joined room(s) to leave and forget; nothing was changed (dry-run)
   re-run `rooms leave-all --execute --reset` for a true reset (stop the bridge first)
   ```

3. Execute. Each invite is rejected, then each joined room is left, and every
   successful leave is followed by a forget. Here one leave keeps failing after
   all its retries, and the server refuses one forget:

   ```text
   $ chaz-matrix --config matrix-bridge.yaml rooms leave-all --reset --execute
   ...
   mode: execute (reset: reject invites, forget after leave) (retries=3, delay=250ms)
   rejected invite !invited:example
   forgot !invited:example
   left !general:example
   warning: forget failed for !general:example (3 attempts): ...
   FAILED !ops:example (3 attempts): ...
   summary: rejected 1, left 1, failed 1, forgot 1, forget-failed 1
   $ echo $?
   1
   ```

4. Recover by re-running the same command. The new run enumerates only what is
   still joined or invited, so `!ops:example` is retried and the rooms already
   handled are not touched again. A room whose forget failed is already left and
   does not reappear, so this command will not retry its forget; its history
   stays readable to the account until it is forgotten some other way.
5. Check the summary and any forget warnings before restarting the bridge.
   Exit `0` means membership changes succeeded, not that every forget succeeded.

Exit codes: `0` for a successful run, an empty room list, or a dry-run; `1`
when one or more rooms failed to leave or invites failed to reject; `2` for a
setup failure — no or ambiguous login, an unresolved password, a failed login,
an authenticated account that does not match the configured username, or (with
`--reset`) a failed invite enumeration.

Safety notes:

- Each run signs in with a **fresh throwaway device** that is logged out at the
  end, so it never disturbs the bridge's own persisted session or device. The
  login path is deliberately crypto-free — no keys are uploaded — which keeps
  the ephemeral device invisible to other clients.
- Every run costs one login from each of Synapse's two login rate-limit
  buckets (0.003/s, burst 5): fine at human or cron cadence, exhaustible by
  tight retry loops around the command. `--reset` roughly doubles the per-room
  API calls (leave plus forget).
- **Pending invitations are untouched by plain `leave-all`** — only rooms the
  account is already joined to are enumerated and left. `--reset` rejects them
  instead.
- Run it **with the bridge stopped** for a quiescent reset: a running bridge
  auto-joins allowed invitations it sees, so a room it re-joins mid-run is not
  part of the enumeration.

## Message Handling

- **DMs**: The bot responds to every message
- **Group rooms**: By default, the bot responds to `!chaz` commands or messages that mention it. An explicitly enabled room can invite participation on every eligible text message.

To send a message with room context:

```text
!chaz summarize the discussion so far
```

To send without the `!chaz` prefix in a DM, just type normally.

## Commands

Commands are sent as Matrix messages. Session ops go through the same transport-neutral dispatch as the TUI — both bridges stay in sync. Most TUI slash commands have a `!chaz` equivalent; the table below covers the common surface. Extension-registered commands (e.g. `!chaz schedule`, `!chaz memory`) are auto-registered for whichever extensions are installed — see the relevant page for syntax.

### Session

| Command                          | Description                                                                                  |
| -------------------------------- | -------------------------------------------------------------------------------------------- |
| `!chaz sessions`                 | List every session known to the registry                                                     |
| `!chaz info`                     | Show details for the session attached to this room                                           |
| `!chaz name [<alias>]`           | Set (or clear, with no arg) a human-friendly alias                                           |
| `!chaz description [<text>]`     | Set (or clear, with no arg) a browse-only [session description](tui.md#session-descriptions) |
| `!chaz attach <session>`         | Bind a free room to a free session (name or DB ID); exact retries are safe                   |
| `!chaz detach [<session-db-id>]` | Remove this room's binding; an ambiguous room requires the exact session DB ID               |
| `!chaz participation on`         | Enable agent-selected replies for this group room after executor confirmation                |
| `!chaz participation status`     | Report whether this login and room are confirmed enabled                                     |
| `!chaz participation off`        | Disable participation in this room                                                           |
| `!chaz channels`                 | List transport channels attached to this session                                             |
| `!chaz share`                    | Generate a shareable ticket URL for this session                                             |
| `!chaz unshare`                  | Stop sharing the current session                                                             |
| `!chaz sync <ticket>`            | Sync a remote session via ticket URL                                                         |
| `!chaz compact`                  | Summarize and compact conversation history                                                   |
| `!chaz context`                  | Context-window usage for this room's agent                                                   |
| `!chaz print`                    | Print the current conversation context                                                       |
| `!chaz stop`                     | Stop the turn running in this session ([details](tui.md#stopping-a-running-turn))            |

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

| Command                        | Description                                                                   |
| ------------------------------ | ----------------------------------------------------------------------------- |
| `!chaz approve` / `!chaz deny` | Decide the pending tool approval (or react to the notice with ✅ / ❌ / ⏭)   |
| `!chaz send <msg>`             | One-shot message with no conversation context                                 |
| `!chaz clear`                  | Ignore all messages before this point                                         |
| `!chaz rename`                 | Rename the Matrix room based on conversation content (not in workspace chats) |
| `!chaz party`                  | 🎉                                                                            |

### Workspace

Only the login's configured `workspace_manager` may run these; see
[Personal workspace](#personal-workspace-space-lobby-and-named-chats).

| Command                           | Where                    | Description                                                              |
| --------------------------------- | ------------------------ | ------------------------------------------------------------------------ |
| `!chaz workspace help`            | anywhere                 | Show the workspace vocabulary                                            |
| `!chaz workspace setup`           | direct chat with the bot | Preview the Space and Lobby, with room version and creator powers        |
| `!chaz workspace confirm <op>`    | direct chat with the bot | Create the previewed Space and Lobby                                     |
| `!chaz workspace status [<op>]`   | Lobby or direct chat     | Workspace summary, or the exact progress of one operation                |
| `!chaz workspace retry <op>`      | Lobby or direct chat     | Continue an incomplete operation, or re-check an uncertain room creation |
| `!chaz workspace cancel <op>`     | Lobby or direct chat     | Stop an unfinished operation; deletes nothing                            |
| `!chaz chat new <name>`           | Lobby                    | Create a fresh named chat room with its own new session                  |
| `!chaz chat list`                 | Lobby                    | List this workspace's chats with their handles (`c1`, `c2`, …) and links |
| `!chaz chat rename <chat> <name>` | Lobby                    | Rename a chat by handle; the room and session stay the same              |
| `!chaz chat rename <name>`        | inside a workspace chat  | Rename this chat                                                         |

## Attached rooms and reply routing

A session has **at most one external attachment**, identified by exact
`(transport, login, channel)`. Each such channel belongs to at most one session
in the owning agent's exposed registry. TUI and CLI are local clients, not extra
attachments; selecting a session there does not change its Matrix room.

`!chaz attach <session>` refuses if this room already belongs to a different
session: detach first. It also refuses if the target session already has a
different attachment, including another transport. Repeating an attach to the
same session and room is safe. Attach does not copy history, replay local
finals, or redirect queued sends from another room.

Legacy conflicts fail closed in both directions: neither multiple rooms on one
session nor multiple sessions on one room gets an arbitrary winner. Inbound
lookup does not create a replacement session, and delivery and approval posts
stop until the binding is unambiguous. There is no automatic split or migration.

Use `!chaz detach` **in the room being removed**. It removes only that exact
login/room binding and stops its delivery and approval observers without a
bridge restart. Any remaining binding retains the original session, history
and required login exposure. The detached room's next addressed message can
create a fresh session. If the room itself belongs to multiple sessions, the
error lists their DB IDs: use `!chaz detach <exact-session-db-id>` to remove
one explicitly. Names and guessed IDs cannot disambiguate it. Errors are
reported rather than claiming detach succeeded.

| Action                                     | Effect                                                                                                       |
| ------------------------------------------ | ------------------------------------------------------------------------------------------------------------ |
| Addressed Matrix text in a DM or group     | Stored with login, room and sender provenance; wakes the session's agent                                     |
| Unaddressed text in an attached group room | Stored as observation for later context; **no model turn**                                                   |
| `!chaz participation on`                   | Authorized, executor-confirmed ambient wake for this room; off by default                                    |
| `!chaz participation off` / `status`       | Stop ambient wake / show confirmed state; messages remain observable                                         |
| Normal final on a Matrix-origin turn       | Saved locally and queued to the same bound room under the owning agent's login                               |
| Normal final on a TUI/local/schedule turn  | Saved locally only, even in a Matrix-bound session                                                           |
| `no_reply({})` in a Matrix-bound session   | Terminal action: no final message and no room post                                                           |
| `matrix__send({"body":"text"})`            | Explicit/proactive room send; on a Matrix-origin turn it counts as the reply and the later final stays local |

The daemon commits a Matrix-origin final, its addressed generic `BridgeEvent` outbound row,
and its completed turn attempt atomically. The bridge delivers only addressed
outbound events whose single destination still matches its own login and
the session binding. A missing binding, mismatched identity, or conflicting
legacy binding fails closed. No other session or joined room can be targeted.
An acknowledged send appears as
**sent** in later context and the TUI; until confirmation it remains **pending**.
The bridge retries pending chunks after an outage with stable Matrix
transaction IDs; it can repeat a request after a lost acknowledgement, so the
homeserver's transaction-ID deduplication matters. Detaching before delivery
prevents an old queued send from being redirected.

Incoming source and outbound status come from entry metadata, not text in the
message body. The local copy of an auto-posted final carries its paired send
identity and appears as a `matrix_reply` with pending/sent status in later
model context; a TUI/local final still appears as a `local_message`. A message
that imitates a provenance header remains an untrusted body. A Matrix mention
to another bot is received by that bot's own bridge and
session; the send event itself does not create a second local agent wake. Only
a normal **local** agent message that `@mentions` an attached agent invites
that agent to produce a local final; burst limits still apply. The model-facing
`no_reply` action is available on every turn of a Matrix-bound session, including
local turns, to keep the tool list stable; non-Matrix-origin finals never auto-post.
A model without tool-calling support cannot safely choose `no_reply` and is
rejected for a Matrix-bound turn rather than silently posting a fallback answer.

### Recovering a legacy binding

1. If a session was bound to two rooms, choose which room to keep. In the
   other room, send `!chaz detach`:

   ```text
   Room detached. Future messages will create a fresh session.
   ```

   The remaining room still uses the original session and transcript. Nothing
   is copied to the removed room, and pending output to it stops in-process.

2. If a single room instead belongs to two sessions, bare detach refuses. The
   response names the exact IDs (shown here as `<A>` and `<B>`):

   ```text
   !chaz Error: channel is bound to multiple sessions: <A>, <B>; use !chaz detach <exact-session-db-id> in this room
   ```

   Send `!chaz detach <A>` in that room, substituting the complete unwanted
   session DB ID. Session `<B>` and its history remain untouched. A wrong ID
   gives `!chaz Error: session <id> is not bound to this room on this login`;
   it does not remove another binding.

3. Once the remaining mapping is unique, detach reinstalls any delivery
   observers that startup refused. Queued output resumes to its original room
   without a new inbound message or a restart. Attaching a different session
   is a separate explicit action, not a move or history merge.

## Migration: schedules no longer auto-post finals

Scheduled turns no longer automatically post their final text to Matrix, even
when their session is attached to a room. Their normal final is saved locally.
Matrix-origin conversational turns still auto-post normal finals as described
above; a scheduled wake is not a Matrix-origin turn.

To publish scheduled work, use a **Pinned** schedule targeting the session
attached to the intended Matrix login and room, and instruct its owning agent
to call `matrix__send({"body":"…"})`. A Fresh schedule has no attached room.
The explicit send is the room post; the scheduled turn's later normal final
stays local and does not produce a duplicate. See [Agent-owned schedules](agents.md#schedules)
for creation and home-daemon requirements.

1. In the attached session, create a pinned morning briefing:

   ```text
   /schedule add brief 0 0 9 * * Mon-Fri chaz Summarize overnight activity. Publish the briefing with matrix__send in this attached session; keep your final local.
   ```

   When it fires, the agent calls, for example:

   ```json
   { "body": "Morning briefing: no urgent changes." }
   ```

   through `matrix__send`. The room receives that explicit post once, not the
   later local final.

2. If an existing schedule only says “summarize overnight activity”, its final
   now appears in the session, **not** in Matrix. Replace its task with an
   explicit-send instruction (remove and re-add the rule if using commands).
3. If the pinned session is not attached, `matrix__send` is unavailable; a
   normal final cannot substitute for it. Attach the intended room with
   `!chaz attach <session>` before the next fire. If another room already owns
   the binding, attach refuses it; detach the old room explicitly before moving
   it. Never change a schedule to Fresh as a workaround for a missing binding.

## Choosing when to wake in a group

Group ambient participation is **off by default**. Only a sender matching
this bridge login's explicit nonempty `allow_list` can set
`!chaz participation on`. The bridge waits for the executor to acknowledge a
fresh setting for this exact login and room; an old participation policy from
before the session-local send contract is deliberately inert until explicitly
re-enabled. `status` and `off` require the same allowlist. Commands, own-bot
messages, pre-join history, duplicate events and unauthorized senders do not
wake the agent. The per-sender rate limit and the pilot ceiling of 20 ambient
turns per room per bridge process apply; excess eligible messages are still
observations, not extra turns. DMs remain addressed by default.

Example in an allowlisted group room:

1. `@you:example` sends `!chaz participation status` and sees
   `Matrix participation: off`. An unaddressed message is recorded but does
   not spend a model call. A mention of `@chaz:example` does start a turn;
   its ordinary final appears in both the session and the Matrix room.
2. If the agent instead calls `matrix__send({"body":"The answer is 42"})`,
   the room receives that explicit post from its Matrix login, while the
   session shows the addressed event as pending, then sent. A later normal
   final on that turn remains **local** and does not produce a second room post.
3. A room already bound to another session tries `!chaz attach <session>` and
   receives `!chaz Error: failed to bind room: room is already bound to session <room-session-db-id>; detach first`.
   Detach this room first if replacing its binding is intentional. If the
   target session still has another attachment, attach also refuses with
   `session has a different or conflicting external attachment; detach or migrate explicitly`.
   Detach the old room explicitly before attaching the free target. An old
   queued send is not rerouted to the new room.
4. `@you:example` sends `!chaz participation on` and sees
   `Matrix participation: on` only after executor confirmation. A bare
   message now wakes the model. A normal final replies in the room, while
   `no_reply({})` as a sole terminal action leaves it silent. If the executor is unavailable,
   the response is
   `!chaz Error: executor did not confirm participation; mode is off`;
   restore it and explicitly enable again.
5. `!chaz participation off` reports `Matrix participation: off`. Bare
   messages return to observation-only; mentions still start turns.

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

| What                   | Behavior                                                                                                         |
| ---------------------- | ---------------------------------------------------------------------------------------------------------------- |
| Device store           | `{login dir}/store/`, a passphrase-encrypted SQLite store holding the device's keys and room state               |
| Store passphrase       | Generated on first start, kept in `{login dir}/session` (mode `0600`, directory `0700`)                          |
| Bot device             | Signed with the account's cross-signing identity on first start; the identity is created if the account has none |
| Outbound room keys     | Shared only with devices their owner has cross-signed (MSC4153)                                                  |
| Inbound messages       | Decrypted only from cross-signed devices; anything else is logged and ignored                                    |
| Command authorization  | Unchanged: `allow_list`, approvals, and command rules apply exactly as in plaintext rooms                        |
| Pre-encryption session | Upgraded in place on first start: same device, new store                                                         |

`{login dir}` is the login's own directory under `{state_dir}/matrix/`; see
[Per-login state directories](#per-login-state-directories).

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

## Personal workspace (Space, Lobby and named chats)

A workspace gives one person a tidy place to keep separate conversations with
the bot: a private Matrix **Space** that lists a **Lobby** and one room per
chat. Every chat room has exactly one new Chaz session, so chats never share
a transcript. They still use the owning agent's shared tools and memory;
separate transcripts do not create separate agents or authorization domains.
Turn it on by setting `workspace_manager` on the login to your exact MXID;
unset, management commands refuse (help remains available).

**A Space is a folder of rooms, not a conversation and not a permission
group.** What each part does:

| Room      | You use it for                                  | Chaz context                        |
| --------- | ----------------------------------------------- | ----------------------------------- |
| Space     | Finding the Lobby and your chats in your client | None; the bot never replies there   |
| Lobby     | `!chaz chat …` and `!chaz workspace …` commands | None; it is command-only, no agent  |
| Each chat | Talking to the agent, like any direct chat      | Exactly one session, created for it |

What is automatic and what you do:

- **Only you manage it.** A command counts only when its sender is exactly the
  configured `workspace_manager` and passes the login's `allow_list`. Being in
  the Space or Lobby, or quoting a command, grants nothing.
- **Only new chats appear.** `chat new` creates a fresh room and a fresh
  session. Existing rooms, sessions, internal sessions and the agent catalog
  are never listed, imported or moved, and `chat list` shows only this
  workspace's chats. An old workspace chat is resumed by opening its link.
- **Privacy is explicit.** The bot creates every room invite-only, with
  history visible to joined members only, no guest access and no room
  directory entry. The Lobby and chats are encrypted from creation. Only you
  are invited, and only once a chat is ready; Space membership never invites
  anyone or grants access. Room names, topics, Space links and the
  `dev.chaz.workspace` marker described below are room state, which Matrix does
  not end-to-end encrypt: do not put secrets in chat names.
- **The bot creates the rooms.** You get power level 100 in each and are not a
  creator. `setup` shows the homeserver's room version before anything is
  created: from room version 12 on, a creator's power is permanent and cannot
  be revoked, so the bot keeps it. An unrecognised room version refuses setup.
- **Names are literal labels.** `chat rename` sets the room name and the
  session's [description](tui.md#session-descriptions) to exactly what you
  typed; no model writes titles. The model-based `!chaz rename` is refused in
  workspace chats. Rooms and sessions are identified by ID, never by name.
- **Bindings are fixed.** `attach`, `detach` and `participation` are refused
  in workspace chats, and the bot never creates a replacement session for a
  chat whose session it cannot open. Literal rename and approval reactions also
  refuse a missing or competing binding. Replies and tool approvals use the
  ordinary [reply routing](#attached-rooms-and-reply-routing) for that room
  only; nothing is ever posted to the Lobby or another chat instead.

**Where the state lives.** The workspace record — the Space, Lobby and chat
room IDs, their session IDs and every operation's progress — is in the bridge
peer's local `chaz_peer` database, which never syncs. Each room also carries a
`dev.chaz.workspace` state event naming its workspace, operation and role; the
bridge uses it to keep such rooms away from ordinary history bootstrap even
when the record does not list them.

**Failure modes.** Room creation cannot be made exactly-once, so every request
is an operation (`op-N`) with saved step progress:

- If a room-creation request has an unknown outcome (a dropped connection or a
  server error), the operation becomes **uncertain**. The request is never sent
  again. `workspace retry` looks for a room the bot created with that
  operation's marker and continues with it; if none, or more than one, is
  visible it stays uncertain. Use `workspace cancel` only if you are sure no
  room was created.
- If a later step fails (session setup, a Space link, an invitation), the
  operation is **incomplete** and its report names what is done and pending.
  `workspace retry` runs only the pending steps. A chat whose session is not
  ready replies "not ready" and sends nothing to the agent.
- A session database created before an interruption is found again through a
  marker in its initial settings. The bridge also finishes any interrupted
  schema/catalog initialization before exposing it. If a DB-creation attempt
  was admitted but no marked DB is visible, retry refuses a replacement: retain
  the operation and investigate storage/sync rather than creating another DB.
- One operation runs at a time; a new request waits until the unfinished one
  is retried or cancelled. Repeated delivery of the same command returns the
  same report. Bridge restarts keep everything; nothing resumes on its own.
- Cancel never deletes or leaves a room. A room an operation already created
  stays as it is, marked "not ready" in `chat list`, and setup cannot be
  cancelled once it created a room — retry it instead. The same holds for a
  chat with a known room or a rename whose Matrix name write was attempted:
  a lost response may hide a successful write. Retry the same literal rename
  to finish both labels rather than cancelling it.

V1 has no archive, threaded Inbox, existing-room adoption, nested Spaces or
model-driven management.

### Walkthrough: a workspace with one interrupted chat

1. Set `workspace_manager: "@you:example"` on the bot's login and restart the
   bridge. In your existing direct chat with the bot, ask for a preview:

   ```text
   > !chaz workspace setup
   Proposed workspace (operation op-1):
   - Bot @chaz:example on login @chaz:example, owning agent chaz (<agent DB ID>), manager @you:example.
   - A private Space “Chaz” and a command-only “Lobby” room.
   - Lobby and chats: invite-only, encrypted, history visible to joined members only, no guests, not in the room directory. Only @you:example is invited.
   - The Space is invite-only and unlisted; its names and links are room state, which Matrix does not end-to-end encrypt.
   - Room version 12: the bot is the room creator with permanent, unlimited power that no one can revoke. @you:example gets power 100 in every room and is not a creator.
   - No existing sessions or rooms are listed, moved or imported.
   Reply `!chaz workspace confirm op-1` to create these rooms.
   ```

2. Confirm it, then accept both invitations in your client:

   ```text
   > !chaz workspace confirm op-1
   op-1: workspace setup — done.
   Done: Space created, Lobby created, listed in the Space, linked back to the Space, invited to the Space, invited to the Lobby.
   Space: https://matrix.to/#/!space:example
   Lobby: https://matrix.to/#/!lobby:example
   Accept both invitations; create chats from the Lobby.
   ```

3. In the Lobby, start a chat. Accept its invitation and talk there:

   ```text
   > !chaz chat new Bike repair
   op-2: new chat “Bike repair” (c1) — done.
   Done: room created, session created and bound, session label set, reply delivery installed, listed in the Space, linked back to the Space, invitation sent.
   Open chat: https://matrix.to/#/!bike:example
   A new session is ready; no other chat history was copied.
   ```

4. Failure and recovery. This example drops the room-creation response, then
   simulates a storage failure after creating the session DB:

   ```text
   > !chaz chat new Trip plans
   op-3: new chat “Trip plans” (c2) — uncertain.
   Pending: room creation (outcome unknown), session created and bound, session label set, reply delivery installed, listed in the Space, linked back to the Space, invitation sent.
   Last error: room.create: outcome unknown (connection reset); it will not be sent again
   !chaz workspace retry op-3 checks again for the room. If you are sure no room was created, !chaz workspace cancel op-3 stops it.

   > !chaz chat new Taxes
   !chaz Error: op-3 is unfinished (uncertain); use !chaz workspace retry op-3 or cancel op-3 first

   > !chaz workspace retry op-3
   op-3: new chat “Trip plans” (c2) — incomplete.
   Done: room created.
   Pending: session created and bound, session label set, reply delivery installed, listed in the Space, linked back to the Space, invitation sent.
   Last error: session.provision: injected failure after DB creation
   Room !trip:example exists; it is not ready for chat. No second room will be created.
   Continue with !chaz workspace retry op-3 (only pending steps run).
   ```

   The retry found the room the lost request had created instead of creating
   another. After storage access is restored, retry again; only the pending steps run:

   ```text
   > !chaz workspace retry op-3
   op-3: new chat “Trip plans” (c2) — done.
   Done: room created, session created and bound, session label set, reply delivery installed, listed in the Space, linked back to the Space, invitation sent.
   Open chat: https://matrix.to/#/!trip:example
   A new session is ready; no other chat history was copied.
   ```

5. Find and rename chats from the Lobby:

   ```text
   > !chaz chat list
   Chats in this workspace:
   c1 “Bike repair” — https://matrix.to/#/!bike:example
   c2 “Trip plans” — https://matrix.to/#/!trip:example

   > !chaz chat rename c1 Winter bike
   op-4: rename c1 to “Winter bike” — done.
   Done: room name set, session label set.
   Room and session unchanged; only the label changed.
   ```

   Typing an ordinary message in the Lobby gets a reminder that it is
   command-only; nothing reaches the agent.

## Limitations

- **Text only.** The Matrix bridge currently ingests only text messages. Image, file, and other non-text Matrix events are skipped on both the live path and during history backfill. Multimodal models will not see attached images sent in the room. Restoring multimodal ingestion is tracked as a TODO in `crates/matrix-bridge/src/bridge/commands.rs` and `crates/matrix-bridge/src/bridge/history.rs`.

## Per-login state directories

Each configured login keeps its Matrix device in its own directory under
`{state_dir}/matrix/`: the `session` file, the encrypted `store/`, the store lock,
and any pending key-reset journal. Two logins that shared a directory would share
one device and its keys, so the bridge makes sure each raw login id (the `id:`, or
the `username:` when `id:` is unset) gets a directory that no other id can reach.

| Item                   | Behavior                                                                                                                                         |
| ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| New directory name     | `<prefix>-<tag>`: the id with characters outside `[A-Za-z0-9_-]` replaced by `_`, lowercased, cut to 48 characters, then a 32-hex-character tag  |
| Tag                    | First 32 hex characters of SHA-256 of `chaz-matrix-login-state-v1`, a NUL byte, and the raw id. The id is not trimmed, normalized or case-folded |
| `login-id` marker      | In every login directory, mode `0600`: `chaz-matrix-login-state-v1`, a newline, then the raw id. Written under the store lock                    |
| Legacy directory       | Name from earlier releases: the replaced-characters id, not lowercased, with no tag. Used in place when it unambiguously belongs to the login    |
| Ambiguous directory    | The whole bridge (and `keys` maintenance) refuses to start and writes nothing                                                                    |
| Empty or duplicate id  | Rejected when the config is loaded                                                                                                               |
| Moved or renamed files | Never. No directory is renamed, copied or deleted, and no device, store or cross-signing identity is regenerated by path resolution              |

### Why names changed, and what happens to an existing directory

Earlier releases named the directory by replacing every character outside
`[A-Za-z0-9_-]` with `_`. Different ids can map to the same name that way:
`@chaz:example` and `@chaz_example` both become `_chaz_example`, and on a
case-insensitive filesystem so do `@Bot:hs` and `@bot:hs`. The tag in the new name
differs for every raw id, and the lowercase prefix keeps names apart even where
the filesystem ignores case. The prefix is only there so you can recognize the
directory; the marker records exactly which id owns it, and a marker naming a
different id refuses rather than letting two logins share state.

On start, before it connects anywhere, the bridge resolves every configured login
together, using the same code `chaz-matrix keys` uses:

1. If the new-name directory exists, it is used. Its marker must name this login.
   A legacy directory that also exists is left untouched.
2. If neither exists, the new-name directory is created (mode `0700`) with a marker.
3. A legacy directory with a marker belongs to the id it names. Any other login
   that maps to the same legacy name gets a new-name directory.
4. An unmarked legacy directory that only one configured login maps to is used in
   place, provided its saved `session` (if there is one) is for that login's
   account and homeserver. A different saved account refuses, as before.
5. If several configured logins map to one unmarked legacy directory, the saved
   session decides: the one login whose account and homeserver match uses it, and
   the others get new-name directories. They could never have started from it.
6. Anything else refuses: several logins match the saved account (two `id:`
   aliases of one account), none does, or there is no session to decide by.

An adopted legacy directory keeps its name for good. The `session` file records
the store's absolute path, and a pending key reset records its staged store, so
moving the directory would break both. Symlinks anywhere in the path, a marker or
session that is a symlink or not a regular file, and an unreadable session are
refused too. Errors never print session contents.

**Repair.** A refusal lists every blocked login and the directory in question. Stop
the bridge, then either remove the login that should not own the directory from
`logins:`, or write the marker for the login that does, for example:

```sh
printf 'chaz-matrix-login-state-v1\n%s' '@chaz:example' > /var/lib/chaz-matrix/matrix/_chaz_example/login-id
chmod 600 /var/lib/chaz-matrix/matrix/_chaz_example/login-id
```

Write the raw id byte for byte, with no trailing newline. The other logins that
mapped to that directory then start fresh devices under new-name directories.

### Walkthrough: upgrading with two logins that shared a legacy name

A bridge config has two logins whose ids both map to the legacy name
`_chaz_example`, and an earlier release left that directory without a `session`:

```yaml
logins:
  - agent: chaz
    type: matrix
    homeserver_url: https://matrix.example
    username: "@chaz:example"
  - agent: helper
    id: "@chaz_example"
    type: matrix
    homeserver_url: https://matrix.example
    username: "@helper:example"
```

1. Start the upgraded bridge. Neither login can be shown to own the directory, so
   nothing starts and nothing on disk changes:

   ```text
   INFO chaz_matrix: Starting chaz-matrix config=/var/lib/chaz-matrix-demo/config.yaml label=matrix
   Error: refusing to start any Matrix login; per-login state is ambiguous or unsafe. Nothing was written, moved or regenerated.
     login "@chaz:example": legacy directory /var/lib/chaz-matrix-demo/state/matrix/_chaz_example has no saved session to tell its claimants apart ("@chaz:example", "@chaz_example"); write a login-id marker naming the owner, or remove the extra logins
     login "@chaz_example": legacy directory /var/lib/chaz-matrix-demo/state/matrix/_chaz_example has no saved session to tell its claimants apart ("@chaz:example", "@chaz_example"); write a login-id marker naming the owner, or remove the extra logins
   ```

   `chaz-matrix keys --login @chaz:example status` refuses with the same message.

2. The directory was `@chaz:example`'s, so write its marker as shown under
   **Repair** above, and start again. The first login uses the legacy directory in
   place; the second gets its own new-name directory:

   ```text
   INFO chaz_matrix: Matrix login state: using legacy-named directory in place login=@chaz:example dir=/var/lib/chaz-matrix-demo/state/matrix/_chaz_example
   INFO chaz_matrix: Matrix login state login=@chaz_example dir=/var/lib/chaz-matrix-demo/state/matrix/_chaz_example-2697f116dea2c15f91519f3371162faf
   ```

3. When the legacy directory does hold a `session`, no repair is needed: the login
   whose account matches it keeps it, and the bridge writes the marker itself the
   first time that login starts.

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
again. A rejected retry does not settle an earlier timed-out upload: pending
uncertainty remains, and `abort` still refuses it. Resume requires all three
staged private identity secrets; it refuses incomplete material rather than
letting the SDK generate another replacement. `abort` accepts a positively cancelled/rejected operation whose server identity
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
Local backup enablement alone is insufficient: the stored backup version and
private key must match the current server backup. The private identity must also
match the published identity before recovery can be configured.
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
