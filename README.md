# chaz

Chaz is an experimental AI agent framework built on [eidetica](https://github.com/arcuru/eidetica). It features a ReAct tool-calling loop, multi-agent orchestration, and persistent + syncable per-session state. The primary gateway is a [Matrix](https://matrix.org) bot connecting to any OpenAI-compatible LLM provider; a TUI is also included.

> ⚠️ **Experimental.** APIs, config, and on-disk state may change without notice. Don't run against data you can't afford to lose.

For a more mature Matrix chatbot focused on day-to-day use, see [baibot](https://github.com/etkecc/baibot).

Announcement Blog Post: [Chaz: An LLM <-> Matrix Chatbot](https://jackson.dev/post/chaz/)

## Getting Help

There is a public Matrix room available at [#chaz:jackson.dev](https://matrix.to/#/#chaz:jackson.dev)

## Features

- **ReAct tool-calling loop** — agents reason, act, and observe in a loop with 9 built-in tools
- **Extension framework** — tools, slash commands, and lifecycle hooks all flow through declared extensions with per-session activation, settings, and an event-log audit trail (see [`docs/`](docs/src/user_guide/extensions.md))
- **Multi-agent orchestration** — Agents (peer entities, keys + identity) invoke per-Agent Worker templates (one-shot LLM calls, no identity) with depth limiting and transitive tool narrowing
- **TUI mode** — local terminal interface for testing and debugging without Matrix
- **Session sharing** — share sessions between chaz instances via eidetica sync
- **Security** — leak detection, network controls, shell sandboxing, tool approval gates
- **Persistent sessions** — conversation history survives restarts via eidetica SQLite

### Built-in Tools

| Tool                | Description                                          |
| ------------------- | ---------------------------------------------------- |
| `get_time`          | Current UTC time                                     |
| `calculate`         | Math expressions                                     |
| `read_file`         | Read file contents                                   |
| `write_file`        | Write to files                                       |
| `web_fetch`         | HTTP GET/POST                                        |
| `web_search`        | Search the web (Tavily/Brave/Serper/DuckDuckGo)      |
| `shell`             | Execute commands (approval required)                 |
| `remember`          | Store key-value facts (own memory or a granted bank) |
| `recall`            | Search stored facts                                  |
| `list_memory_banks` | List shared memory banks this agent can access       |
| `spawn_agent`       | Delegate to a named peer Agent (Ava, Chaz)           |
| `spawn_worker`      | Invoke a Worker template under the calling Agent     |

## Running

### Matrix Mode

Connect to Matrix rooms and respond to messages:

```bash
chaz --config config.yaml
```

### TUI Mode

Local terminal interface for testing, debugging, and session management:

```bash
chaz --config config.yaml --tui
```

#### TUI Commands

| Command                                             | Description                                        |
| --------------------------------------------------- | -------------------------------------------------- |
| `/help`                                             | Show all commands and key bindings                 |
| `/sessions`, `/s`                                   | Open session picker                                |
| `/new`                                              | Create a new session                               |
| `/join <id>`                                        | Switch to session by name or eidetica DB ID        |
| `/info`                                             | Show current session details                       |
| `/share`                                            | Generate shareable ticket for current session      |
| `/sync <ticket>`                                    | Sync a remote session via ticket                   |
| `/agents`, `/agent list`                            | List agents attached to this session (host marked) |
| `/agent add <ref>`                                  | Attach an agent (`<ref>` = display name or DB ID)  |
| `/agent remove <ref>`                               | Detach an agent                                    |
| `/agent host [<ref>]`                               | Set (or with no arg, clear) the host agent         |
| `/agent new <name> [k=v ...]`                       | Create a Living Agent (see `docs/` for fields)     |
| `/agent set <ref> <field> <value>`                  | Edit one field on a Living Agent's config          |
| `/agent hosted`                                     | List Living Agents hosted on this peer             |
| `/agent delete <ref>`                               | Unregister a Living Agent locally (DB preserved)   |
| `/agent share <ref>`                                | Share an agent DB via ticket                       |
| `/agent import <ticket>`                            | Sync + register an agent DB from a ticket          |
| `/pubkey`                                           | Print this peer's pubkey (for `/agent invite`)     |
| `/agent invite <ref> <pubkey> [admin\|write\|read]` | Authorise another peer to co-own an agent          |
| `/agent revoke-peer <ref> <pubkey>`                 | Revoke a co-owner's access                         |
| `/heartbeat list`                                   | List heartbeat rules on this session               |
| `/heartbeat add <id> <cron(6 fields)> <ref> <task>` | Upsert a cron-driven heartbeat rule                |
| `/heartbeat remove <id>`                            | Remove a heartbeat rule                            |
| `/extensions [list]`                                | List extensions on this peer + active status       |
| `/extensions add <name>`                            | Activate an extension on this session              |
| `/extensions remove <name>`                         | Deactivate an extension on this session            |
| `/extensions settings <name>`                       | Show per-session settings JSON for an extension    |
| `/extensions set <name> <key> <value>`              | Merge a key into an extension's per-session config |
| `/clear`                                            | Clear display (entries remain in DB)               |
| `/raw`                                              | Dump raw entry data for debugging                  |
| `/debug`                                            | Toggle debug mode (also Ctrl+D)                    |
| `/quit`, `/q`                                       | Exit                                               |

#### TUI Key Bindings

| Key               | Action                                            |
| ----------------- | ------------------------------------------------- |
| `Ctrl+D`          | Toggle debug mode (shows timestamps, entry types) |
| `Ctrl+C`          | Quit                                              |
| `Up/Down`         | Scroll messages                                   |
| `PageUp/PageDown` | Fast scroll (20 lines)                            |
| `Home/End`        | Move cursor in input                              |

## Session Sharing

Chaz instances can share sessions over the network using eidetica's sync protocol. This enables:

- Viewing a remote agent's conversation from a local TUI
- Multiple instances collaborating on the same session
- Real-time updates — writes from either side propagate automatically

### How It Works

Each chaz instance starts an HTTP sync server automatically. Sessions are shared via **database tickets** — URLs that encode the session's database ID and the server's address.

### Sharing a Session

On the instance that has the session:

```
/share
```

This prints a ticket URL like:

```
eidetica:?db=sha256:abc123...&pr=http:127.0.0.1:12345
```

### Syncing a Remote Session

On another instance, paste the ticket:

```
/sync eidetica:?db=sha256:abc123...&pr=http:192.168.1.10:12345
```

After syncing, use `/sessions` to find and open the synced session. New messages on either side will propagate automatically.

### Notes

- Both instances must be able to reach each other over the network
- The sync server binds to a random port by default (logged at startup)
- Sessions synced from Matrix will appear in the TUI session list

## Matrix Commands

When in a Matrix room, Chaz responds to `!chaz` prefixed commands. In DMs, it responds to every message.

```
!chaz help      — Show available commands
!chaz print     — Print the conversation context
!chaz send <msg> — Send a message without context
!chaz model <m> — Select the model to use
!chaz backend <name> <api_base> <api_key> — Add a custom backend
!chaz role [<role>] [<prompt>] — Get/set/define roles
!chaz list      — List available models
!chaz clear     — Ignore messages before this point
!chaz rename    — Rename the room based on conversation
!chaz agents    — List agents attached to this session
!chaz agent add <ref>     — Attach an agent to this session
!chaz agent remove <ref>  — Detach an agent
!chaz agent host [<ref>]  — Set or clear the host agent
!chaz agent new <name> [k=v ...]            — Create a Living Agent
!chaz agent set <ref> <field> <value>       — Edit one field
!chaz agent hosted                          — List hosted Living Agents
!chaz agent delete <ref>                    — Unregister locally (DB preserved)
!chaz agent share <ref>                     — Share an agent via ticket
!chaz agent import <ticket>                 — Sync + register an agent DB
!chaz pubkey                                — Print this peer's pubkey
!chaz agent invite <ref> <pubkey> [admin|write|read] — Authorise a co-owner
!chaz agent revoke-peer <ref> <pubkey>      — Revoke a co-owner
!chaz heartbeat list                                         — List heartbeat rules
!chaz heartbeat add <id> <cron(6 fields)> <ref> <task...>    — Upsert a heartbeat rule
!chaz heartbeat remove <id>                                  — Remove a rule
!chaz attach <session>    — Bind this room to a session
!chaz detach              — Unbind this room
!chaz channels  — List rooms attached to this session
```

## Configuration

Create a YAML config file:

```yaml
homeserver_url: https://matrix.org
username: "chaz"
password: ""
allow_list: "@user:matrix.org" # Regex for allowed accounts
state_dir: "/path/to/state" # Persistence directory

# LLM backends (OpenAI-compatible)
backends:
  - name: openrouter
    type: openaicompatible
    api_key: "${OPENROUTER_API_KEY}" # Supports env var references
    api_base: https://openrouter.ai/api/v1
    models:
      - name: anthropic/claude-sonnet-4

# Agent definitions
agents:
  - name: chaz
    system_prompt: "You are Chaz, a helpful Matrix assistant."
    allowed_tools: null # null = all tools
    # Worker templates — invocable from this Agent via `spawn_worker(name=…)`.
    workers:
      - name: researcher
        system_prompt: "You are a focused research assistant. Use web_fetch and calculate to answer questions concisely."
        max_iterations: 20
        tools: ["web_fetch", "calculate", "get_time"]

# Security settings
security:
  auto_approved_tools:
    ["get_time", "calculate", "read_file", "remember", "recall"]
  tool_policies:
    shell:
      approval: Always
      grants:
        shell:
          allow: ["ls", "cat", "grep", "find"]
          deny: ["rm", "sudo"]
    web_fetch:
      grants:
        network:
          endpoints:
            - host: "api.example.com"
          allow_private: false # SSRF blocking (default)

# Per-agent grant overlays are merged per-kind over the config above:
# agents:
#   - name: researcher
#     grants:
#       web_fetch:
#         network:
#           endpoints:
#             - host: "*.wikipedia.org"
```

## Install

### Nix (Recommended)

```bash
nix run github:arcuru/chaz
```

The flake contains an overlay and a home-manager module:

```nix
{inputs, ... }: {
  imports = [ inputs.chaz.homeManagerModules.default ];
  services.chaz = {
    enable = true;
    settings = {
      homeserver_url = "https://matrix.jackson.dev";
      username = "chaz";
      password = "hunter2";
      allow_list = "@me:matrix.org";
    };
  };
}
```

### Docker

```yaml
services:
  chaz:
    image: arcuru/chaz:main
    restart: unless-stopped
    network_mode: host
    volumes:
      - ./config.yaml:/config.yaml
      - chaz-state:/state
    # Cap log size — chaz can be chatty under tracing=debug
    logging:
      driver: "json-file"
      options:
        max-size: "1m"
        max-file: "1"

volumes:
  chaz-state:
```

### From Source

```bash
cargo build --release
./target/release/chaz --config config.yaml
```

## Development

Uses Nix flakes with direnv:

```bash
direnv allow        # or: nix develop .#
just build          # cargo build
just test           # cargo test
just lint           # clippy + lints
just fmt            # treefmt
just ci             # all checks
```

## Repository

**Mirrored on [GitHub](https://github.com/arcuru/chaz) and [Codeberg](https://codeberg.org/arcuru/chaz). GitHub is the official repo, but use either repo to contribute.**
