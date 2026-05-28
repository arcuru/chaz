# Matrix Bot

Chaz connects to Matrix as a bot, responding to messages in rooms it's invited to.

## Setup

1. Create a Matrix account for the bot on any homeserver
2. Configure `homeserver_url`, `username`, `password`, and `allow_list` in your config
3. Run `chaz --config config.yaml`

The bot will log in, accept invites from allowed users, and start responding.

## Message Handling

- **DMs**: The bot responds to every message
- **Group rooms**: The bot responds to messages prefixed with `!chaz` or that mention the bot

To send a message with room context:

```text
!chaz summarize the discussion so far
```

To send without the `!chaz` prefix in a DM, just type normally.

## Commands

Commands are sent as Matrix messages. Session ops go through the same transport-neutral dispatch as the TUI — both gateways stay in sync. Most TUI slash commands have a `!chaz` equivalent; the table below covers the common surface. Extension-registered commands (e.g. `!chaz schedule`, `!chaz memory`) are auto-registered for whichever extensions are installed — see the relevant page for syntax.

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

A Matrix room is connected to a session through an explicit _channel_ record (`room_id → session_db_id`). The first time you talk to the bot in a new room it auto-creates a session and attaches the room to it.

Use `!chaz attach <session>` to rebind the room to a different session (e.g., to resume a synced session, or to route a scheduled-task session into a specific room). Multiple rooms can attach to the same session — responses fan out to every attached room. `!chaz detach` removes the binding; the next message in the room creates a fresh session.

At gateway startup, the bot re-installs response-delivery callbacks for every persisted channel whose room it's joined to. This is what makes scheduled-task responses reach a Matrix room even when no user is currently active there.

## Per-Session Settings

Model, role, and backend selections live in the session's own eidetica database (under a `meta` DocStore), not on the room. That means a session's config travels with it across eidetica sync — sharing a session shares its name, agent, model, role, and backend reference.

## Session Persistence

Conversation history lives in per-session eidetica databases and survives bot restarts. The Matrix sync token is persisted by headjack, so the bot resumes from where it left off. Message batching prevents duplicate responses after a restart: messages received during the catch-up sync that were already processed are skipped.

## Retry Behavior

If the Matrix connection drops, the bot retries with a 5-second backoff. The retry loop handles transient network errors and homeserver restarts.

## Tool Approval

The bot surfaces approval requests as markdown notices in the room. Respond either via reactions (✅ approve · ❌ deny · ⏭ approve all) or by sending `!chaz approve` / `!chaz deny`. To skip approval altogether for specific low-risk tools, add them to `security.auto_approved_tools`.

## Limitations

- **Text only.** The Matrix gateway currently ingests only text messages. Image, file, and other non-text Matrix events are skipped on both the live path and during history backfill. Multimodal models will not see attached images sent in the room. Restoring multimodal ingestion is tracked as a TODO in `crates/bin/src/gateway/matrix/commands.rs` and `crates/bin/src/gateway/matrix/history.rs`.
