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
| tui | ctx ~7.7k/128k (6.0%) tok | agent: default | model: gpt-5 | 1.5k/0.2k tok • 0% cached |
+--[ > ]----------------------------------------------+
| type here...                                        |
+-----------------------------------------------------+
```

The TUI has four main pieces:

1. **Tab bar** — one tab per open session. Click to switch, `[x]` to close, or use `Ctrl+PageUp`/`Ctrl+PageDown`. Closing the last tab is refused (the TUI always shows at least one session).
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
   when those modes are on.
4. **Input box** — type messages and commands. Slash commands open an inline completion popup with grouped categories; arrow keys move the highlight. The input wraps at the terminal edge and grows with the draft, capped so it never takes the whole frame (a long draft squeezes the transcript instead); past that cap the box scrolls to keep the cursor visible. Use `Alt+Enter` for a new line, `Enter` to send, and `Left`/`Right` to move by a visible character (including combined Unicode characters).

When prior sessions exist, the TUI opens straight into the session picker on launch so you choose which one to resume (or pick the "New session" row). A truly fresh state directory drops directly into the default `tui` session.

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
| `/sessions`, `/s` | Open the session picker (also `Ctrl+P`)                                |
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
| `Alt+Enter`                     | Insert a line break in the draft instead of sending                 |
| `Esc`                           | First press: dismiss completion popup. Second (no popup): quit      |
| `F1`                            | Open the help overlay                                               |
| `Ctrl+P`                        | Toggle the session picker                                           |
| `Ctrl+D`                        | Toggle debug mode                                                   |
| `Ctrl+W`                        | Close the active tab (refuses to close the last tab)                |
| `Ctrl+PageUp` / `Ctrl+PageDown` | Cycle to previous / next tab (wraps)                                |
| `Ctrl+C`                        | Quit                                                                |

Approval prompts hijack the keyboard while open: `y` approve, `n` deny, `a` approve all remaining tool calls for this turn.

### Mouse

Mouse capture is enabled. Click on completion rows, help-overlay command rows, approval buttons, picker rows, tab titles, or tab close `[x]` widgets to act on them. The scroll wheel scrolls the help overlay when it is open; in the session and model pickers it moves the selection three rows. In Settings, it selects over the visible list rows and reads over the visible content body (see [Reading Settings details](#reading-settings-details)); headers, the category rail, and the status strip swallow it. Other overlays swallow it; otherwise it scrolls history.

## Reading Settings details

Open Peer Settings with `s` in the session picker (`Ctrl+P`), or Session
Settings with `/settings` in chat. At normal terminal sizes, the category rail
stays on the left and the current page stays on the right.

### Keys and pointer

| Input                        | Category focus                                                                                        | List focus                                                                                  | Content focus                                          |
| ---------------------------- | ----------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- | ------------------------------------------------------ |
| `Up` / `Down`                | Cycle categories                                                                                      | Select an entity or model scope; wraps                                                      | Read one wrapped line; clamps                          |
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
and approval input take precedence over the reader. Wheeling or clicking a
covered Settings list cannot move its selection or the hidden chat.

### Selection is not reading position

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

### Read an overflowing worker group

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
   to the session picker. Reading itself writes no settings.

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
| > sha256:abc… "tui" * [tui] default • 2h ago       |
|                                                     |
|   sha256:def… [matrix] default • 1d ago             |
|                                                     |
|   sha256:xyz… [spawn] researcher • 4d ago           |
|                                                     |
+-----------------------------------------------------+
| [Up/Down] navigate | [Enter] select | [n] new | ... |
+-----------------------------------------------------+
```

Sessions are listed from registry metadata: eidetica DB root ID, human-friendly name, bridge, agent, age, and status. The picker does not scan transcripts for entry counts, previews, or costs; selecting a session loads that conversation on demand, while `/costs` explicitly scans usage data. Catalog loading happens in the background, and agent metadata is fetched only for rows visible in the picker viewport, so navigation and cancel remain available while storage is slow. Loaded metadata stays cached while the TUI is open. The "New session" row stays pinned above the scrolling list; selecting it retains the list's viewport and continues loading those visible rows. The picker shows every session the registry knows about: TUI, Matrix-attached, `spawn_agent` / `spawn_worker` children, and anything synced from remote peers. The current session is marked with `*`. Press `Enter` to switch, `n` to create a new session, or `Esc` to cancel.

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

## Entry Types

The TUI renders different entry types with distinct styles:

| Type       | Appearance                             | Description                                                    |
| ---------- | -------------------------------------- | -------------------------------------------------------------- |
| Message    | **Bold colored sender** + content      | Chat messages from users and agents                            |
| Directive  | **Bold sender (directive):** + content | Task instructions (from spawn_agent / spawn_worker, scheduler) |
| Ack        | Dimmed "_agent_ acknowledged turn"     | Historical audit entry; not live activity                      |
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

## Testing the terminal lifecycle

Contributors can run `nix develop .# -c just tui-pty` on Linux with `uv` available.
This drives the real TUI under a narrow PTY with a disposable database and a
loopback stub backend: Unicode input, final reply rendering, resize, and idle
Ctrl+C quit with terminal restoration.
It also checks that missing output fails on both early exit and timeout, and
that children are reaped.
This complements the fast widget snapshots; it does not test in-flight job
cancellation.
See `dev/tui-pty/README.md` for isolation details and failure artifact locations.
