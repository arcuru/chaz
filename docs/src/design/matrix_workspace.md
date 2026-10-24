# Matrix Workspace

The Matrix bridge can manage one personal workspace per login: a private
Space, a command-only Lobby and fresh named chat rooms, each with exactly one
session. The user-facing model, commands and a walkthrough are in
[Matrix → Personal workspace](../user_guide/matrix.md#personal-workspace-space-lobby-and-named-chats).
This page records why v1 has this shape, the contracts the code keeps, and
what is deliberately deferred.

## Scope of v1

- One private, bot-created Space and one real Lobby room. The Lobby carries
  management commands only: it has no session and never wakes a model.
- Chats are created only by an explicit, authenticated `!chaz chat new <name>`.
  Each gets a new session; nothing existing is enrolled, imported or moved.
- A small message-command vocabulary (`!chaz workspace …`, `!chaz chat …`).
  Messages, not slash commands, because Matrix clients may intercept unknown
  slash commands. No model interprets a management command or writes a title.
- Personal audience: the configured manager and the bot. Invitations are
  explicit, never derived from Space membership.

Deferred, and not implied by anything in v1: archive, a threaded Inbox,
adopting an existing room, promoting a thread to a room, nested project
Spaces, and model-callable or proactive management.

## Contracts

**Identity is IDs, not names or Space edges.** A chat is the recorded room ID
and session ID. Space child/parent state links rooms for navigation only. The
record also pins the login, owning agent and manager it was created for; a
bridge configured differently refuses to act on it.

**Authorization is the exact manager.** A request is accepted only from the
login's `workspace_manager` MXID, after the ordinary allow-list and decrypted
event path. Room membership never substitutes, and a manager change refuses
the existing record rather than inheriting it.

**Workspace rooms stay out of generic session paths.** The message handler
routes every event through `workspace::route` before history bootstrap or any
session lookup. Join bootstrap skips rooms the record lists and rooms whose
homeserver state carries the `dev.chaz.workspace` marker. A ready chat
resolves its session by binding and never falls back to get-or-create, so a
failed lookup cannot mint a replacement. `attach`, `detach`, `participation`
and the model-based `rename` are refused in workspace chats.

**Outputs stay addressed.** A chat's replies and approvals use the existing
`(login, room, session)` delivery reconciler and approval watcher, installed
during provisioning and reinstalled at startup like any bound room. Workspace
notices contain management metadata only, posted to the room that asked.

**Creation is journaled, not atomic.** Matrix `createRoom` has no transaction
ID, and Matrix state and Eidetica writes cannot share a transaction. Every
request is an operation whose step progress is written to the peer-local
`chaz_peer` DB before each external effect:

1. The room-creation step is marked _attempted_ before a single POST with SDK
   retries disabled. A 4xx answer means no room and resets the step; any other
   failure leaves the operation _uncertain_. An attempted step is reconciled,
   never re-sent: exactly one joined room created by the bot with this
   operation's marker continues the operation; zero or several keep it held.
2. The chat's session DB is created with the operation marker in its initial
   settings (the same commit that creates the DB), so
   `provision_marked_conversational_channel_session` recovers a DB whose ID
   was lost and refuses if two carry the marker or the room is already bound
   elsewhere.
3. Binding, owning-agent hosting, publication on the login and delivery are
   installed before the chat is marked ready; only then are Space links set and
   the manager invited, so no first message can arrive before the chat can
   answer it.

Duplicate sync delivery returns the stored receipt for that event ID. One
operation is unfinished at a time. Partial results are reported step by step
and continue only on an explicit `retry`; `cancel` never deletes, leaves or
re-permissions a room.

**Privacy is set explicitly.** Each room is created invite-only, with joined
history visibility, guest access forbidden, no directory entry and no
invitations in the create request; the Lobby and chats enable encryption at
creation. The power-level override gives the manager 100. Room version 12+
makes the creator's power permanent; the preview states the actual version
and rule, and an unrecognised version refuses setup rather than guessing.

## Alternatives considered

| Option                                | Why not in v1                                                                                         |
| ------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| Manual Space plus existing commands   | Users create rooms and reconcile names by hand; `attach` is not a safe lifecycle primitive.           |
| Inbox first (one thread per question) | Thread support is off by default in current mobile clients and the threaded-session mode is unmerged. |
| Mirror every session into the Space   | Exposes internal sessions and metadata, implies relationships and access that do not exist.           |
| Human-created Space and Lobby         | Avoids bot creator permanence there, but chats created by the bot would still make it creator.        |
| Reuse `!chaz name` / `!chaz rename`   | `name` sets a globally unique alias; `rename` asks a model. Neither is a literal shared label.        |

## Roadmap

On-request named rooms (v1) → an optional, client-verified threaded Inbox or
explicit enrollment of an existing room → separately accepted thread-to-room
promotion → opt-in suggestions → only then bounded automatic management.

Future direction, not current scope:

- **Archive** means moving a chat under an Archive sub-Space of the workspace,
  keeping its room, session and history. It does not mean hiding a chat by
  removing it from the workspace, pausing it, or leaving or deleting the room.
- **Model-callable management**, at least from a future model-backed Lobby
  session, would call the same authenticated, journaled operations. The bridge
  may then keep the Matrix name and topic in step with the session's existing
  title and description and selected operational metadata such as the model,
  rather than a second naming model. Disclosing such metadata into room state
  needs its own decision.
- The command-only Lobby is a v1 constraint, not a permanent rule.

## Code map

| Concern                               | Where                                                                                                                |
| ------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| Record, operations, commands, routing | `crates/matrix-bridge/src/bridge/workspace/mod.rs`                                                                   |
| SDK and registry effects              | `crates/matrix-bridge/src/bridge/workspace/live.rs` (`create_request`, `LiveMatrix`, `LiveSessions`)                 |
| Marker-based session provisioning     | `session::transport` (`provision_marked_conversational_channel_session`), `SessionRegistry::find_sessions_by_source` |
| Handler integration and exclusions    | `crates/matrix-bridge/src/bridge/mod.rs` (`backfill_joined_room`, message handler, `spawn_workspace_command`)        |
