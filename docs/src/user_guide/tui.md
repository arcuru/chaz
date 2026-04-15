# TUI Mode

The TUI (Terminal User Interface) provides a local chat interface for testing, debugging, and session management without Matrix.

```bash
chaz --config config.yaml --tui
```

## Interface Layout

```
+--[ Chaz ]------------------------------------------+
| user:                                               |
|   What's the current time?                          |
|                                                     |
| default thinking...                                 |
|   > get_time({})                                    |
|   < get_time: 2026-04-15T10:30:00Z                 |
|                                                     |
| default:                                            |
|   The current time is 10:30 AM UTC.                 |
|                                                     |
+-----------------------------------------------------+
| tui | agent: default | messages: 2 | /help          |
+--[ > ]----------------------------------------------+
| type here...                                        |
+-----------------------------------------------------+
```

The TUI has three sections:

1. **Messages area** — conversation history with all entry types
2. **Status bar** — current session, agent, message count
3. **Input box** — type messages and commands

## Commands

| Command | Description |
|---------|-------------|
| `/help`, `/?` | Show all commands and key bindings |
| `/sessions`, `/s` | Open session picker |
| `/new` | Create a new session |
| `/join <id>` | Switch to a session by transport ID |
| `/info` | Show current session details (entry counts, debug state) |
| `/share` | Generate a shareable ticket URL for the current session |
| `/sync <ticket>` | Sync a remote session via a ticket URL |
| `/clear` | Clear the display (entries remain in the database) |
| `/raw` | Dump all raw entry data (index, timestamp, type, sender, content) |
| `/debug` | Toggle debug mode |
| `/quit`, `/q` | Exit |

## Key Bindings

| Key | Action |
|-----|--------|
| `Enter` | Send message or execute command |
| `Ctrl+D` | Toggle debug mode |
| `Ctrl+C` | Quit |
| `Up/Down` | Scroll messages (3 lines) |
| `PageUp/PageDown` | Fast scroll (20 lines) |
| `Home/End` | Move cursor to start/end of input |
| `Esc` | Quit |

## Debug Mode

Toggle with `Ctrl+D` or `/debug`. When active:

- Every entry shows its timestamp and type (e.g., `[10:30:00 Message]`)
- Tool result previews expand from 120 to 500 characters
- The status bar shows `DEBUG`

This is useful for understanding the session entry flow, correlating with log output, and debugging agent behavior.

The `/raw` command provides an even more detailed dump: every entry's index, timestamp, type, sender, and content in a tabular format.

## Session Picker

Open with `/sessions` or `/s`:

```
+--[ Sessions ]---------------------------------------+
|                                                     |
| > tui * (default, 15 entries)                       |
|     user: What's the current time?                  |
|                                                     |
|   !room123:matrix.org (default, 42 entries)         |
|     user: Tell me about quantum computing           |
|                                                     |
|   spawn:abc-123 (researcher, 3 entries)             |
|     default: Research the latest AI papers           |
|                                                     |
+-----------------------------------------------------+
| [Up/Down] navigate | [Enter] select | [n] new | ... |
+-----------------------------------------------------+
```

The picker shows all sessions from the registry, including:

- Matrix room sessions
- TUI sessions
- Child sessions created by `spawn_agent`
- Sessions synced from remote instances

The current session is marked with `*`. Press `Enter` to switch, `n` to create a new session, or `Esc` to cancel.

## Entry Types

The TUI renders different entry types with distinct styles:

| Type | Appearance | Description |
|------|------------|-------------|
| Message | **Bold colored sender** + content | Chat messages from users and agents |
| Directive | **Bold sender (directive):** + content | Task instructions (from spawn_agent, scheduler) |
| Ack | Dimmed "*agent* thinking..." | Agent is processing |
| ToolCall | Dimmed `> tool_name(args)` | Agent invoked a tool |
| ToolResult | Dimmed `< tool_name: output` | Tool returned a result |
| Error | Red `ERROR sender: message` | An error occurred |

Senders are color-coded: agents in green, users in cyan, system in yellow.

## Tool Approval

When an agent calls a tool that requires approval, the TUI shows an inline prompt:

```
--- Tool Approval Required ---
  Tool: shell
  Risk: High
  Args: {"command": "ls -la"}
  [y]es  [n]o  [a]ll
```

Press `y` to approve, `n` to deny, or `a` to approve all remaining tool calls for this turn.
