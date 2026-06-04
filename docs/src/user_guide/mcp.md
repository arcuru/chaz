# MCP External Tools

Chaz supports external tools via the [Model Context Protocol (MCP)](https://modelcontextprotocol.io/). MCP servers plug into the same registry the built-ins use and are subject to the same policy layer (risk, approval, grants, leak detection). Two transports are supported:

- **stdio** — chaz spawns a subprocess and speaks JSON-RPC over its stdin/stdout. Pick this for local tools (`npx @modelcontextprotocol/server-filesystem`, `uvx mcp-server-git`, etc.).
- **Streamable HTTP** — chaz POSTs requests to a URL and reads JSON or SSE responses. Pick this for remote/hosted servers.

Adding a server is **config-only** — no code changes, no rebuild.

## Configuration

Add MCP servers in the `mcp_servers` section of your config. Stdio and HTTP servers can coexist:

```yaml
mcp_servers:
  # Stdio transport — chaz spawns and supervises the subprocess.
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

  # Streamable HTTP transport — chaz POSTs to a remote server.
  - name: remote-search
    url: "https://mcp.example.com/v1"
    default_policy:
      risk: low
      approval: never
```

### Fields

| Field            | Required | Description                                                                 |
| ---------------- | -------- | --------------------------------------------------------------------------- |
| `name`           | Yes      | Namespace prefix for tools (e.g., `filesystem` → `filesystem.read_file`)    |
| `command`        | Stdio    | Command to spawn the MCP server subprocess                                  |
| `args`           | No       | Arguments for the command (stdio only)                                      |
| `env`            | No       | Environment variables (supports `${VAR}` references; stdio only)            |
| `url`            | HTTP     | Endpoint URL for Streamable HTTP transport — when set, `command` is ignored |
| `default_policy` | No       | Default `ToolPolicy` for every tool from this server                        |

Set exactly one of `command` (stdio) or `url` (HTTP) per server. You can also drop one MCP server config per file into `mcp_server_dir` (see [Configuration](configuration.md)); those entries are merged with the inline list at startup.

## Tool Discovery and Namespacing

At startup, chaz:

1. Spawns each MCP server subprocess
2. Performs the MCP `initialize` handshake
3. Calls `tools/list` to discover available tools
4. Registers each tool as `server_name.tool_name` (e.g., `filesystem.read_file`)

Failed servers are logged and skipped — they don't block startup. Name collisions across servers are detected and duplicates are skipped with a warning.

## Auto-Restart (stdio only)

If a **stdio** MCP server process crashes (detected via IO errors when calling a tool), chaz automatically restarts it:

- **Exponential backoff**: 1s, 2s, 4s, 8s, 16s between attempts
- **Max attempts**: 5 consecutive restarts before giving up
- **Counter reset**: The restart counter resets to zero after a successful tool call
- **Re-initialization**: After restart, the MCP handshake and tool discovery are repeated

This means transient subprocess crashes are handled transparently — the tool call that triggered the restart is retried after a successful restart. HTTP transport has no equivalent — chaz just surfaces the error to the caller.

Both transports honour the spec's `notifications/tools/list_changed`: when a server signals its tool set has changed, chaz lazily re-fetches `tools/list` before the next call. There's no need to restart chaz to pick up new tools.

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
      "filesystem__*": brief
      "github__*": summary
```

Use `describe_tool` for on-demand discovery when tools are in Brief or Summary mode:

```json
{ "tool": "filesystem__read_file" }
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
      - "filesystem__*" # All filesystem MCP tools
```

See [Agents](agents.md) for details on tool narrowing.

## End-to-end walkthrough: adding the filesystem server

The shortest "agent reads a file through MCP" path:

1. Add the server to your config and (re)start chaz:

   ```yaml
   mcp_servers:
     - name: filesystem
       command: npx
       args: ["-y", "@modelcontextprotocol/server-filesystem", "/home/me/notes"]
   ```

2. Confirm registration. On startup chaz logs the handshake and discovered tool count per server (look for `MCP '<name>'` lines). A failed handshake is logged and the server is skipped — chaz keeps running with whatever did register.

   In a session you can ask the agent itself:

   ```
   What MCP tools are registered? Use describe_tool on any filesystem__* tool you find.
   ```

   `describe_tool` returns the full schema and is what the LLM uses to discover details about tools hidden by [tool profiles](tools.md#tool-profiles).

3. Use them in a turn. The first call to a Medium-risk tool will trigger an approval prompt in the TUI:

   ```
   List the files in /home/me/notes and read the first .md file.
   ```

   The agent calls `filesystem__list_directory` then `filesystem__read_file`. Each call runs under chaz's policy layer — risk tier, approval, leak detection, timeout.

4. **If something fails**: chaz logs the handshake failure and skips the server (it does not block startup). Check the log for `MCP '<name>'` lines. For stdio servers, fix the command/args and restart; for HTTP, verify the URL and reachability.

5. To narrow which agents can see an MCP namespace, add a glob to the agent's `allowed_tools`:

   ```yaml
   agents:
     - name: notetaker
       allowed_tools: ["read_file", "filesystem__*"]
   ```

   `filesystem__*` matches every tool that namespace exposes — present or future. The same glob form works in [tool profiles](tools.md#tool-profiles) for controlling how the LLM sees the tool definitions.

## Limitations

- **No streaming**: Tool results are returned as a single response, not streamed.
- **Stdio sandboxing is process-level only**: the MCP subprocess inherits chaz's environment. Pair sensitive servers with [grants](tools.md#capability-boundary) and per-agent `allowed_tools` rather than relying on the server itself for isolation.
