# Tools

Chaz agents interact with the world through tools. The ReAct loop calls tools based on LLM decisions, subject to security policies and approval gates.

## Built-in Tools

Every built-in is owned by an [extension](extensions.md); disabling an extension hides its tools from the LLM. The owning extension is shown so you can find a tool's lifecycle (and disable it per-session/per-agent) at a glance.

| Tool                | Owner      | Risk   | Approval           | Description                                                            |
| ------------------- | ---------- | ------ | ------------------ | ---------------------------------------------------------------------- |
| `get_time`          | `system`   | Low    | Never              | Returns the current UTC time                                           |
| `calculate`         | `system`   | Low    | Never              | Evaluates math expressions (via meval)                                 |
| `describe_tool`     | `system`   | Low    | Never              | Returns full description/schema for a tool (discovery)                 |
| `compact`           | `core`     | Low    | Never              | Summarize and compact conversation context                             |
| `spawn_agent`       | `core`     | Medium | UnlessAutoApproved | Delegates a task to a named sub-agent (persistent identity)            |
| `spawn_task`        | `core`     | Medium | UnlessAutoApproved | Runs a one-shot task in a fresh child session, then revokes its key    |
| `shell`             | `core`     | High   | Always             | Executes a shell command                                               |
| `read_file`         | `fs`       | Low    | Never              | Reads file contents from disk                                          |
| `write_file`        | `fs`       | Medium | UnlessAutoApproved | Writes content to a file                                               |
| `edit_file`         | `fs`       | Medium | UnlessAutoApproved | Replace exact text in a file (single or atomic-multi-edit)             |
| `web_fetch`         | `web`      | Medium | UnlessAutoApproved | HTTP GET or POST requests                                              |
| `web_search`        | `web`      | Low    | Never              | Search the web; returns title/url/snippet per result                   |
| `remember`          | `memory`   | Low    | Never              | Stores a key-value fact in the agent's own memory (or a granted bank)  |
| `recall`            | `memory`   | Low    | Never              | Searches the agent's own memory (or a granted bank) by keyword         |
| `list_memory_banks` | `memory`   | Low    | Never              | Lists the memory banks this agent has been granted access to           |
| `skill_list`        | `skills`   | Low    | Never              | List the skills available to this agent (progressive disclosure)       |
| `skill_search`      | `skills`   | Low    | Never              | Search the skill catalog by keyword                                    |
| `skill_show`        | `skills`   | Low    | Never              | Fetch a skill's body — the "activation" half of progressive disclosure |
| `schedule_add`      | `schedule` | Low    | Never              | Add a recurring agent-owned schedule (cron)                            |
| `schedule_modify`   | `schedule` | Low    | Never              | Partial update of an existing schedule                                 |
| `schedule_remove`   | `schedule` | Low    | Never              | Delete a schedule by id                                                |
| `schedule_list`     | `schedule` | Low    | Never              | List an agent's schedules                                              |
| `schedule_once`     | `schedule` | Low    | Never              | Add a one-shot schedule firing after N seconds                         |

That's 23 tools across 7 extensions today. External tools from [MCP servers](mcp.md) plug in under the same policy layer and show up here too, namespaced as `<server>.<tool>`.

## Risk Levels

- **Low** -- safe operations with no side effects
- **Medium** -- operations that modify state or access the network
- **High** -- operations that execute arbitrary code

## Approval Requirements

- **Never** -- tool runs without asking the user
- **UnlessAutoApproved** -- runs automatically if listed in `security.auto_approved_tools`, otherwise asks
- **Always** -- always asks the user before running

In the TUI, approval is an inline prompt (y/n/a). In Matrix, approval is not yet implemented -- unapproved tools time out.

## Tool Details

### get_time

Returns the current UTC timestamp. No arguments.

### calculate

Evaluates a mathematical expression string. Uses the `meval` crate.

```json
{ "expression": "2 * pi * 6371" }
```

### read_file / write_file

Read or write files on the host filesystem. Both go through the host's `FileRead` / `FileWrite` capabilities; once the [`FsGrant`](#capability-boundary) path enforcement is wired they'll honour `allow_read` / `allow_write` allowlists.

```json
{"path": "/tmp/notes.txt"}
{"path": "/tmp/output.txt", "content": "Hello, world!"}
```

### edit_file

Replace exact text in a file. The tool validates that each `old_text` appears **exactly once** before writing, which makes diff-style edits safe. Use the `edits` array to apply several replacements atomically (all or nothing).

```json
{"path": "/tmp/notes.txt", "old_text": "TODO: implement", "new_text": "Implemented in PR #42"}

{"path": "/tmp/config.toml", "edits": [
  {"old_text": "version = \"0.3.0\"", "new_text": "version = \"0.3.1\""},
  {"old_text": "verbose = false",     "new_text": "verbose = true"}
]}
```

### web_fetch

Performs HTTP requests. Subject to network policy (endpoint allowlisting, SSRF protection).

```json
{"url": "https://api.example.com/data", "method": "GET"}
{"url": "https://api.example.com/submit", "method": "POST", "body": "{\"key\": \"value\"}"}
```

### web_search

Runs a search query and returns up to 10 normalized results (`{title, url, snippet}`). Typically pairs with `web_fetch` — search for a topic, then fetch the most relevant result.

```json
{ "query": "CRDT synchronization algorithms", "max_results": 5 }
```

Backends are an **ordered preference list** configured under `web_search.backends` — the tool tries each in turn and falls through to the next on any error, returning the first success. Configuration lives in [Configuration](configuration.md#web-search). Supported backends:

- **kagi** — Kagi Search API (requires `api_key`; invite-only beta)
- **tavily** — LLM-oriented search API (requires `api_key`)
- **brave** — Brave Search API (requires `api_key`)
- **serper** — Google SERP via serper.dev (requires `api_key`)
- **searxng** — any SearxNG instance (requires `url:`, no key by default). Self-host or point at a public instance.
- **duckduckgo** — keyless fallback that scrapes DuckDuckGo's HTML page

Empty results are a legitimate answer and do **not** trigger failover — otherwise a bad query would mask itself by running through every backend. Only errors (network, HTTP non-2xx, bad JSON) advance to the next entry.

The tool is Low-risk and approval-free because the agent never supplies the destination URL — only a query string — and every HTTP destination is fixed by operator config.

### shell

Executes a shell command. Subject to command allowlist/denylist filtering.

```json
{ "command": "ls -la /tmp" }
```

### remember / recall

Persistent key-value memory. By default writes to the agent's own memory (its own Living Agent DB's `memory` subtree), which travels with the agent through eidetica sync and is naturally isolated from other agents.

```json
{"key": "user_timezone", "value": "America/New_York"}
{"query": "timezone"}
```

**Shared memory banks.** Pass an optional `bank` argument to read or write a shared bank the agent has been granted access to. Banks are separate eidetica DBs (or other Agent DBs) configured by an operator via `/memory grant`; the agent's own grants are listed by `list_memory_banks`. Write access requires `write` permission on the bank.

```json
{"key": "deadline", "value": "Friday", "bank": "project-alpha"}
{"query": "deadline", "bank": "project-alpha"}
```

There is no "global" scope: cross-agent sharing is always a bank with an explicit grant. Access is authoritatively enforced by the bank DB's eidetica `AuthSettings` — the agent's key must be authorized on the bank DB itself.

### list_memory_banks

Lists the memory banks this agent has been granted access to, with the permission level (Read or Write) for each. `self` is always listed — that's the agent's own memory. Use the names it returns with `remember` / `recall`'s `bank` argument.

### describe_tool

Returns the full description and JSON Schema for any registered tool. Useful when tool profiles hide details (Brief or Summary mode) and the agent needs the full specification.

```json
{ "tool": "filesystem.read_file" }
```

### compact

Summarizes the conversation history via an LLM call and writes a `Summary` entry. The context builder treats the most recent Summary as the conversation start boundary, effectively compacting older messages.

### spawn_agent

Delegates a task to another agent in a child session. The named agent's persistent identity and memory are used — pick this when the work needs continuity. See [Agents](agents.md).

```json
{
  "agent": "researcher",
  "task": "Find the latest papers on CRDT synchronization",
  "async": false
}
```

### spawn_task

Runs a one-shot task in a fresh child session under a freshly-generated keypair. When the task finishes the key is **revoked** — the session DB persists as an audit record but can't be extended. Use this for focused work that should not pollute any agent's memory; use `spawn_agent` when you want the named agent's identity and memory to carry forward.

```json
{
  "task": "Read /tmp/log.json and summarise the error counts by minute",
  "tools": ["read_file", "calculate"],
  "async": false
}
```

`tools` narrows the caller's scope for the child; omit to inherit the full scope. `model` and `max_iterations` are optional overrides.

### skill_list / skill_search / skill_show

Progressive-disclosure access to the [skills catalog](memory.md#skills). `skill_list` returns one line per available skill (`name — description`); `skill_search` filters that list by keyword. Neither loads the body. `skill_show` is what actually pulls a skill into the agent's view — call it with the name from the catalog.

```json
{ "query": "git" }
{ "skill": "release-checklist" }
```

Skills come from three sources merged at session start: disk skills shipped on this peer, skills written into the responding agent's own DB, and skills in any granted skill banks attached to the session. The merged catalog is exposed via these tools only when the `skills` extension is active for the agent.

### schedule_add / schedule_modify / schedule_remove / schedule_list

Agent-facing CRUD over agent-owned schedules, mirroring the `/schedule` slash commands described in [Agents — Schedules](agents.md#schedules). Schedules live in the owning agent's DB (not the session) and are fired by chaz's `RoutineEngine`, which sleeps until the next due fire instead of polling. `schedule_add` targets the current session (Pinned) by default, or a fresh session per fire. Cron uses 6 fields: `sec min hour day_of_month month day_of_week`.

```json
{
  "id": "morning-brief",
  "cron": "0 0 9 * * Mon-Fri",
  "task": "Summarize overnight activity"
}
```

The `agent` field is optional — omit it to target yourself, or pass a display name / DB id to target another agent on this peer.

**Lifecycle bounds.** A recurring schedule can be retired automatically:

- `max_fires` — retire after this many fires. `cron` hourly + `max_fires: 8` expresses "wake hourly for 8 hours".
- `expires_at` — RFC 3339 timestamp after which it stops firing.

Whichever bound is hit first wins; both are optional (omit = unbounded). `fire_count` is tracked authoritatively in the agent DB so `max_fires` survives restarts. When a bound is reached the schedule is persisted as disabled (it shows in `schedule_list` as `(disabled)` with its `[fired N×]` count rather than being deleted, so the history stays auditable). `schedule_modify` can set/replace `max_fires`/`expires_at`; re-enabling a schedule that already passed a bound will simply retire again on its next fire.

### schedule_once

One-shot wakeup that fires a directive into this session after a delay, then deletes itself. The directive is routed back to the calling agent — cross-agent scheduling stays in `schedule_add`. Use it for "come back to this in N seconds" cases; for recurring work, use `schedule_add` instead.

```json
{
  "after_seconds": 1800,
  "task": "Check whether the build finished and report status."
}
```

- `after_seconds` is bounded to `[30, 2_592_000]` (30 seconds to 30 days).
- The wakeup time has up to 30 s of jitter past the requested delay (poll interval).
- Generated rule id has the form `wakeup-<epoch_ms>`; it surfaces in `/schedule list` and `schedule_list` with an `@YYYY-MM-DD HH:MM:SSZ` marker in place of a cron expression.

## External Tools (MCP)

Chaz supports external tools via the Model Context Protocol. MCP servers run as subprocesses (or are reached over Streamable HTTP) and their tools are registered alongside built-ins, subject to the same policy layer. See [MCP External Tools](mcp.md) for configuration and a worked example.

## Tool Profiles

Tool profiles control **how** each tool's definition is presented to the LLM — useful when the full set is large and you want to keep token cost down or hide everything but the names from cheap models. A profile maps tool name (exact or glob) to one of:

- **Full** (default) — name, description, full JSON Schema.
- **Brief** — name + first sentence of the description, parameter names only (no per-param descriptions).
- **Summary** — name only, parameters trimmed to an empty schema. The LLM has to call `describe_tool` to learn more.
- **Hidden** — not sent to the LLM at all. The tool stays callable if the agent already knows about it.

Profiles live under top-level `tool_profiles:` in config and are referenced by name from an agent or preset. The resolution order for a tool is **exact name → glob prefix (`namespace.*`) → profile default**.

```yaml
tool_profiles:
  compact:
    default: full
    tools:
      "filesystem.*": brief
      "github.*": summary
      shell: hidden

agents:
  - name: skim
    tool_profile: compact
```

The `compact` profile here shows full schemas for built-ins, brief schemas for the filesystem MCP namespace, name-only for github, and drops `shell` entirely.

## Security Controls

### Capability boundary

Tools access system resources through a **ToolHost** — a sandboxed capability boundary. The tool declares what it wants to do (e.g. `Capability::Shell { command: "ls -la", … }`), and the host decides whether to allow it based on configured grants. The tool itself never reads file paths or hits the network directly; only the host does. The default `NativeToolHost` enforces grants in-process; future hosts will add OS-level (bwrap) or VM-level (WASM) sandboxing.

Today there are three grant kinds, declared per-tool under `security.tool_policies.<tool>.grants`:

| Grant     | Field                        | What it does                                                                             |
| --------- | ---------------------------- | ---------------------------------------------------------------------------------------- |
| `shell`   | `allow` / `deny`             | Command-prefix allowlist + denylist. Empty `allow` = allow-all. `deny` always wins.      |
| `network` | `endpoints`                  | List of `{ host, path_prefix?, methods? }` patterns. Empty list = allow any public host. |
| `network` | `allow_private`              | If `false` (default), private IPs and internal hostnames are blocked even when allowed.  |
| `fs`      | `allow_read` / `allow_write` | Path allowlists. **Schema stub** — accepted in config today, enforcement still pending.  |

Resolution: the tool's `default_policy()` ships baseline grants; `security.tool_policies` overrides per-tool at config load; per-agent grants on `agents:` overlay last, kind-by-kind ([`Grants::merge_over`](https://github.com/arcuru/chaz/blob/vibe/crates/lib/src/grants.rs) — the agent layer replaces a kind only when it explicitly sets one).

#### Worked example: lock `shell` down to `git` and `ls`, deny everything else

```yaml
security:
  tool_policies:
    shell:
      approval: unless_auto_approved # let agents run allowed commands without prompting
      grants:
        shell:
          allow: ["git", "ls", "cat"]
          deny: ["rm", "sudo"]
    web_fetch:
      grants:
        network:
          endpoints:
            - host: "api.github.com"
              methods: ["GET"]
            - host: "*.example.com"
              path_prefix: "/v1"
          allow_private: false
```

What this gets you:

- `shell` accepts `git status`, `ls -la`, `cat /etc/hosts`, but the host **denies** `git push && rm foo` because the denylist matches `rm` in the second segment of the pipeline. Empty grant or missing `grants.shell` = permissive (the host enforces nothing).
- `web_fetch` can GET `https://api.github.com/...` and any path under `https://*.example.com/v1/...`, but is denied other hosts entirely; private IPs (`127.0.0.1`, `10.0.0.0/8`, internal hostnames) are blocked regardless.
- Want a specific agent to be tighter still? Re-declare the same `grants:` block under `agents: - name: <agent>` — that overlay replaces the resolved policy's grants per-kind for that agent only.

The legacy `security.shell_allowlist` / `security.shell_denylist` / `security.allowed_endpoints` fields still work but are deprecated; they're synthesised into the new grant shape at startup with a one-time `warn!`.

### Leak detection

All tool outputs are scanned for secret patterns (API keys, tokens, etc.) before entering the LLM context. The leak detector supports 12 patterns and can either redact or block the output.

### Output safety

Tool results fed back to the LLM are wrapped in XML delimiters (`<tool_output tool="name">...</tool_output>`) with angle-bracket escaping, preventing prompt injection through tool output.

### Timeouts and rate limits

Tool execution is wrapped in a configurable timeout (default varies by tool). Tools can also have a `rate_limit` (max calls per minute) configured in their policy.

See [Security](security.md) for details on network policies, shell sandboxing, rate limiting, and approval configuration.
