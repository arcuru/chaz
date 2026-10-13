# TUI Mode

The TUI (Terminal User Interface) is the default surface. It provides a local chat interface for testing, debugging, and session management without Matrix.

```bash
chaz --config config.yaml
```

| Launch                         | Opens                                                          | Creates a session?       |
| ------------------------------ | -------------------------------------------------------------- | ------------------------ |
| `chaz`                         | The [session hub](#session-hub), even when sessions exist      | No                       |
| `chaz "PROMPT"`                | A new conversation with `PROMPT` in the composer, unsent       | Yes, exactly one         |
| `chaz --session NAME`          | The session named `NAME`                                       | Only if `NAME` is absent |
| `chaz --session NAME "PROMPT"` | The session named `NAME` with `PROMPT` in its composer, unsent | Only if `NAME` is absent |

A prompt is never sent for you; review it and press `Enter`. `--session` takes a
session name, not an ID. If the name exists but its session cannot be opened,
chaz exits with the error instead of opening or creating another conversation.
`-p`/`--print` and `cmd` are unchanged.

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
| tui | ctx ~7.7k/128k (6.0%) tok | agent: default | model: gpt-5 | 1.5k/0.2k tok • 0% cached |
+--[ > ]----------------------------------------------+
| type here...                                        |
+-----------------------------------------------------+
```

The TUI has four main pieces:

1. **Tab bar** — one tab per open conversation view. Click to switch, `×` to close, or use `Ctrl+H`/`Ctrl+L` for the previous/next tab. Closing a view never deletes, detaches, or stops its session; closing the last one returns to the [session hub](#session-hub). The right side lists the keys that fit.
2. **Messages area** — conversation history with all entry types
3. **Status bar** — session name, context usage/budget, then the agent and model. A single-agent
   session shows `agent: <name> | model: <model>`; a multi-agent session lists
   the whole roster with the host marked `*` and each agent's model
   (`agents: alpha*→opus, beta→haiku`), collapsing to a count if it would
   overflow. `ctx ~<used>/<max> (<percent>%) tok` shows the host's last reported input
   count, effective input budget, and occupancy percentage (see [Context usage](#context-usage)).
   After the agent/model comes the session's
   running token totals and cost (`<prompt>/<completion> tok • <cached>% cached
• $<cost>`), summed across **all** agents. `DEBUG` / `EXP` indicators append
   when those modes are on, and `⚠ refresh failed` appends while the tab's last
   background read failed (see [Responsiveness](#responsiveness-and-background-refresh)).
4. **Input box** — type messages and commands. Slash commands open an inline completion popup with grouped categories; arrow keys move the highlight. The input wraps at the terminal edge and grows with the draft, capped so it never takes the whole frame (a long draft squeezes the transcript instead); past that cap the box scrolls to keep the cursor visible. Use `Alt+Enter` for a new line, `Enter` to send, and `Left`/`Right` to move by a visible character (including combined Unicode characters).

An ordinary launch opens the [session hub](#session-hub) rather than any conversation; there is no built-in default session.

## Context usage

The status bar's `ctx ~57k/1M (5.7%) tok` pair is separate from the running
prompt/completion totals. The numerator is the **host agent's final LLM call's
reported prompt tokens**, not the sum of every tool-loop call.
`~` marks an estimate of current occupancy: the last call does not include
subsequent messages or a freshly rebuilt system prompt.
Chaz does not manufacture a count from a percentage or locally estimate a
missing provider count. `unknown` means no usable count is available (including
missing/zero usage or a last response from a different model).

Counts use rounded `k`/`M` suffixes, omitting a redundant `.0`.
The percentage is calculated from the unrounded counts and shown only when
both the count and budget are known; it can exceed 100% after lowering a cap.

The denominator is the runtime's effective **input** context budget, not the
output-token cap. Configured model windows take precedence over discovered
windows; an explicit agent cap can lower the budget. When the model window is
unknown, the configured input-budget fallback still applies. A zero budget is
shown as `unknown`. The bar refreshes model defaults, learned windows and caps
on redraw. Long session names are clipped to keep the pair visible.

For example:

1. Select a model advertising a 1,050,000-token window via `/models` → select a scope → `Enter`.
   Before a response, the bar shows `ctx unknown/1.1M tok`.
2. Send a message. If the final call reports 12,345 prompt tokens, it shows
   `ctx ~12.3k/1.1M (1.2%) tok`, even if the turn's accumulated prompt total is larger.
3. Switch to a 32,000-token model. Until that model replies, the old model's
   count is not reused: `ctx unknown/32k tok`.
4. If the provider omits usage, it remains `unknown`; a later response with
   usage restores the numerator. An agent cap of 16,000 changes the denominator
   to `16k`, including after reopening the session.

## Commands

The TUI catalogs every built-in slash command in its inline completion popup — type `/` to open it, `Tab` / arrow keys to navigate, `Enter` to insert. `F1` or `/help` shows the same catalog as a scrollable overlay. The list below is the same one rendered there, grouped the same way.

### Session

| Command           | Description                                                            |
| ----------------- | ---------------------------------------------------------------------- |
| `/help`, `/?`     | Open the help overlay (also `F1`)                                      |
| `/sessions`, `/s` | Open the session hub (also `Ctrl+P`)                                   |
| `/new`            | Create a new session and switch to it                                  |
| `/new <group>`    | Create a new session with a named agent group attached                 |
| `/groups`         | List the configured agent groups                                       |
| `/join <ref>`     | Switch to a session by name or eidetica DB ID                          |
| `/name <alias>`   | Set a human-friendly alias for the current session (also `/rename`)    |
| `/name`           | Clear the session alias                                                |
| `/info`           | Show current session details (name, DB ID, entry counts)               |
| `/costs`          | Aggregate LLM usage and cost across all sessions ([details](usage.md)) |
| `/interrupted`    | List turns that require an explicit retry decision                     |
| `/retry <id>`     | Ask the executor to retry the currently observed interrupted attempt   |
| `/channels`       | List Matrix rooms currently attached to this session                   |
| `/share`          | Generate a shareable ticket URL for the current session                |
| `/sync <ticket>`  | Sync a remote session via a ticket URL                                 |
| `/compact`        | Ask the executor to compact the current immutable session snapshot     |
| `/print`          | Dump the transcript                                                    |
| `/stop`           | Stop the active turn in this session; idle sessions are unchanged      |

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

| Command                                                   | Description                                               |
| --------------------------------------------------------- | --------------------------------------------------------- |
| `/schedule list`                                          | List an agent's schedules                                 |
| `/schedule add <id> <cron> <agent> <task...>`             | Add a schedule (6-field cron: `sec min hour dom mon dow`) |
| `/schedule add interval <id> <seconds> <agent> <task...>` | Add a fixed-delay interval schedule                       |
| `/schedule modify <id> cron <6 fields> [agent]`           | Replace a schedule trigger with cron                      |
| `/schedule modify <id> interval <seconds> [agent]`        | Replace a schedule trigger with an interval               |
| `/schedule remove <id>`                                   | Remove a schedule by id                                   |

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
| `/settings`            | Open [Session Settings](#settings) (also `Ctrl+S` in chat)    |
| `/debug`               | Toggle debug mode (also `Ctrl+D`)                             |
| `/quit`, `/q`, `/exit` | Exit                                                          |

Unknown `/<name>` commands route to extension dispatch — see the error you get back for the closest match.

## Key Bindings

| Key                   | Action                                                                |
| --------------------- | --------------------------------------------------------------------- |
| `Enter`               | Accept highlighted completion (if extending); else send / execute     |
| `Tab` / `Shift+Tab`   | Open completion popup and cycle highlighted entry                     |
| `Up` / `Down`         | Move completion selection if popup is open; else scroll history (3)   |
| `PageUp` / `PageDown` | Fast scroll history (20 lines)                                        |
| `Home` / `End`        | Move cursor to start / end of input                                   |
| `Alt+Enter`           | Insert a line break in the draft instead of sending                   |
| `Esc`                 | Dismiss overlay/completion first; otherwise stop busy turn, quit idle |
| `F1`                  | Open the help overlay                                                 |
| `Ctrl+P`              | Open the session hub (in the hub: back to the open conversation)      |
| `Ctrl+D`              | Toggle debug mode                                                     |
| `Ctrl+W`              | Close the active view; the last one returns to the hub                |
| `Ctrl+H` / `Ctrl+L`   | Cycle to previous / next tab (wraps, chat only)                       |
| `Ctrl+S`              | Open Session Settings in chat; Peer Settings in the hub               |
| `Ctrl+C`              | Quit                                                                  |

Approval prompts accept `y` to approve, `n` to deny, and `a` to approve all remaining tool calls for this turn. `Esc` and slash commands remain available to stop the turn; `Ctrl+C` still quits.

### Mouse

Mouse capture is enabled. Click on completion rows, help-overlay command rows, approval buttons, hub and picker rows, tab titles, or tab close `×` widgets to act on them. The scroll wheel scrolls the help overlay when it is open; in the session hub and model picker it moves the selection three rows. In Settings, it selects over the visible list rows and reads over the visible content body (see [Reading Settings details](#reading-settings-details)); headers, the category rail, and the status strip swallow it. Other overlays swallow it; otherwise it scrolls history.

## Debug Mode

Toggle with `Ctrl+D` or `/debug`. When active:

- Every entry shows its timestamp and type (e.g., `[10:30:00 Message]`)
- Tool result previews expand from 120 to 500 characters
- The status bar shows `DEBUG`

This is useful for understanding the session entry flow, correlating with log output, and debugging agent behavior.

The `/raw` command provides an even more detailed dump: every entry's index, timestamp, type, sender, and content in a tabular format.

## Session Hub

The session hub is the TUI's home: the list of this peer's sessions plus a pinned **New session** row.

```text
+--[ Sessions ]-------------------------------------------+
|                                                         |
| > + New session                                         |
|                                                         |
|   "work" * [tui] default • 2h ago                       |
|                                                         |
|   …3f9a1c0d [matrix] default • 1d ago                   |
|                                                         |
+---------------------------------------------------------+
 ↑↓ select · Enter open · n new · r rename · s peer settings · Ctrl+C quit
```

| Key / action     | In the hub                                                                                      |
| ---------------- | ----------------------------------------------------------------------------------------------- |
| `↑` / `↓`, wheel | Move the selection (the wheel moves three rows)                                                 |
| `Enter`, click   | Open the selected session, or create one on **New session** (click selects, second click opens) |
| `n`              | Create a session and open it                                                                    |
| `r`              | Rename the selected session                                                                     |
| `s` / `Ctrl+S`   | [Peer Settings](#settings); `Esc` there returns to the hub                                      |
| `Esc`            | Back to the open conversation; stays in the hub when none is open                               |
| `Ctrl+P`         | Back to the open conversation; reload when the list failed or no conversation is open           |
| `Ctrl+C`         | Quit                                                                                            |

The footer shows the subset of these keys that fits the terminal, and `Esc back` only when there is a conversation to return to.

### How the hub, sessions and views relate

A **session** is stored data: its transcript, roster and name live in the peer's database whether or not the TUI shows it. A **view** is a tab showing one session in this TUI process. The hub is the base view underneath every tab.

- **Launching creates nothing.** `chaz` opens the hub even on an empty install; you create a session explicitly with **New session** or `n`, which stores it and opens it at once. The launch prompt and `--session` forms in the [launch table](#tui-mode) are the only other ways a launch creates one.
- **Opening is not creating.** Selecting a session that already has a view focuses that view instead of adding a duplicate.
- **Closing a view is local.** `Ctrl+W` or `×` removes the view only. The session keeps its history, agents and bridge attachments, and an agent already working in it carries on. Closing the last view returns to the hub; reopen the session from there.
- **Drafts follow their view.** `Ctrl+H` moves left to the previous tab and `Ctrl+L` moves right to the next, wrapping at either end. Unsent text and the cursor stay with each conversation when you switch tabs, visit the hub, or open Settings, as does each view's scroll position. Closing a view discards its draft.
- **Settings scope is explicit.** From the hub, `s` opens Peer Settings, which needs no open conversation. Session Settings exists only for an open conversation and is titled with its name.
- **The list is local.** Rows come from this peer's session catalog: TUI sessions, Matrix-attached ones, `spawn_agent` / `spawn_worker` children, and anything synced so far. It is not an inventory of every remote peer. Agent names load only for visible rows; a row shows `…` until then. `*` marks the focused conversation.
- **A session named `tui` is ordinary.** Earlier versions opened a session named `tui` on every launch. Existing ones stay as they are and appear in the list like any other session.

The list reads registry metadata only: root ID, name, bridge, agent, age and status. It never scans transcripts for counts, previews or costs, and `/costs` scans usage explicitly. The catalog loads in the background, so navigation, New and Peer Settings work while it loads. _Loading sessions…_, _No saved sessions yet_ and _Failed to load sessions: … — Ctrl+P retries._ are distinct states; a failure is never shown as an empty list. A failed open or create stays in the hub with the error in the footer until the next key.

### Walkthrough: from the hub and back

1. On a fresh install, run `chaz --config config.yaml`. The hub shows only **New session**, followed by `No saved sessions yet — select "New session" above.` Nothing has been stored.
2. Press `n`. A conversation opens with an empty composer. Its tab bar reads ` …3f9a1c0d ×  Ctrl+P sessions · Ctrl+S settings · Ctrl+W close`. Send a message and leave a half-typed reply in the composer.
3. Press `Ctrl+P`. The hub lists the new session marked `*`. Press `Esc` to return; the half-typed reply and cursor are where you left them. To open another tab, press `Ctrl+P` then `n`. Use `Ctrl+H` to return to the first draft and `Ctrl+L` to switch right again; close the second view with `Ctrl+W` to return to the first.
4. Press `Ctrl+W`. The view closes and the hub returns, still listing the session; its history is intact. Select it and press `Enter` to reopen it.
5. Press `s` in the hub, read a Peer Settings page, and press `Esc` to come back to the hub. No conversation needed to be open.
6. Quit with `Ctrl+C` and run `chaz --config config.yaml --session work "draft the agenda"`. A session named `work` is created and opened, with `draft the agenda` waiting unsent. Run the same command again: `work` reopens, and no second session appears.
7. Failure: if the session index names `work` but its database can no longer be opened, chaz neither opens another conversation nor creates a replacement:

   ```text
   Error: Failed to open session 'work'

   Caused by:
       …
   ```

   Inside the hub, a stale row that cannot be opened behaves the same way: the footer shows `Failed to switch session: …` and you stay in the hub.

## Settings

Settings has two separate scopes: **Peer** covers this process's hosted agents and configuration; **Session** covers the active conversation's roster and model overrides. A per-agent Session model override does not change that agent's DB default. Peer → Agents can change the DB default, which applies wherever a session override does not win.

| Entry path                         | Opens                                                  |
| ---------------------------------- | ------------------------------------------------------ |
| `Ctrl+S` in chat or `/settings`    | Session Settings                                       |
| `/models` in chat                  | Session Settings → Models, with the scope list focused |
| `Ctrl+S` or `s` in the session hub | Peer Settings (no open conversation needed)            |

`Ctrl+S` does nothing while Settings or the model picker is already open. `Ctrl+H` and `Ctrl+L` cycle tabs only in chat, so Settings and pickers retain their conversation caller. Plain `PageUp`/`PageDown` still scroll. Settings remembers its caller: `Esc` returns to chat or the session hub, rather than quitting. At normal terminal sizes the category rail stays on the left and the current page on the right. Navigation starts at **Category**, then enters **List → Content** (static/empty pages skip List); see [Reading Settings details](#reading-settings-details) for all keys and pointer controls. Number keys follow the sidebar order below (`1`–`9` for Peer, `1`–`6` for Session).

A model picker returns to its Settings page on selection or `Esc`; canceling writes no model. Add prompts, add-agent pickers and YAML diffs handle `Esc` first, so cancel them before leaving Settings. Exiting Settings does not undo edits already applied.

### Shipped pages and controls

These tables list every category in sidebar order. Read-only pages are inspectors, not editors waiting for an apply key. The six **coming soon** pages are placeholders and have no settings controls.

| Peer category | Current behavior                                                                                                                                                                                                                                                                                                                                                                |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Agents        | Inspect hosted agents and selected-agent details. In List or Content, `Enter` picks the agent's DB-default model. `r` opens a YAML diff: `r` applies the additive drift merge, `R` requests a full reseed with confirmation, and `a` lets you pick fields (`Space` toggles, `Enter` applies).                                                                                   |
| Backends      | Read-only backend names, API base URLs, and configured/known model counts; no credential editor.                                                                                                                                                                                                                                                                                |
| Defaults      | Edit the ordered agents attached to new sessions; the first is the routing host. `a` opens an add prompt, `d` removes the selected row, and `Ctrl+↑` / `Ctrl+↓` reorder it. Changes persist in the peer DB, not YAML, and do not change existing session rosters. Accepted edits apply in order to the latest defaults; a pending add is not lost by a later remove or reorder. |
| Bridges       | Read-only status: TUI active, CLI available, Matrix/Discord external binaries.                                                                                                                                                                                                                                                                                                  |
| Extensions    | **Coming soon.**                                                                                                                                                                                                                                                                                                                                                                |
| MCP           | Read-only server list and selected-server status, tools, and failure details.                                                                                                                                                                                                                                                                                                   |
| Groups        | **Coming soon.**                                                                                                                                                                                                                                                                                                                                                                |
| Identity      | **Coming soon.**                                                                                                                                                                                                                                                                                                                                                                |
| About         | Read-only version, state directory, in-process bridges, and configuration/count summary.                                                                                                                                                                                                                                                                                        |

| Session category | Current behavior                                                                                                                               |
| ---------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| Overview         | Read-only active-session identity, creation time, entry count, attached-agent count and host, current agent, and effective model.              |
| Agents           | Inspect the attached roster. `a` opens a filtered picker of hosted agents not already attached (`Enter` adds); `d` removes the selected agent. |
| Models           | Edit the session-wide pin or a per-agent session override via the [model picker](#model-picker).                                               |
| Routing          | **Coming soon.**                                                                                                                               |
| History          | **Coming soon.**                                                                                                                               |
| Sharing          | **Coming soon.**                                                                                                                               |

### Example: session override, then peer inspection

1. In chat, type `/models`. The Models page opens with `Session` selected. Press `↓` to select an attached agent's row, then `Enter`; the picker title names that agent's scope.
2. Type to filter models and press `Enter` to apply one. You return to Models with a per-agent override for **this session only**. To back out instead, press `Esc` in the picker: you return to Models with no model change. Press `Esc` on Models to return to chat.
3. Open the session hub with `Ctrl+P`, then press `s` (or `Ctrl+S`). This opens **Peer**, not the highlighted session's Settings. Select Backends (key `2`) to inspect configured backends without editing them. Press `Esc` to return to the session hub, then `Esc` to return to chat. Merely browsing either scope does not write settings.

### Reading Settings details

#### Keys and pointer

| Input                        | Category focus                                                                                        | List focus                                                                                  | Content focus                                          |
| ---------------------------- | ----------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- | ------------------------------------------------------ |
| `Up` / `Down`                | Cycle categories; wraps                                                                               | Select an entity or model scope; wraps                                                      | Read one wrapped line; clamps                          |
| `Right`                      | Enter a nonempty list, otherwise Content                                                              | Enter the body on Peer Agents/MCP; otherwise no-op                                          | No-op                                                  |
| `Left`                       | No-op                                                                                                 | Return to Category                                                                          | Return to the list, or Category on a static/empty page |
| `PageUp` / `PageDown`        | No-op                                                                                                 | No-op                                                                                       | Read a page, with one line of overlap                  |
| `Home` / `End`               | First/last category                                                                                   | First/last category                                                                         | Beginning/end of the body                              |
| `Tab` / `Shift+Tab`, `1`–`9` | Cycle/jump categories                                                                                 | Same, returning to Category                                                                 | Same, returning to Category                            |
| `Enter`                      | Enter a list/static body; Session Models opens its picker                                             | Peer Agents sets the selected agent's default model; Session Models opens its scoped picker | Peer Agents keeps that model action; otherwise no-op   |
| `Esc`                        | Return to the caller                                                                                  | Same                                                                                        | Same; use Left to leave only the reader                |
| Click an entity row          | Select it and focus List                                                                              | Same                                                                                        | Same                                                   |
| Click the body               | Focus Content without moving selection or reading position                                            | Same                                                                                        | Same                                                   |
| Wheel over list/body         | List: select three entities, clamped. Body: read three wrapped lines, clamped. Focus does not change. | Same                                                                                        | Same                                                   |

Action letters such as `a`, `d`, and `r`, and Defaults' `Ctrl+Up`/`Ctrl+Down`
reordering keep their existing selected-row targets. Prompts, pickers, diffs,
and approval input take precedence over the reader. A pending tool approval
shows the same request and buttons as chat, preserving any Settings draft and
reading position until it is answered. Wheeling or clicking a covered Settings
list cannot move its selection or the hidden chat.

#### Selection is not reading position

An agent's workers are decorative list rows, not selectable agents. If the
whole agent-and-workers group fits the list's current row budget, it stays
expanded. Otherwise its agent row shows `[20 workers → details]` (with the
actual count). The existing body still contains every worker's fields.

When a body overflows, a pinned row identifies the selected entity and shows
its visible wrapped-line range, such as `DETAILS [1–14/131] · alpha`.
`DETAILS` is highlighted while Content owns the keys. Scrolling this body
never changes the selected agent/server or a model action's scope.
System prompts remain deliberately abbreviated previews, not full-prompt viewers.

There is one reading position. Changing the category or selected entity,
including a catalog replacement at the same index, resets it to the beginning;
leaving Settings resets it too. Focus changes and opening/canceling a model
picker or diff preserve it. Resizing rewraps the body and clamps the numeric
wrapped-line offset. It does not preserve a semantic text anchor or remember
separate positions for previously visited entities. A body that fits again
returns to its ordinary, unscrolled layout.

#### Read an overflowing worker group

1. In an 80×24 terminal, open Peer Settings → Agents and select an agent
   `alpha` with twenty worker templates. Its row shows:

   ```text
   > [20 workers → details] alpha
   ```

   Shorter groups that fit still show their nested `└` rows.

2. Press Right to focus List, then Right again for Content. For short field
   values, the pinned row reads:

   ```text
   DETAILS [1–14/131] · alpha
   ```

   Use Down or PageDown to read. End exposes the final workers' model,
   spawn-depth, tools and prompt-preview fields; `alpha` remains selected:

   ```text
   DETAILS [118–131/131] · alpha
   ```

3. If the wheel seems inactive over the pinned row or a list header, move it
   into the body below the pinned row. Headers swallow wheel events; blank
   body cells still scroll. Move over the list only when you intend to select
   a different agent, which resets reading to Home.
4. Resize to 120×40. The same agent and focus remain selected; the visible
   range is remeasured and the offset clamps to the new last page if needed.
   Use Home to return to the first fields. Left returns to List; Esc returns
   to the session hub. Reading itself writes no settings.

### Adding an agent from Settings

In Session Settings → Agents, press `a` to open the add-agent picker.
Typing filters candidate names (case-insensitive substring); `↑`/`↓` selects,
`Enter` adds the highlighted agent, and `Esc` cancels without changing the roster.
The filtered selection remains the action target even when the list scrolls or
resizes. The wheel moves three candidates only over the picker's match rows;
background category/list clicks are ignored while the picker owns input.

For example:

1. With more candidates than visible rows, move down to the final candidate.
   The list scrolls to show `> candidate-29`; `Enter` adds that candidate,
   not a row from the previous visible page.
2. Type a filter with no matches. The picker shows `(no matches)` and `Enter`
   adds nothing. Reopen with `a`, or remove the filter with `Backspace` before
   accepting, or press `Esc` to cancel.

## Named Sessions

Give sessions human-friendly names instead of opaque IDs:

```text
/name daily-standup
```

Named sessions can be referenced anywhere a session identifier is accepted:

```text
/join daily-standup
```

The name appears in the status bar, session hub, and `/info` output. Names must be unique across all sessions. Use `/name` (with no argument) to clear the name.

### Metadata edits need readable stored state

Session metadata edits, including `/name`, `/model` and agent/settings changes,
first read all existing metadata. A storage failure or malformed field refuses
the whole edit, even if it targets another field. Missing fields are allowed;
unreadable fields are not treated as empty or reset to defaults.

This also applies to bridge participation settings and runtime, agent and job
metadata updates. On a transport failure, restore the connection before editing
again. Malformed stored metadata stays write-blocking; reopening the TUI or
retrying another edit does not repair it. There is no automatic reset policy.

## Model Picker

Pick a model for a specific scope — the whole session, or one agent in this session. `/models` opens Session Settings → Models, where each row is a scope you can edit:

```text
+--[ Models ]----------------------------------------------------+
|                                                                |
| > Session         claude-opus-4-7                              |
|       resolves to claude-opus-4-7                              |
|                                                                |
|   Per-agent overrides                                          |
|   chaz            (uses session pin)                           |
|   researcher      openai/gpt-5-mini                            |
|                                                                |
|   Enter — open picker for selected scope                       |
+----------------------------------------------------------------+
```

- **`Session`** (row 0) — the session-wide pin (`SessionMeta.model`, what `/model <id>` writes). Every agent uses this unless its own row sets an override.
- **`<agent>`** — per-agent override for that agent (`SessionMeta.agent_models[name]`, what `/model <agent> <id>` writes). Falls back to the session pin when unset.

`↑` / `↓` (or click) selects a row. Overflowing Settings lists keep the selected row visible. Defaults, Session Agents, and Models reserve room for their footer hints when space permits; in a short pane, selectable rows take precedence over hints. `Enter` opens the picker locked to that row's scope:

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

There is no global key binding for `/models` — terminals without the keyboard-enhancement protocol can't distinguish `Ctrl+M` from `Enter`, which made any natural binding unreliable through `tmux + ssh`. Type `/models` to open. The same limitation is why a draft line break is bound to `Alt+Enter` rather than `Shift+Enter`: only terminals that negotiate the enhanced protocol report the `Shift`, which is also accepted where it arrives.

## Live turn activity

| Display                            | Source                            | Clears when                                                       |
| ---------------------------------- | --------------------------------- | ----------------------------------------------------------------- |
| `thinking...` in the messages area | Unexpired executor per-turn start | The attempt completes (including errors) or its heartbeat expires |
| `_agent_ acknowledged turn`        | Historical Ack entry              | Never a live indicator; remains in history                        |

The TUI watches the shared session database, not its own submitted-message state.
A remote executor's start and completion arrive through sync, so a tab opened
mid-turn shows the same indicator. A crashed executor cannot leave it visible
forever: without a heartbeat, the TUI clears it within 45 seconds, checked
on its five-second refresh. The runtime's ownership of the session is not a
turn claim; an idle session shows no activity. An interrupted turn requires
`/interrupted` and an explicit `/retry` before another attempt can start.

For example, after sending `@chaz:example` a question in a shared Matrix room:

1. Open that session in the TUI. During the executor's turn, the bottom of the
   messages area shows `thinking...` even though the prompt was not typed here.
2. After the reply, `thinking...` disappears; an older `chaz acknowledged turn`
   line may remain as history. On an error it also disappears.
3. If the executor exits before completion, `thinking...` clears after the
   heartbeat expires. `/interrupted` then lists the interrupted request;
   `/retry <request_id>` starts a new claim after you decide retrying is safe.

## Responsiveness and background refresh

The TUI draws cached state and handles local input independently of backend
work. Enter accepts an action into an ordered queue; it does not wait for a
session read, command, or message write. Settings opens from cached metadata,
and opening a stored session leaves the hub usable while its data loads.
Each tab owns its transcript, name/models, extension strip and live-turn count.
A completion addresses the requesting tab incarnation, never whichever tab
happens to be focused. Closing and reopening a tab cannot apply its old results.
A delayed open does not take focus after newer local navigation. Ordinary opens
resolve their destination before installing observation: `/join` to an open
tab reuses its live approval receiver. New destinations finish observation
setup before exposing their composer. Jobs opened from either Jobs or the hub
remain authorized observers, never ordinary execution sessions.

| Trigger / display                      | Behavior                                                                          |
| -------------------------------------- | --------------------------------------------------------------------------------- |
| Session write / Settings open          | Coalesced background transcript/meta/status refresh                               |
| Every five seconds                     | Refresh activity; check service health and retry failed reads                     |
| `⟳ refreshing (cached)`                | A read is pending; displayed data is the last successful read                     |
| `⚠ refresh failed`                     | That read failed; last-good data remains, even if another read succeeds           |
| `action(s) pending`                    | Accepted backend actions execute in order, one at a time                          |
| Queue full or backend disconnected     | New backend actions are not accepted; composer/prompts stay editable              |
| `Write still pending; outcome unknown` | A deadline warns, but does not cancel, retry or advance past the write            |
| Quit with unfinished actions           | Esc (without popup), Ctrl+C and `/quit` warn first; repeat to abandon queued work |

### Cached is not confirmed

The first quit with pending work shows a top-of-screen `Quit again to exit`
warning in every mode, including Settings editors, model selection and overlays.
Drafts remain intact and visible; a second explicit quit confirms exit. An open
completion popup still consumes the first Esc without arming quit.

Reads have a bounded wait and supervised panic/cancellation handling. A failed
read releases its slot for recovery without clearing cached data or restoring
an otherwise live terminal. Errors are tracked separately for transcript/meta,
status and activity; a successful activity poll cannot hide a failed transcript
read. Catalog failures likewise retain prior rows and mark their metadata stale.
Errors are logged at `warn`; worker panics are logged at `error`.

Writes are different. A transport failure, timeout or cancellation cannot prove
that a message or command did not commit. The TUI never automatically retries
an accepted action. A slow write stays owned and later accepted actions remain
queued behind it. Errors report an unknown outcome rather than success; check
persisted state before deciding to resubmit. Unconfirmed accepted input is kept
and reported after terminal restoration on exit. Queued actions abandoned by
an explicit exit were not started; a started write may already have effects.

On an actual service disconnect, the TUI keeps the cached view and draft. Once
accepted actions settle, it tears down the old runtime and reconnects through
the normal connector using the same configured login and execution role.
Successful recovery reopens the same session IDs and refreshes their caches;
it does not replay submitted messages or commands. Handle reopening is separate
from hydration: a failed transcript read retains cached history while status can
refresh independently. A tab that could not reopen retries on the healthy runtime
without restarting other tabs. That tab refuses backend writes until its handle
is available, but its cached view and draft remain usable. Admission/reconnect
notices clear on acceptance, completion or recovery; unresolved write outcomes
remain visible independently. A slow health read alone is not grounds to
replace a runtime. Configuration/login errors are displayed, not retried
indefinitely.

Pending keypresses and wheel notches are handled in order, but can draw as one
frame. Pointer motion is ignored without redrawing. Rendering reuses wrapped
transcripts until they change and draws the visible rows rather than rebuilding
all history for each keystroke.

### Walkthrough: a slow backend, then a lost connection

1. In a conversation, submit a message and keep typing a second draft. The
   first action may remain pending while the second draft and local scrolling
   stay usable. Open Settings with Ctrl+S, or the hub with Ctrl+P; browsing cached
   data does not wait for the first write. If the write remains slow, the status
   reports:

   ```text
   Write still pending; outcome unknown. Not cancelled or retried; later actions remain queued
   ```

2. If a refresh fails, the transcript/name/status do not become empty. The bar
   shows `⚠ refresh failed`. An actual service disconnect reports:

   ```text
   Backend disconnected — cached data is stale; reconnecting after accepted actions settle. No writes replayed
   ```

   New backend submissions are refused while disconnected, but the existing
   draft remains editable. Do not assume a previously submitted message failed
   to commit just because the service went away.

3. When the service returns, the same views refresh and the draft survives.
   The disconnect notice clears; `⟳ refreshing (cached)` remains while a read
   is pending. If only transcript hydration fails, `⚠ refresh failed` and the
   cached transcript remain even after the extension status becomes fresh.
   A later successful read clears that read's error without replaying a write.

   Check the transcript or persisted command result before explicitly retrying
   any unconfirmed action. If you instead exit while work is pending, the first
   quit request warns; repeating it restores the terminal and reports queued
   versus potentially committed work, including unconfirmed accepted input.

## Entry Types

The TUI renders different entry types with distinct styles:

| Type       | Appearance                             | Description                                                    |
| ---------- | -------------------------------------- | -------------------------------------------------------------- |
| Message    | **Bold colored sender** + content      | Chat messages from users and agents                            |
| Directive  | **Bold sender (directive):** + content | Task instructions (from spawn_agent / spawn_worker, scheduler) |
| Ack        | Dimmed "_agent_ acknowledged turn"     | Historical audit entry; not live activity                      |
| ToolCall   | Dimmed `> tool_name(args)`             | Agent invoked a tool                                           |
| ToolResult | Dimmed `< tool_name: output`           | Tool returned a result                                         |
| Error      | Red `ERROR sender: message`            | An error occurred, or a turn was stopped                       |
| Cancel     | Dimmed `⏹ stop requested by sender`    | A participant asked the running turn to stop                   |

Senders are color-coded: agents in magenta, users in cyan, system in yellow. Green is the UI accent, not the agent sender color.

## Stopping a Running Turn

Press `Esc` while a turn is running, or send `/stop`.
The live activity line shows `thinking... Esc stop · Ctrl+C quit`.
Overlays and the completion popup dismiss on the first `Esc`; press it again to stop.
`Ctrl+C` quits, including while the agent is busy.

The frontend writes a `Cancel` event into the session with the active request and attempt IDs.
The executor observes that event through the ordinary session-write path, then interrupts its model call, approval wait, or tool execution.
The frontend does not abort the executor task itself.
A synced client uses the same event path; Matrix exposes it as `!chaz stop`.

The event targets only the attempt visible when you ask to stop.
Delayed or repeated events cannot stop a later turn, including a retry of the same request.
When no active attempt is visible, `/stop` returns `No running turn to stop.` without recording an event.
Queued requests are not cancelled.
Other sessions and already-started child sessions keep running; stop those from their own session.
Session write permission is required, and the executor ignores agent-authored or malformed stop events.

Every recorded native tool call gets a result, including calls in the batch that had not started: `Tool execution cancelled by user`.
The completed attempt has a terminal cancellation record and a visible agent `Error`: `Stopped: the turn was cancelled at the user's request.`
It retains usage from completed model calls for `/costs`, and does not produce a final reply.
If transcript persistence fails, the attempt remains interrupted rather than claiming a successful stop.

On Unix, stopping a native shell call kills its process group, including ordinary descendants.
Completed side effects are not rolled back, partial shell output is not recovered, and descendants that deliberately escape the group are outside this cleanup.
The executor must have the session registered and receive its synced event before it can stop remote work.
Matrix acknowledges the command but does not post the local terminal `Error` to the room.

### Walkthrough

1. Ask for slow shell work and approve it with `y`:

   ```text
   user:
     run sleep 20, then tell me it finished

     ▸ shell {"command":"sleep 20"}
     thinking... Esc stop · Ctrl+C quit
   ```

2. Press `Esc`, or send `/stop`. With a popup open, dismiss it first.
   The shell stops and the transcript shows:

   ```text
   system:
     Stop requested for the active turn.
     ⏹ stop requested by user
     ✗ shell Tool execution cancelled by user
     ERROR chaz: Stopped: the turn was cancelled at the user's request.
   ```

3. Send another message. The earlier event cannot target this new attempt:

   ```text
   user:
     never mind, just say hi

   chaz:
     Hi!
   ```

4. If you send `/stop` after the reply, nothing is running to stop:

   ```text
   system:
     No running turn to stop.
   ```

   Send the next message normally; there is no stop event to clear.
   Bare `Esc` while idle still quits the TUI.

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
To stop instead, press `Esc` or send `/stop`; a cancelled approval cannot approve a later tool call.

## Testing the terminal lifecycle

Contributors can run `nix develop .# -c just tui-pty` on Linux with `uv` available.
This drives the real TUI under a narrow PTY with a disposable database and a
loopback stub backend: Unicode input, final reply rendering, resize, Esc and
/stop during a blocked model call, overlay/completion-first dismissal, pending
approval and native shell cancellation, next-turn recovery, and Ctrl+C quit
while busy with terminal restoration.
It also checks that missing output fails on both early exit and timeout, and
that children are reaped.

It also stops a disposable IPC owner while Enter, Settings and hub actions
are pending, verifies ordered commands and cache/draft retention across an
actual service stop/restart, and exercises read timeout/panic under a controlling
PTY without losing terminal ownership. This complements the widget and executor tests; it does not exercise
production providers or assert rollback of effects or cancellation of durable
child jobs.
See `dev/tui-pty/README.md` for isolation details and failure artifact locations.

## Jobs: observe without taking over

`/jobs` (or `Ctrl+G`) opens the local Jobs monitor. It follows the Agent DB's
stable per-executor references, then reads the job Session DB for attempt and
result evidence. A projection row is discovery, not execution authority.

Local v1 requires **existing Write authority** on the observed databases;
Admin is also supported, but not required. This includes the Agent/executor
sources and job sessions. The observer UI is non-executing, **not a Read-only
credential boundary**. The current Eidetica service needs Write to materialize
native record caches when reading. True Read-only observation is deferred
until that upstream path is repaired and verified; it is not supported yet.
Chaz never upgrades a key, grants access, or substitutes a privileged credential.
A delegated service session whose native Write query is unsupported also stays
explicitly unavailable; the monitor does not guess its authority.

| Existing authority | Local-v1 behavior                                       |
| ------------------ | ------------------------------------------------------- |
| Write or Admin     | Observe; steer only an active attempt within its limits |
| Read only          | Explicit insufficient-permission outcome                |
| Unauthorized       | Refuse access; a row or job handle grants nothing       |

| Control                         | Action                                                                |
| ------------------------------- | --------------------------------------------------------------------- |
| `↑` / `↓`                       | Select an ancestry node and preview its task, activity and result     |
| `Enter`                         | Open a job in an observe-only conversation tab                        |
| `PgUp` / `PgDn`, `Home` / `End` | Read the selected task or its diagnostics                             |
| `d` / `F2`                      | Toggle technical details in Jobs (IDs, source errors, wait deadlines) |
| `F2` in a job tab               | Toggle observer/input identifiers and diagnostics                     |
| `r`                             | Refresh the overview without registering an executor                  |
| `Esc`                           | Return from the overview                                              |
| `Ctrl+G`                        | Toggle between chat and Jobs                                          |
| `Ctrl+W`                        | Close the current tab; the resident job continues                     |
| Job composer                    | Publish authenticated input for the same attempt's next model call    |

### Read tasks first, inspect identifiers when needed

The tree uses task titles, preserves ancestry and sibling order, and highlights
the selected row in bold. Its second line names the recorded status and Agent.
The header counts only jobs in the executor projection; **Context** rows
(originating conversations and references outside that projection) have a separate count. A completed job says
**Completed**, not “claimed.” Missing data says **Unavailable**, not “no jobs.”

Wide terminals show the tree and selected task side by side. Below 110 columns
they stack, leaving the detail pane the larger share. The detail starts with
the task, parent/children, recorded waits and terminal result, then recent
activity. Long tasks wrap; `PgUp`/`PgDn` read them without moving the tree
selection. `Home`/`End` jump to either end. `d` switches to IDs, executor/source
diagnostics and full recent records; press it again to return to the task.

**Started · recent activity** means activity was recently recorded, not that the
job is proven running. **Started · status uncertain** has no such recent evidence.
Wait labels come from entered, bounded `job_wait` calls, not model intentions.
**Wait recorded (unfinished)** is persisted evidence, not proof the execution is
still waiting. The freshness line shows snapshot age or a stale refresh failure;
unavailable sources stay visible. No progress, ETA or heartbeat is invented.
This view does not fetch remote databases or request access automatically.

Opening a job does not start work, adopt a runtime, or append another ordinary
turn. The tab follows persisted messages and tool activity. Job-local settings,
retry, cancellation and approval-policy changes are unavailable here.
Closing a tab or the client does not cancel the job or discard published input.

Jobs uses the same [background boundary](#responsiveness-and-background-refresh)
as ordinary conversations: selection, diagnostics, cached scrolling and drafts
stay local while refreshes or accepted actions run. Overview refreshes coalesce;
opening an observer and publishing steering use the ordered action queue.
Pending work and exit confirmation remain visible in both Jobs and observer tabs.
Reconnect reopens job tabs as observers, never as ordinary executing sessions.

### Steering an active turn

Chat and Jobs use the same next-call input path. The lifecycle differs:
idle Chat starts a new turn; a finished Job stays view-only and cannot change
the result already delivered to its parent. A Job tab is still an observer,
not another executor or a way to change its tools, grants or limits.

| Control / display             | Meaning                                                                     |
| ----------------------------- | --------------------------------------------------------------------------- |
| `Enter` while busy            | Publish steering for the observed active attempt                            |
| `Enter` in idle Chat          | Start an ordinary turn                                                      |
| `Alt+Enter`                   | Insert a newline; do not submit                                             |
| `> next-call input`           | The composer is targeting a started turn                                    |
| Gray input body below history | Published or selected, but not yet incorporated                             |
| Normal user text in history   | Incorporated in a model request after its projection and budget checks      |
| `F2`                          | Inspect request/attempt IDs and receipts, including inputs already promoted |
| Finished Job composer         | View-only; sending cannot reopen the job                                    |

The executor finishes the current model response and **every tool in its native
batch** before taking steering. It takes **all pending inputs together**, in
publication order, not UUID or timestamp order. Concurrent publications with
no causal order use the database's canonical entry order. Tools and approval
waits are not interrupted. Tool-free turns use the same boundary. A final text
or silence decision becomes provisional if input was accepted before closing;
the same attempt continues before publishing its terminal result.

The whole pending body is gray. Acceptance alone does not promote it. After
required context projection and budget checks, the host records incorporation
before invoking the backend. That moves the input into normal history after
the preceding assistant/tool exchange and before the next assistant reply.
Intermediate assistant text stays visible. Refresh and reopening reconstruct
that order from durable boundaries, not the time you typed the input.

Incorporation is a **local** fact, not proof that a remote model received or
understood anything. Receipts distinguish `Queued · published, awaiting
acceptance`, `Accepted · awaiting next call`, and `Accepted · dispatch recorded
for call N; inclusion unconfirmed`. `Included · call N returned` means a response
to that request was observed. `F2` retains those diagnostics after promotion.
A backend failure can leave promoted text with an uncertain receipt; it is not
silently replayed into a new attempt. A projection or budget refusal leaves
unincorporated text gray and reports `Not applied` when the attempt ends.

Closing a tab does not discard published input. A lost publication
acknowledgement retains the request ID for the client lifetime, including tab
close/reopen and reconnect. Resending the same text retries that ID; different
text is refused until the publication is resolved. A definite pre-publication
refusal recovers the text without retaining an uncertain ID. If the observed
attempt closes or is retried before publication, the input is refused, not
converted into an idle follow-up turn. Inspect the outcome, then explicitly
resubmit if you still want it. A completed Job cannot accept that resubmission.

Completed supported attempts preserve steering in later Chat context within
the normal history budget. A valid checkpointed `job_wait` restores already
incorporated input in place and drains newly pending input after the wait's
paired result. Only that observation resumes; earlier model calls and tools do
not run again. Other interrupted attempts require explicit retry, which does
not adopt their unconfirmed steering. Stopping while a steered call is in
flight leaves its receipt uncertain. Later Chat context excludes that
unconfirmed input but keeps earlier observed text and paired tool exchanges. No exactly-once external-effect guarantee
is implied.

Upgrade every executor, client, transport peer and history consumer sharing
a session **before** enabling these writers. Older tolerant readers preserve
the new records but cannot display promoted text, replay that attempt's
structured context or resume its checkpoint. Older executors do not consume
Chat steering. There is no automatic peer negotiation; upgrade an old reader
rather than rewriting history. See the [reader-first upgrade contract](../architecture/sessions.md#reader-first-wire-compatibility).

### Walkthrough: busy Chat and recovery

1. In Chat, send `Compare the two formats`. While the agent is busy, type
   `Use a small example`, press `Alt+Enter`, type `Show the trade-off`, then
   press `Enter`. The pending area shows both lines in gray:

   ```text
   Input 1 · Queued · published, awaiting acceptance
   Use a small example
   Show the trade-off
   ```

2. Send another steering message before the next boundary. The current
   response and full tool batch finish first. All pending messages then appear
   together as normal user text, followed by the assistant's continuation.
   There is no separately scheduled follow-up turn. Press `F2` while that
   response is in flight to inspect `Accepted · dispatch recorded for call 1;
inclusion unconfirmed`. A returned response changes the receipt to
   `Included · call 1 returned`. Call numbers are illustrative.

3. Close and reopen the tab. The provisional assistant text, promoted input
   and continued answer still appear once, in that order. Idle Chat can now
   accept an ordinary new turn.

4. If the attempt finishes before your queued publication runs, the action
   fails with `Input refused before publication` and recovers your text. It
   has not scheduled another turn. Read the final answer before pressing
   `Enter` again in idle Chat. If instead acknowledgement was lost, inspect
   persisted receipts and resend the **same** text to resolve the existing ID,
   rather than creating duplicate input.

5. If a required context projector refuses the continuation or its input
   exceeds the budget, the body stays gray with `Not applied`. A backend
   dispatch failure instead can leave normal-colored text with `Uncertain` in
   `F2` details. Neither outcome is confirmation of remote delivery or a reason
   to replay automatically. Recover the text and make an explicit next action.

### Walkthrough: steer, then inspect

1. Run a client-role TUI against the same configured Eidetica service as a
   resident executor (`execution: client` on the viewer), using existing
   Write-authorized credentials (Admin also works). Ask the executor's
   ordinary conversation to submit a local Agent job with `spawn_agent`.
2. Open `/jobs`. At 80 columns the tree sits above the detail. Select the task
   titled `Compare storage choices`; its row and detail heading say
   `Started · recent activity`. The preview names its parent and any recorded
   child waits. Press `d` to see the source/attempt IDs, then `d` to hide them.
   `PgDn` reads a long task; the tree selection stays put. Resize to 120 columns
   for the side-by-side layout. Press `Enter` to open the job tab. Its fixed
   banner says `Job observer` and `Write-capable · recorded, not proven live`.
3. Send `Use the smaller example first`. The receipt reads
   `Input 1 · Queued · published, awaiting acceptance`, then
   the body moves into normal history before the next response. `F2` shows
   `Input 1 · Accepted · dispatch recorded for call 1; inclusion unconfirmed`,
   and later `Input 1 · Included · call 1 returned`. Input/call numbers here
   are illustrative. `F2` reveals the request and attempt IDs; it does not send
   input. Close the tab with `Ctrl+W`: the resident executor still finishes.
4. Reopen Jobs. The row says `Completed` (or `Failed`), and the preview's
   `Result` (or `Outcome`) appears above recent activity. Open it to read the
   transcript and input receipts. The composer says `Finished job · view-only`.
   Sending again is refused; it cannot replace the result or start work.
5. With an independently Read-only credential, opening the job is refused:
   `insufficient permission: local-v1 job observation requires existing Write authority (Admin allowed); Read is unsupported`.
   An unreadable source stays explicitly unavailable; it is not an empty
   successful Jobs view. Return to the viewer's already Write-authorized
   credential to inspect it. Chaz does not grant or upgrade authority.
6. For another failure path, stop only the disposable viewer's access to its test
   service. A refresh displays `Stale · refresh failed/timed out` with the
   retained task and result still readable. Selection and `d` diagnostics remain
   usable; an observer retains its transcript, receipts and editable draft with
   an `Unavailable` banner. Restore access and press `r` to refresh. After an
   actual reconnect, the same observer and draft recover without another job turn.
   A stale start is not an invitation to replay: inspect the attempt before
   using the separate explicit retry workflow outside the job observer.
