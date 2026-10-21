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

| Field                  | Required | Description                                                                                     |
| ---------------------- | -------- | ----------------------------------------------------------------------------------------------- |
| `name`                 | Yes      | Namespace prefix for tools (e.g., `filesystem` → `filesystem.read_file`)                        |
| `command`              | Stdio    | Command to spawn the MCP server subprocess                                                      |
| `args`                 | No       | Arguments for the command (stdio only)                                                          |
| `env`                  | No       | Environment variables (supports `${VAR}` references; stdio only)                                |
| `url`                  | HTTP     | Endpoint URL for Streamable HTTP transport — when set, `command` is ignored                     |
| `default_policy`       | No       | Default `ToolPolicy` for every tool from this server                                            |
| `startup_timeout_secs` | No       | Seconds to allow for handshake + tool discovery before marking the server failed (default `30`) |

Set exactly one of `command` (stdio) or `url` (HTTP) per server. You can also drop one MCP server config per file into `mcp_server_dir` (see [Configuration](configuration.md)); those entries are merged with the inline list at startup.

## Tool Discovery and Namespacing

For each configured server, chaz:

1. Spawns the MCP server subprocess
2. Performs the MCP `initialize` handshake — records which primitives (tools, resources, prompts) the server advertises
3. Calls `tools/list` to discover available tools (only when the server claims the `tools` capability)
4. Registers each tool as `server_name__tool_name` (e.g., `filesystem__read_file`)
5. Adds capability wrapper tools for any other primitives the server claimed (see [Resources and Prompts](#resources-and-prompts))

Failed servers are logged and skipped. Name collisions across servers are detected and duplicates are skipped with a warning.

## Background Startup

Servers start **in the background**, concurrently with each other, and chaz does not wait for any of them. The peer is interactive from the first frame whether a server takes 40 ms or 20 seconds.

Tools become available as their server finishes. A turn's tool list is assembled when the turn starts, so a server that lands mid-conversation is advertised on the next turn — no restart, no reload. A server that lands mid-_turn_ is callable but not yet advertised; the model will not know to ask for it until the turn after.

If the model calls a tool whose server is still starting — from a resumed conversation, say, where the name appears earlier in the transcript — it is told the server is still starting and that the call is worth retrying, rather than being told the tool does not exist.

`startup_timeout_secs` (default `30`) bounds one server's handshake and discovery. A server that overruns it contributes no tools. Chaz explicitly closes its transport before marking startup failed; this cleanup attempt has the separate per-child budget below. Other servers continue starting concurrently.

**One-shot runs are the exception.** `chaz --print` gets exactly one turn, so it waits for every server to settle before running it — deferring a load that single turn needs would produce an answer with no MCP tools rather than a faster one. This costs no more than blocking startup used to, since servers start concurrently. `chaz cmd` runs no turn at all and never waits.

Server state is visible in the TUI under **Peer → MCP**, where every configured server appears immediately as `starting…` and then resolves to running or failed. `starting` is not a failure — it means the connection is still in flight.

## Resources and Prompts

The MCP spec defines three primitives a server can expose: **Tools**, **Resources**, and **Prompts**. Chaz consumes all three. Whichever primitives a server advertises in its `initialize` response light up the corresponding wrapper tools:

| Server advertises | Wrapper tools added                                         |
| ----------------- | ----------------------------------------------------------- |
| `resources`       | `<server>__list_resources`, `<server>__read_resource(uri)`  |
| `prompts`         | `<server>__list_prompts`, `<server>__get_prompt(name, ...)` |

The model uses these like any other tool: `list_resources` returns URIs + metadata, `read_resource` fetches one by URI. For prompts, `list_prompts` shows declared argument shapes (required args marked with `*`) and `get_prompt` renders the template into a single assistant-readable block.

If a server only advertises `tools`, none of these wrappers appear. There's no per-server config knob to disable them — they follow what the server claims.

### Resource contents

`read_resource` concatenates every text content block the server returns. Binary blobs are summarized inline as `[binary <mime>: ~N bytes]` rather than dumped — chaz never spills decoded binary into the model's context. The same 100 KB truncation that bounds tool output bounds resource reads.

### Prompts as tool results

`get_prompt` returns the rendered template as a tool result (each message tagged `[role] body`). It does NOT replace the agent's system prompt — the model decides what to do with the content. Use this for parameterized prompt libraries the server exposes (`summarize`, `explain-code`, etc.).

## Direct-child shutdown

Chaz owns the process it spawns, including while initialization is still pending. Shutdown stops new requests, joins startup, and closes the actual subprocess stdin before escalating:

| Phase                                    | Maximum allowance per child |
| ---------------------------------------- | --------------------------- |
| Close stdin and wait for EOF-driven exit | 2 seconds                   |
| Send POSIX TERM and wait                 | 2 seconds, on POSIX only    |
| Native forced kill and reap              | 2 seconds                   |

These are fixed allowances, not configuration fields. One monotonic deadline includes startup cancellation, joins, and I/O-lock acquisition; time spent coordinating leaves less grace time. Non-POSIX platforms skip TERM and use Tokio's native forced kill after EOF grace.

Close is terminal: a retained tool handle cannot restart a closed server. Replacement reaps the previous direct child before spawning another, and cancellation cannot install a new child with old pipes. A server whose startup times out never publishes tools later.

An `incomplete MCP cleanup` error means cleanup was **not confirmed**, not that the child exited. Chaz keeps the cleanup handle and retries; service reconnection cannot build a replacement generation until the old cleanup succeeds. There is no detached reaper. Closing several servers is sequential: a cleanup attempt can take up to 6 seconds per child on POSIX, or 4 seconds elsewhere. Retries and other peer teardown work add time; this is not a constant peer shutdown deadline.

**Wrapper descendants are excluded.** Commands such as `npx` and `uvx` remain supported, but Chaz only signals and reaps its direct child. It neither kills process groups nor waits for descendants holding inherited stdout open. A wrapper that starts other processes must manage them itself. Abrupt termination of Chaz is also outside explicit shutdown's guarantee; `kill_on_drop` is only a best-effort fallback.

### Shutdown walkthrough

1. Run a one-shot prompt with an MCP server configured, or exit an interactive peer normally. A cooperative server receives stdin EOF and exits without a signal. A successful one-shot returns exit code `0` only after its MCP cleanup completes; its log contains `Runtime cleanup complete`. A failed prompt still cleans up and keeps its original nonzero exit status.
2. If the child ignores EOF, POSIX shutdown tries TERM after the EOF allowance, then forced kill if necessary. No MCP protocol shutdown message is sent. A child ignoring EOF and TERM normally needs about four seconds before forced reap; more configured children can add their own allowances.
3. If a lock, signal, or wait cannot complete, inspect the log for the named server and the failure stage. For example, the error text can be:

   ```text
   fixture: incomplete MCP cleanup: I/O generation lock timed out
   ```

   Chaz reports `Runtime cleanup incomplete; retaining generation for retry` (or `Generation cleanup incomplete; retaining ownership for retry` under service supervision). It does not reconnect over that child. When the underlying failure clears, a later cleanup attempt reaps the retained child and shutdown or reconnection can finish. Repeated messages mean cleanup remains unresolved, not that replacement servers are being started.

## Auto-Restart (stdio only)

If a **stdio** MCP server process crashes (detected via IO errors when calling a tool), chaz automatically restarts it:

- **Exponential backoff**: 1s, 2s, 4s, 8s, 16s between attempts
- **Max attempts**: 5 consecutive restarts before giving up
- **Counter reset**: The restart counter resets to zero after a successful tool call
- **Re-initialization**: After restart, the MCP handshake is repeated

Recovery prepares the connection for a later call; it does **not** repeat the failed `tools/call`. The original error reaches the model even if recovery fails. HTTP send/body failures also surface without resubmission. A 404 rejecting an issued HTTP session is different: chaz reinitializes and retries once because the server rejected the request before running it. A 404 without an issued session is an ordinary error.

Neither approval nor `readOnlyHint`/`idempotentHint` authorizes replay after a lost tool reply. See [Errors do not undo effects](tools.md#errors-do-not-undo-effects) before repeating an operation.

Both transports honour the spec's `notifications/tools/list_changed`: when a server signals its tool set has changed, chaz lazily re-fetches `tools/list` before the next call. There's no need to restart chaz to pick up new tools.

## Policy

Each MCP tool resolves to a `ToolPolicy` in this precedence order:

1. **`security.tool_policies.<namespaced_name>`** — per-tool override in chaz config; wins unconditionally.
2. **`mcp_servers[*].default_policy`** — per-server pin in chaz config.
3. **Annotations from `tools/list`** — the server self-reports its behavior via the MCP 2025-06 `annotations` field. Chaz maps:
   - `destructiveHint: true` → `risk: high`, `approval: always`
   - `readOnlyHint: true` → `risk: low`, `approval: never`
   - (`destructive` wins if both are set; misconfigured servers fail closed)
4. **chaz default** — `risk: medium`, `approval: unless_auto_approved`, `timeout: 60`.

A well-behaved MCP server can therefore ship without any chaz-side policy boilerplate; chaz trusts the server's self-declaration unless you pin a different value:

```yaml
mcp_servers:
  - name: filesystem
    default_policy:
      risk: low
      approval: never
      timeout: 120
```

Per-tool override via `security.tool_policies` (uses the namespaced name):

```yaml
security:
  tool_policies:
    filesystem__write_file:
      approval: always
      risk: medium
    filesystem__read_file:
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

2. Confirm registration. Chaz logs each server's handshake and discovered tool count as it finishes (look for `MCP '<name>'` lines); the TUI's **Peer → MCP** page shows the same thing live. A failed handshake is logged and the server is skipped — chaz keeps running with whatever did register.

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

4. **If something fails**: chaz logs the handshake failure and skips the server. Check the log for `MCP '<name>'` lines, or open **Peer → MCP** and select the server for the error text. For stdio servers, fix the command/args and restart; for HTTP, verify the URL and reachability. A server stuck on `starting…` is still within its `startup_timeout_secs`; it will resolve to failed when that elapses.

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
