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

Commands are sent as Matrix messages:

| Command                                     | Description                                   |
| ------------------------------------------- | --------------------------------------------- |
| `!chaz help`                                | Show available commands                       |
| `!chaz print`                               | Print the current conversation context        |
| `!chaz send <msg>`                          | Send a message without conversation context   |
| `!chaz model <model>`                       | Set the model for this room                   |
| `!chaz backend <name> <api_base> <api_key>` | Add a custom backend                          |
| `!chaz role`                                | Show current role and available roles         |
| `!chaz role <name>`                         | Set role for this room                        |
| `!chaz role <name> <prompt>`                | Define a new role                             |
| `!chaz list`                                | List available models                         |
| `!chaz clear`                               | Ignore all messages before this point         |
| `!chaz rename`                              | Rename the room based on conversation content |

## Per-Room Settings

Each room stores its own model, role, and backend selection using Matrix room tags in the `is.chaz.*` namespace. These persist across restarts.

## Session Persistence

Each Matrix room maps to a dedicated eidetica session database. Conversation history survives bot restarts. The Matrix sync token is persisted by headjack, so the bot resumes from where it left off.

Message batching prevents duplicate responses after a restart: messages received during the catch-up sync that were already processed are skipped.

## Retry Behavior

If the Matrix connection drops, the bot retries with a 5-second backoff. The retry loop handles transient network errors and homeserver restarts.

## Tool Approval

Tool approval in Matrix is not yet implemented (TUI-only for now). Tools that require approval will time out and be denied when running via Matrix. Configure `security.auto_approved_tools` for tools you want to run without approval.
