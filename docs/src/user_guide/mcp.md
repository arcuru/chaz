# MCP External Tools

Chaz supports external tools via the [Model Context Protocol (MCP)](https://modelcontextprotocol.io/). MCP servers run as subprocesses communicating over JSON-RPC (stdin/stdout). Their tools are registered alongside built-in tools and subject to the same policy layer.

## Configuration

Add MCP servers in the `mcp_servers` section of your config:

```yaml
mcp_servers:
  - name: filesystem
    command: npx
    args: ["-y", "@modelcontextprotocol/server-filesystem", "/home/user"]
    env:
      SOME_VAR: "value"
    default_policy:
      risk: medium
      approval: unless_auto_approved
      timeout: 30

  - name: github
    command: npx
    args: ["-y", "@modelcontextprotocol/server-github"]
    env:
      GITHUB_TOKEN: "${GITHUB_TOKEN}"
```

### Fields

| Field            | Required | Description                                                              |
| ---------------- | -------- | ------------------------------------------------------------------------ |
| `name`           | Yes      | Namespace prefix for tools (e.g., `filesystem` → `filesystem.read_file`) |
| `command`        | Yes      | Command to spawn the MCP server                                          |
| `args`           | No       | Arguments for the command                                                |
| `env`            | No       | Environment variables (supports `${VAR}` references)                     |
| `default_policy` | No       | Default policy for all tools from this server                            |

## Tool Discovery and Namespacing

At startup, chaz:

1. Spawns each MCP server subprocess
2. Performs the MCP `initialize` handshake
3. Calls `tools/list` to discover available tools
4. Registers each tool as `server_name.tool_name` (e.g., `filesystem.read_file`)

Failed servers are logged and skipped — they don't block startup. Name collisions across servers are detected and duplicates are skipped with a warning.

## Auto-Restart

If an MCP server process crashes (detected via IO errors when calling a tool), chaz automatically attempts to restart it:

- **Exponential backoff**: 1s, 2s, 4s, 8s, 16s between attempts
- **Max attempts**: 5 consecutive restarts before giving up
- **Counter reset**: The restart counter resets to zero after a successful tool call
- **Re-initialization**: After restart, the MCP handshake and tool discovery are repeated

This means transient crashes are handled transparently — the tool call that triggered the restart is retried after a successful restart.

## Policy

MCP tools default to Medium risk / UnlessAutoApproved / 60s timeout. Override per server:

```yaml
mcp_servers:
  - name: filesystem
    default_policy:
      risk: low
      approval: never
      timeout: 120
```

Or override individual tools via `security.tool_policies`:

```yaml
security:
  tool_policies:
    filesystem.write_file:
      approval: always
      risk: medium
    filesystem.read_file:
      approval: never
      risk: low
```

## Tool Profiles

Control how MCP tool definitions are presented to the LLM using [tool profiles](tools.md#tool-profiles). Glob patterns match namespaces:

```yaml
tool_profiles:
  compact:
    default: full
    tools:
      "filesystem.*": brief
      "github.*": summary
```

Use `describe_tool` for on-demand discovery when tools are in Brief or Summary mode:

```json
{ "tool": "filesystem.read_file" }
```

## Agent Tool Allowlists

Agents can reference MCP tools by exact name or glob pattern:

```yaml
agents:
  - name: coder
    allowed_tools:
      - shell
      - read_file
      - write_file
      - "filesystem.*" # All filesystem MCP tools
```

See [Agents](agents.md) for details on tool narrowing.

## Limitations

- **Subprocess only**: Currently supports stdin/stdout JSON-RPC transport. SSE/HTTP transport is planned.
- **No dynamic re-discovery**: Tools are discovered once at startup (or restart). The `notifications/tools/list_changed` notification is not yet handled.
- **No streaming**: Tool results are returned as a single response, not streamed.
