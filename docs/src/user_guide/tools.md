# Tools

Chaz agents interact with the world through tools. The ReAct loop calls tools based on LLM decisions, subject to security policies and approval gates.

## Built-in Tools

| Tool          | Risk   | Approval           | Description                                  |
| ------------- | ------ | ------------------ | -------------------------------------------- |
| `get_time`    | Low    | Never              | Returns the current UTC time                 |
| `calculate`   | Low    | Never              | Evaluates math expressions (via meval)       |
| `read_file`   | Low    | Never              | Reads file contents from disk                |
| `remember`    | Low    | Never              | Stores a key-value fact in persistent memory |
| `recall`      | Low    | Never              | Searches stored facts by keyword             |
| `write_file`  | Medium | UnlessAutoApproved | Writes content to a file                     |
| `web_fetch`   | Medium | UnlessAutoApproved | HTTP GET or POST requests                    |
| `spawn_agent` | Medium | UnlessAutoApproved | Delegates a task to a sub-agent              |
| `shell`       | High   | Always             | Executes a shell command                     |

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

### shell

Executes a shell command. Subject to command allowlist/denylist filtering.

```json
{ "command": "ls -la /tmp" }
```

### remember / recall

Persistent key-value memory shared across all sessions.

```json
{"key": "user_timezone", "value": "America/New_York"}
{"query": "timezone"}
```

### spawn_agent

Delegates a task to another agent in a child session. See [Agents](agents.md).

```json
{
  "agent": "researcher",
  "task": "Find the latest papers on CRDT synchronization",
  "async": false
}
```

## Security Controls

All tool outputs are scanned for secret patterns (API keys, tokens, etc.) before entering the LLM context. The leak detector supports 12 patterns and can either redact or block the output.

Tool execution is wrapped in a configurable timeout (default varies by tool).

See [Security](security.md) for details on network policies, shell sandboxing, and approval configuration.
