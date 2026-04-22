# Agents

Chaz agents have persistent identity as _Living Agents_ — each agent is its own eidetica database signed by a per-agent key. Whoever holds the key hosts the agent. Sessions declare participating agents by listing their pubkeys in the session's AuthSettings; routing follows key possession.

YAML `agents:` config is the bootstrap path: at startup, chaz materializes one Agent DB per yaml entry (idempotent), populating its `config` and `meta` stores from the yaml. Existing yaml workflows keep working; the DBs are what travel with eidetica sync.

## Defining Agents (bootstrap via YAML)

```yaml
agents:
  - name: default
    role: chaz # System prompt (from roles section)
    max_iterations: 10 # Max ReAct loop iterations before forced summary
    allowed_tools: null # null = all tools, or list specific tools
    can_spawn: # Which agents this one can delegate to
      - researcher
      - coder

  - name: researcher
    role: researcher
    max_iterations: 20
    allowed_tools:
      - web_fetch
      - calculate
      - get_time
      - remember
      - recall

  - name: coder
    role: coder
    max_iterations: 15
    allowed_tools:
      - shell
      - read_file
      - write_file
      - calculate
      - "filesystem.*" # Glob: all tools from "filesystem" MCP server
    presets:
      quick:
        max_iterations: 5
      deep:
        max_iterations: 30
```

At startup, each yaml entry becomes an Agent DB named `agent:<display_name>` on first boot only. On subsequent boots, existing DBs are reused without overwriting their `config` — yaml is a bootstrap template, and the AgentDb is the authoritative source of agent configuration once it exists. Edit live config with `/agent set <ref> <field> <value>`, which takes effect on the next message (no restart needed) via runtime hydration from the DB.

## Agent DB schema

Each Agent DB contains five well-known stores:

| Store          | Kind                         | Contents                                                                                    |
| -------------- | ---------------------------- | ------------------------------------------------------------------------------------------- |
| `config`       | DocStore                     | Serialized `AgentDbConfig`: role, model, allowed_tools, max_iterations, grants, presets     |
| `memory`       | `Table<MemoryEntry>`         | The agent's own persistent key-value facts (written by `remember`, read by `recall`)        |
| `meta`         | DocStore                     | `AgentMeta`: display_name, description, capabilities, avatar                                |
| `history`      | `Table<SessionHistoryEntry>` | Sessions this agent has participated in (appended on attach)                                |
| `memory_banks` | `Table<MemoryBankRef>`       | Refs to shared memory banks this agent has been granted access to (name, db_id, permission) |

The peer maintains two local indexes in its `chazdb` (the peer-local bookkeeping database): an `agents` DocStore for Living Agents and a `memory_banks` DocStore for standalone Memory Bank DBs. Both map `db_id → (display_name, pubkey)` and share the same `HostedIndex` type. Both exist because eidetica has no inverse "list DBs this key can access" query.

## Session participation

A session's _authoritative_ participant list is its eidetica AuthSettings. Adding an agent to a session grants its pubkey `Permission::Write` on the session DB; revoking removes it. The `SessionMeta.agents: Vec<AgentRef>` field is a readable cache that stays in sync.

### `/agent` commands

Every transport uses the same set of commands. TUI: `/agent <sub>`. Matrix: `!chaz agent <sub>`.

Every ref is either an agent's display name or its eidetica DB ID; resolution tries display name first.

| Command                      | What                                                                                                                           |
| ---------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `/agent add <ref>`           | Grant the agent Write permission on the session, append to `SessionMeta.agents`, log entry in the agent's history. Idempotent. |
| `/agent remove <ref>`        | Revoke the agent's session key and remove from `SessionMeta.agents`. History is append-only and is preserved.                  |
| `/agent list` (or `/agents`) | List agents attached to the current session. The _host_ agent is marked.                                                       |
| `/agent host <ref>`          | Designate the session's host agent (see turn-taking). Agent must already be attached.                                          |
| `/agent host` (no arg)       | Clear the host agent.                                                                                                          |

## Turn-taking

When a message arrives on a multi-agent session, routing picks one agent in this precedence:

1. Explicit override (scheduler `/run`, gateway directives).
2. **`@<name>` mention** in the message text — first token matching an attached agent's display_name wins. `@alpha`, `@beta-bot,`, `@gamma.` all work; `a@b.com` is ignored (no leading `@` at token start).
3. **Host agent** (`SessionMeta.host_agent_db_id`) if that agent is still attached.
4. First attached agent in AuthSettings order.
5. Legacy `SessionMeta.agent_name` (pre-Living-Agents sessions).
6. Default agent from yaml.

Mentions are case-insensitive and match exact display names. No prefix matching.

## Heartbeat rules

A heartbeat rule is a cron-scheduled trigger stored inside the session. The `HeartbeatRunner` on every peer polls hosted sessions every 30s; rules targeting agents this peer hosts get fired. Each firing writes a `Directive` entry to the session, just like a manual message, and the mention-aware router picks the target.

`last_fired` is tracked peer-locally in the `chazdb`, not in the synced rule — each peer hosting the target agent fires its own schedule independently.

### `/heartbeat` commands

Cron uses 6 fields: `sec min hour day_of_month month day_of_week`.

| Command                                                                        | What                                                         |
| ------------------------------------------------------------------------------ | ------------------------------------------------------------ |
| `/heartbeat list` (or bare `/heartbeat`)                                       | List rules on the current session.                           |
| `/heartbeat add <id> <sec> <min> <hour> <dom> <mon> <dow> <agent_ref> <task…>` | Upsert a rule keyed by `<id>`. Task may contain `@mentions`. |
| `/heartbeat remove <id>`                                                       | Remove a rule by id.                                         |

Example — make `researcher` post a morning briefing to the current session weekdays at 09:00:

```text
/heartbeat add brief 0 0 9 * * Mon-Fri researcher Summarize overnight activity and surface anything urgent.
```

## Tool Narrowing

Tool access is controlled at two levels:

1. **Agent definition**: `allowed_tools` restricts which tools an agent can see. Supports exact names and glob patterns (`"filesystem.*"` matches all tools from that MCP server namespace).
2. **Transitive narrowing**: When agent A spawns agent B, B's tools are the _intersection_ of A's tools and B's `allowed_tools`.

This means a child agent can never have more tools than its parent, even if its definition allows them.

```mermaid
graph TD
    D[default<br/>all 9 tools] -->|spawn| R[researcher<br/>5 tools]
    D -->|spawn| C[coder<br/>4 tools]
    R -.->|"cannot spawn<br/>(not in can_spawn)"| C
```

## Spawn Permissions

The `can_spawn` field controls which agents can be delegated to. Permissions are checked bidirectionally:

- The calling agent must list the target in `can_spawn`.
- The target agent must exist in the registry.

Spawn depth is limited by `max_iterations` to prevent infinite recursion.

## Presets

Agents can define named presets that override fields:

```yaml
presets:
  quick:
    max_iterations: 5
  deep:
    max_iterations: 30
    role_suffix: "Be thorough and explore multiple angles."
```

The calling agent can request a preset via the `spawn_agent` tool:

```json
{ "agent": "researcher", "task": "...", "preset": "deep" }
```

## Synchronous vs Asynchronous Spawn

By default, `spawn_agent` waits for the child agent to complete and returns the result. With `"async": true`, it returns immediately and the child runs in the background:

```json
{ "agent": "researcher", "task": "...", "async": true }
```

Async spawns return the child session ID, which can be found via `/sessions` in the TUI.

## How Spawn Works Internally

When an agent calls `spawn_agent`:

1. A new session database is created via the server's `register_child_session`.
2. A `Directive` entry is written to the child session.
3. The server's `on_local_write` callback detects the directive and spawns an agent task.
4. The agent runs the ReAct loop, writing Ack, ToolCall, ToolResult, and response entries.
5. A completion signal notifies the parent (for synchronous spawns).
6. The parent reads the response from the child session.

This routes through the same server processing path as user messages, unifying all agent invocation.
