# Discord Bot

Chaz connects to Discord as a bot, responding in channels it can see. The
Discord bridge runs as its own process (`chaz-discord`),
separate from the `chaz` daemon — read [Transport Bridges](bridges.md) first for
the architecture and the one-time approval flow. This page covers
Discord-specific configuration and behavior.

## Discord portal setup

1. Create an application and a bot at the
   [Discord Developer Portal](https://discord.com/developers/applications), and
   copy the **bot token**.
2. Under **Bot → Privileged Gateway Intents**, enable **Message Content
   Intent**. The bridge requests this gateway intent to read channel text.
3. Invite the bot to your server with the **Send Messages**, **Read Message
   History**, and **Add Reactions** permissions (reactions drive the
   approve/deny flow).

## Setup

These steps use a direct Eidetica owner with a provisioned store. When sharing the
executor's Eidetica service login, use the [service connection
settings](bridges.md#connection-settings), omit `ticket:` and skip the share and
approval steps.

1. **Share the owning agent from the daemon** and copy the ticket:

   ```text
   /agent share chaz
   # eidetica:?db=<agent_db_id>&pr=iroh:<addr>
   ```

2. **Write the bridge config** (default `$XDG_CONFIG_HOME/chaz/discord-bridge.yaml`,
   or pass `--config`):

   ```yaml
   unlock_password: ${CHAZ_BRIDGE_UNLOCK}

   logins:
     - agent: chaz
       ticket: "eidetica:?db=<agent_db_id>&pr=iroh:<addr>"
       bot_token: ${DISCORD_TOKEN} # or omit and set the DISCORD_TOKEN env var
       allowed_users: [123456789012345678] # Discord user ids; empty = allow everyone
       # login_id: discord             # optional; defaults to "discord"

   # chaz runtime keys the embedded server needs:
   state_dir: /var/lib/chaz-discord
   execution: client
   eidetica:
     connection: "sqlite:///var/lib/chaz-discord/eidetica.db"
     login: { username: chaz-discord, password: your-eidetica-password }
     sync: { iroh: true }
   ```

   `bot_token` accepts a `${ENV}` reference or literal; when omitted it falls
   back to the `DISCORD_TOKEN` environment variable, so the secret can stay out
   of the file. `allowed_users` is an optional allow-list of Discord user ids —
   empty means everyone (the bot always ignores other bots and its own
   messages).

3. **Run the bridge** (with the daemon already running):

   ```bash
   chaz-discord --config /etc/chaz/discord-bridge.yaml
   ```

4. **Approve the access request** on the daemon (`/sharing requests` →
   `/sharing approve <id>`) and restart the bridge — see
   [Transport Bridges → Setup](bridges.md#setup).

## Message Handling

1. In a channel intended for the bot, send `What did we decide?` from an allowed
   account. A DM works the same way. **No mention is required in a server channel.**
   Keep the bot out of channels whose conversation should not reach the agent.
   `allowed_users` filters senders, not destinations.
2. The bridge finds or creates the owning agent's session for this login and channel
   ID, installs reply delivery, and writes the message body. A second message in the
   channel uses that session. `!chaz help` shows bridge commands instead of starting
   a turn; some shared commands (such as `!chaz info`) also need a session and can
   create it before the first chat message.
3. Replies from the bound session appear in the channel. Unlike Matrix, Discord
   delivers ordinary agent messages even if the turn began in a local client. Use a
   separate session for private work.

## Rate limiting

`message_limit` is a top-level setting. Each login counts ordinary messages per
Discord user until the bridge restarts. By default there is no cap.

For example, with `message_limit: 2`, the user's third ordinary message gets:

```text
!chaz Error: you have used up your message limit of 2 messages.
```

Nothing is written to the session. Waiting an hour won't help; `!chaz help`
still works because commands are exempt. If the cap was unintended, change the
setting and restart the bridge. There is no need to replace the session.

`room_size_limit` is Matrix-only.

## Commands

Commands are sent as Discord messages prefixed with `!chaz`, routed through the
same transport-neutral dispatch the TUI and Matrix bridge use. `!chaz help`
lists the surface; the common commands match the
[Matrix command tables](matrix.md#commands) (session ops, `!chaz agent …`,
`!chaz sharing …`, `!chaz model`, etc.). Channel-rebinding verbs don't apply on
Discord.

## Tool Approval

Approval requests are posted as messages in the channel. Respond either via
reactions (✅ approve · ❌ deny · ⏭ approve all) or by replying `!chaz approve` /
`!chaz deny`. To skip approval for specific low-risk tools, add them to
`security.auto_approved_tools`.

## Limitations

- **Text only.** Like the Matrix bridge, the Discord bridge ingests only text;
  attachments and embeds are not seen by the model.
- **One channel ↔ one session**, fixed. There is no in-Discord rebind command.

## See also

- [Transport Bridges](bridges.md) — architecture, config layout, approval flow
- [Matrix Bot](matrix.md) — shared commands and Matrix-only room controls
- [Sharing & Sync](session_sharing.md) — tickets and the `/sharing` queue
