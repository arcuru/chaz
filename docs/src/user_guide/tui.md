# TUI Mode

The TUI (Terminal User Interface) is the default surface. It provides a local chat interface for testing, debugging, and session management without Matrix.

```bash
chaz --config config.yaml
```

You can pre-fill the input box with a starting prompt by passing it as a positional argument:

```bash
chaz --config config.yaml "what's on my plate today?"
```

## Interface Layout

```text
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
| tui | agent: default | model: gpt-5 | ctx 6% | 1.5k/0.2k tok • 0% cached |
+--[ > ]----------------------------------------------+
| type here...                                        |
+-----------------------------------------------------+
```

The TUI has four main pieces:

1. **Tab bar** — one tab per open session. Click to switch, `[x]` to close, or use `Ctrl+PageUp`/`Ctrl+PageDown`. Closing the last tab is refused (the TUI always shows at least one session).
2. **Messages area** — conversation history with all entry types
3. **Status bar** — session name, then the agent and model. A single-agent
   session shows `agent: <name> | model: <model>`; a multi-agent session lists
   the whole roster with the host marked `*` and each agent's model
   (`agents: alpha*→opus, beta→haiku`), collapsing to a count if it would
   overflow. Then `ctx N%` — how full the **primary (host) agent's** context
   window is, based on its most recent turn — followed by the session's
   running token totals and cost (`<prompt>/<completion> tok • <cached>% cached
• $<cost>`), summed across **all** agents. `DEBUG` / `EXP` indicators append
   when those modes are on.
4. **Input box** — type messages and commands. Slash commands open an inline completion popup with grouped categories; arrow keys move the highlight.

When prior sessions exist, the TUI opens straight into the session picker on launch so you choose which one to resume (or pick the "New session" row). A truly fresh state directory drops directly into the default `tui` session.

## Commands

The TUI catalogs every built-in slash command in its inline completion popup — type `/` to open it, `Tab` / arrow keys to navigate, `Enter` to insert. `F1` or `/help` shows the same catalog as a scrollable overlay. The list below is the same one rendered there, grouped the same way.

### Session

| Command           | Description                                                            |
| ----------------- | ---------------------------------------------------------------------- |
| `/help`, `/?`     | Open the help overlay (also `F1`)                                      |
| `/sessions`, `/s` | Open the session picker (also `Ctrl+P`)                                |
| `/new`            | Create a new session and switch to it                                  |
| `/join <ref>`     | Switch to a session by name or eidetica DB ID                          |
| `/name <alias>`   | Set a human-friendly alias for the current session (also `/rename`)    |
| `/name`           | Clear the session alias                                                |
| `/info`           | Show current session details (name, DB ID, entry counts)               |
| `/costs`          | Aggregate LLM usage and cost across all sessions ([details](usage.md)) |
| `/channels`       | List Matrix rooms currently attached to this session                   |
| `/share`          | Generate a shareable ticket URL for the current session                |
| `/sync <ticket>`  | Sync a remote session via a ticket URL                                 |
| `/compact`        | Summarize and compact conversation history                             |
| `/print`          | Dump the transcript                                                    |

### Living Agents

See [Agents](agents.md) for the model and full per-command behaviour.

| Command                                            | Description                                                               |
| -------------------------------------------------- | ------------------------------------------------------------------------- |
| `/agents`, `/agent list`                           | List agents attached to this session                                      |
| `/agent add <ref>`                                 | Attach an agent (display name or DB ID)                                   |
| `/agent remove <ref>`                              | Detach an agent                                                           |
| `/agent host [<ref>]`                              | Set (or clear, with no arg) the session's host agent                      |
| `/agent room`                                      | Chat-room status: roster, host, burst budget                              |
| `/agent hosted`                                    | List every Living Agent this peer hosts                                   |
| `/agent new <name> [k=v ...]`                      | Create a Living Agent on this peer                                        |
| `/agent set <ref> <field> <value>`                 | Edit an agent's runtime config (takes effect on next message)             |
| `/agent delete <ref>`                              | Unregister a Living Agent (DB preserved)                                  |
| `/agent share <ref>`                               | Generate a share ticket for an agent's DB                                 |
| `/agent unshare <ref>`                             | Stop sharing an agent DB                                                  |
| `/agent import <ticket> [perm]`                    | Request access to an agent DB (`admin`\|`write`\|`read`, default `write`) |
| `/agent invite <ref> <pubkey> [perm]`              | Pre-seed another peer's pubkey on this agent (`admin`\|`write`\|`read`)   |
| `/agent revoke-peer <ref> <pubkey>`                | Revoke a co-owner's access                                                |
| `/agent rehost [--agent] [--clear] <ref> [pubkey]` | Reassign the home peer for an agent or its session-level entry            |
| `/agent home-status [<ref>]`                       | List `home_pubkey` per agent + session                                    |
| `/pubkey`                                          | Show this peer's default pubkey                                           |

### Memory & Skill banks

See [Memory](memory.md).

| Command                               | Description                                                    |
| ------------------------------------- | -------------------------------------------------------------- |
| `/memory list`                        | List memory banks this peer hosts                              |
| `/memory new <name>`                  | Create a new bank on this peer                                 |
| `/memory delete <name>`               | Unregister a bank (DB preserved)                               |
| `/memory grant <bank> <agent> [perm]` | Grant an agent access to a bank (`read`\|`write`)              |
| `/memory revoke <bank> <agent>`       | Revoke an agent's access                                       |
| `/memory share <name>`                | Generate a share ticket for a bank's DB                        |
| `/memory unshare <name>`              | Stop sharing a memory bank                                     |
| `/memory import <ticket> [perm]`      | Request access to a bank via ticket (`admin`\|`write`\|`read`) |

### Sharing queue (co-ownership)

| Command                       | Description                                   |
| ----------------------------- | --------------------------------------------- |
| `/sharing`, `/sharing status` | List databases this peer is currently sharing |
| `/sharing requests`           | List pending bootstrap requests               |
| `/sharing approve <id>`       | Approve a bootstrap request by id             |
| `/sharing reject <id>`        | Reject a bootstrap request by id              |
| `/unshare`                    | Stop sharing the current session              |

### Schedule

See [Agents — Schedules](agents.md#schedules).

| Command                                       | Description                                               |
| --------------------------------------------- | --------------------------------------------------------- |
| `/schedule list`                              | List an agent's schedules                                 |
| `/schedule add <id> <cron> <agent> <task...>` | Add a schedule (6-field cron: `sec min hour dom mon dow`) |
| `/schedule remove <id>`                       | Remove a schedule by id                                   |

### Extensions

See [Extensions](extensions.md). Extensions can also register their own slash commands; they appear in the completion popup once installed.

| Command                                | Description                                                 |
| -------------------------------------- | ----------------------------------------------------------- |
| `/extensions`, `/extensions list`      | List extensions and per-session/per-agent status            |
| `/extensions add <name> [agent]`       | Enable an extension on this session or for a specific agent |
| `/extensions remove <name> [agent]`    | Disable an extension                                        |
| `/extensions settings <name>`          | Print the extension's settings                              |
| `/extensions set <name> <key> <value>` | Update an extension setting                                 |

### LLM config

| Command                       | Description                                                                    |
| ----------------------------- | ------------------------------------------------------------------------------ |
| `/models`                     | Open Session Settings → [Models](#model-picker)                                |
| `/model`                      | Show the model resolved for the current agent + every override on this session |
| `/model <id>`                 | Set the session-wide model pin (every agent unless per-agent override wins)    |
| `/model <agent> <id>`         | Set a per-agent override scoped to this session                                |
| `/model <agent> clear`        | Clear that agent's per-agent override                                          |
| `/role [<name> [<prompt>]]`   | Show, select, or define a role                                                 |
| `/backend <name> <url> <key>` | Add a custom backend for the session                                           |
| `/backends`                   | List known backends and models                                                 |

**Model resolution order** for any given turn (highest priority first):

1. Per-agent session override — `SessionMeta.agent_models[agent_name]`
2. Session-wide pin — `SessionMeta.model`
3. The agent's `default_model` from its DB config (seeded from YAML `agents[].model`)
4. The backend's default model

`/model` (no args) names which source wins for the _current agent_, so the display always matches what the next message will actually run.

### TUI utilities

| Command                | Description                                                   |
| ---------------------- | ------------------------------------------------------------- |
| `/clear`               | Clear the display (entries remain in the database)            |
| `/raw`                 | Dump raw entry data (index, timestamp, type, sender, content) |
| `/debug`               | Toggle debug mode (also `Ctrl+D`)                             |
| `/quit`, `/q`, `/exit` | Exit                                                          |

Unknown `/<name>` commands route to extension dispatch — see the error you get back for the closest match.

## Key Bindings

| Key                             | Action                                                              |
| ------------------------------- | ------------------------------------------------------------------- |
| `Enter`                         | Accept highlighted completion (if extending); else send / execute   |
| `Tab` / `Shift+Tab`             | Open completion popup and cycle highlighted entry                   |
| `Up` / `Down`                   | Move completion selection if popup is open; else scroll history (3) |
| `PageUp` / `PageDown`           | Fast scroll history (20 lines)                                      |
| `Home` / `End`                  | Move cursor to start / end of input                                 |
| `Esc`                           | First press: dismiss completion popup. Second (no popup): quit      |
| `F1`                            | Open the help overlay                                               |
| `Ctrl+P`                        | Toggle the session picker                                           |
| `Ctrl+D`                        | Toggle debug mode                                                   |
| `Ctrl+W`                        | Close the active tab (refuses to close the last tab)                |
| `Ctrl+PageUp` / `Ctrl+PageDown` | Cycle to previous / next tab (wraps)                                |
| `Ctrl+C`                        | Quit                                                                |

Approval prompts hijack the keyboard while open: `y` approve, `n` deny, `a` approve all remaining tool calls for this turn.

### Mouse

Mouse capture is enabled. Click on completion rows, help-overlay command rows, approval buttons, picker rows, tab titles, or tab close `[x]` widgets to act on them. The scroll wheel scrolls history (or the active overlay when one is open).

## Debug Mode

Toggle with `Ctrl+D` or `/debug`. When active:

- Every entry shows its timestamp and type (e.g., `[10:30:00 Message]`)
- Tool result previews expand from 120 to 500 characters
- The status bar shows `DEBUG`

This is useful for understanding the session entry flow, correlating with log output, and debugging agent behavior.

The `/raw` command provides an even more detailed dump: every entry's index, timestamp, type, sender, and content in a tabular format.

## Session Picker

Open with `/sessions` or `/s`:

```text
+--[ Sessions ]---------------------------------------+
|                                                     |
| > sha256:abc… "tui" * (default, 15 entries)         |
|     user: What's the current time?                  |
|                                                     |
|   sha256:def… (default, 42 entries)                 |
|     user: Tell me about quantum computing           |
|                                                     |
|   sha256:xyz… (researcher, 3 entries)               |
|     default: Research the latest AI papers          |
|                                                     |
+-----------------------------------------------------+
| [Up/Down] navigate | [Enter] select | [n] new | ... |
+-----------------------------------------------------+
```

Sessions are listed by their eidetica DB root ID; any attached Matrix rooms or human-friendly names appear alongside. The picker shows every session the registry knows about: TUI, Matrix-attached, `spawn_agent` / `spawn_worker` children, and anything synced from remote peers. The current session is marked with `*`. Press `Enter` to switch, `n` to create a new session, or `Esc` to cancel.

## Named Sessions

Give sessions human-friendly names instead of opaque IDs:

```text
/name daily-standup
```

Named sessions can be referenced anywhere a session identifier is accepted:

```text
/join daily-standup
```

The name appears in the status bar, session picker, and `/info` output. Names must be unique across all sessions. Use `/name` (with no argument) to clear the name.

## Model Picker

Pick a model for a specific scope — the whole session, or one agent in this session. `/models` opens Session Settings → Models, where each row is a scope you can edit:

```text
+--[ Models ]----------------------------------------------------+
|                                                                |
| > Session         claude-opus-4-7                              |
|       resolves to claude-opus-4-7                              |
|                                                                |
|   Per-agent overrides                                          |
|   ava             (uses session pin)                           |
|   researcher      openai/gpt-5-mini                            |
|                                                                |
|   Enter — open picker for selected scope                       |
+----------------------------------------------------------------+
```

- **`Session`** (row 0) — the session-wide pin (`SessionMeta.model`, what `/model <id>` writes). Every agent uses this unless its own row sets an override.
- **`<agent>`** — per-agent override for that agent (`SessionMeta.agent_models[name]`, what `/model <agent> <id>` writes). Falls back to the session pin when unset.

`↑` / `↓` (or click) selects a row. `Enter` opens the picker locked to that row's scope:

```text
+--[ Search models (143) ]----------------------------------------+
|   > _                                                          |
+-----------------------------------------------------------------+
+--[ Pick model — researcher ▼ ]---------------------------------+
|   MODEL                              IN     OUT   CACHE   CAPS |
|   ▸ anthropic/claude-opus-4.7      $15.0  $75.0    $1.5   V    |
|     openai/gpt-5-mini               $0.40  $1.6     —     V    |
|     ...                                                        |
+-----------------------------------------------------------------+
| type to filter | ↑↓ PgUp/Dn Home/End | Enter select | ...      |
+-----------------------------------------------------------------+
```

The title names the scope you're editing. The picker pulls the live OpenRouter catalog (cached 24 h, refresh with `Ctrl+R`) and merges it with the models declared in your YAML `backends:` so favorites stay pinned at the top. Each row shows input / output / cache-read prices in $/Mtok plus a capability badge: `V`ision (image input), `A`udio (audio input), `M`ovie (video input), `I`mage-gen (image output), `S`peech (audio output).

Typing in the search box does fzf-style fuzzy matching across model ids and capability labels — `vision` filters to vision-capable models without a separate UI; `claude opus` finds Anthropic's top tier across providers.

`Enter` writes the highlighted model to whichever scope the picker is locked to and returns you to the Models page. The scope is set when you open the picker — there's no in-picker scope switching; pick a different row to edit a different scope.

| Key                   | Action                                              |
| --------------------- | --------------------------------------------------- |
| (typing)              | Append to fuzzy-search query                        |
| `↑` / `↓`             | Move cursor in the filtered list                    |
| `PageUp` / `PageDown` | Jump 10 rows                                        |
| `Home` / `End`        | Jump to first / last row                            |
| `Enter`               | Apply the highlighted model to the picker's scope   |
| `Ctrl+R`              | Force-refresh the catalog (bypass the 24 h cache)   |
| `Ctrl+U`              | Clear the search query                              |
| `Esc`                 | Dismiss without changing anything; return to Models |

There is no global key binding for `/models` — terminals without the keyboard-enhancement protocol can't distinguish `Ctrl+M` from `Enter`, which made any natural binding unreliable through `tmux + ssh`. Type `/models` to open.

## Entry Types

The TUI renders different entry types with distinct styles:

| Type       | Appearance                             | Description                                                    |
| ---------- | -------------------------------------- | -------------------------------------------------------------- |
| Message    | **Bold colored sender** + content      | Chat messages from users and agents                            |
| Directive  | **Bold sender (directive):** + content | Task instructions (from spawn_agent / spawn_worker, scheduler) |
| Ack        | Dimmed "_agent_ thinking..."           | Agent is processing                                            |
| ToolCall   | Dimmed `> tool_name(args)`             | Agent invoked a tool                                           |
| ToolResult | Dimmed `< tool_name: output`           | Tool returned a result                                         |
| Error      | Red `ERROR sender: message`            | An error occurred                                              |

Senders are color-coded: agents in green, users in cyan, system in yellow.

## Tool Approval

When an agent calls a tool that requires approval, the TUI shows an inline prompt:

```text
--- Tool Approval Required ---
  Tool: shell
  Risk: High
  Args: {"command": "ls -la"}
  [y]es  [n]o  [a]ll
```

Press `y` to approve, `n` to deny, or `a` to approve all remaining tool calls for this turn.
