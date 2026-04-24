# Tools

Chaz agents interact with the world through tools. The ReAct loop calls tools based on LLM decisions, subject to security policies and approval gates.

## Built-in Tools

| Tool                | Risk   | Approval           | Description                                                           |
| ------------------- | ------ | ------------------ | --------------------------------------------------------------------- |
| `get_time`          | Low    | Never              | Returns the current UTC time                                          |
| `calculate`         | Low    | Never              | Evaluates math expressions (via meval)                                |
| `read_file`         | Low    | Never              | Reads file contents from disk                                         |
| `remember`          | Low    | Never              | Stores a key-value fact in the agent's own memory (or a granted bank) |
| `recall`            | Low    | Never              | Searches the agent's own memory (or a granted bank) by keyword        |
| `list_memory_banks` | Low    | Never              | Lists the memory banks this agent has been granted access to          |
| `describe_tool`     | Low    | Never              | Returns full description/schema for a tool (discovery)                |
| `compact`           | Low    | Never              | Summarize and compact conversation context                            |
| `web_search`        | Low    | Never              | Search the web; returns title/url/snippet per result                  |
| `write_file`        | Medium | UnlessAutoApproved | Writes content to a file                                              |
| `web_fetch`         | Medium | UnlessAutoApproved | HTTP GET or POST requests                                             |
| `spawn_agent`       | Medium | UnlessAutoApproved | Delegates a task to a sub-agent                                       |
| `shell`             | High   | Always             | Executes a shell command                                              |

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

Read or write files on the host filesystem.

```json
{"path": "/tmp/notes.txt"}
{"path": "/tmp/output.txt", "content": "Hello, world!"}
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

- **tavily** — LLM-oriented search API (requires `api_key`)
- **brave** — Brave Search API (requires `api_key`)
- **serper** — Google SERP via serper.dev (requires `api_key`)
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

Delegates a task to another agent in a child session. See [Agents](agents.md).

```json
{
  "agent": "researcher",
  "task": "Find the latest papers on CRDT synchronization",
  "async": false
}
```

## External Tools (MCP)

Chaz supports external tools via the Model Context Protocol. MCP servers run as subprocesses and their tools are registered alongside built-ins, subject to the same policy layer. See [MCP External Tools](mcp.md) for configuration and details.

## Security Controls

All tool outputs are scanned for secret patterns (API keys, tokens, etc.) before entering the LLM context. The leak detector supports 12 patterns and can either redact or block the output.

Tool results fed back to the LLM are wrapped in XML delimiters (`<tool_output tool="name">...</tool_output>`) with angle-bracket escaping, preventing prompt injection through tool output.

Tool execution is wrapped in a configurable timeout (default varies by tool). Tools can also have a `rate_limit` (max calls per minute) configured in their policy.

See [Security](security.md) for details on network policies, shell sandboxing, rate limiting, and approval configuration.
