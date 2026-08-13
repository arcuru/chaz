# Discord Bot

Chaz connects to Discord as a bot, responding in channels it can see. The
Discord bridge runs as its own process and eidetica peer (`chaz-discord`),
separate from the `chaz` daemon — read [Transport Bridges](bridges.md) first for
the architecture and the one-time approval flow. This page covers
Discord-specific configuration and behavior.

## Discord portal setup

1. Create an application and a bot at the
   [Discord Developer Portal](https://discord.com/developers/applications), and
   copy the **bot token**.
2. Under **Bot → Privileged Gateway Intents**, enable **Message Content
   Intent** — the bridge requests it, and without it message bodies arrive
   empty.
3. Invite the bot to your server with the **Send Messages**, **Read Message
   History**, and **Add Reactions** permissions (reactions drive the
   approve/deny flow).

## Setup

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
   backends:
     - name: openai
       api_key: ${OPENAI_API_KEY}
   agents:
     - name: chaz
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

- **DMs**: the bot responds to every message from an allowed user.
- **Server channels**: the bot responds to messages prefixed with `!chaz` or
  that mention the bot.

Each Discord channel is bound to its own session, fixed per channel (unlike
Matrix, you don't rebind a channel to a different session from within Discord).
The first message in a channel auto-creates the session.

## Rate limiting

`message_limit` caps how many messages one Discord user may send the bot for
the life of the bridge process; over the cap the bridge answers with an error
and writes nothing to the session. `!chaz` commands are exempt. The limit is
unset by default, and it is a per-process counter, not a per-hour rate — it
exists to keep a chatty channel from growing the session DB without bound.

`room_size_limit` is Matrix-only: it counts joined room members, which has no
Discord analogue worth the gateway calls. Use `allowed_users` to bound who can
reach the bot in a large server.

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
- [Matrix Bot](matrix.md) — the full command reference (shared with Discord)
- [Sharing & Sync](session_sharing.md) — tickets and the `/sharing` queue
