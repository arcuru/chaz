# Getting Started

This guide walks you through setting up and running chaz.

## Prerequisites

- An OpenAI-compatible LLM API key (e.g., [OpenRouter](https://openrouter.ai/))
- For Matrix mode: a Matrix account for the bot
- Rust toolchain or Nix (for building from source)

## Installation

### Nix (Recommended)

```bash
nix run github:arcuru/chaz -- --config config.yaml
```

### From Source

Chaz is rustls-based and only needs a Rust toolchain plus `pkg-config` at build time (no system OpenSSL or SQLite). The Nix dev shell provides everything.

```bash
git clone https://github.com/arcuru/chaz
cd chaz
nix develop .#         # or install rustc + cargo + pkg-config yourself
cargo build --release  # binary at ./target/release/chaz
```

The trailing `.#` on `nix develop` is required — without it Nix picks up eidetica's flake from the workspace.

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

volumes:
  chaz-state:
```

## Minimal Configuration

Create a `config.yaml`:

```yaml
# For TUI-only testing, this is all you need:
backends:
  - name: openrouter
    type: openaicompatible
    api_key: "${OPENROUTER_API_KEY}"
    api_base: https://openrouter.ai/api/v1
    models:
      - name: anthropic/claude-sonnet-4

# For Matrix, add these:
homeserver_url: https://matrix.org
username: "my-bot"
password: "hunter2"
allow_list: "@myuser:matrix.org"

# Optional: persistence directory (default: $XDG_STATE_HOME/chaz)
state_dir: "/path/to/state"
```

Set your API key:

```bash
export OPENROUTER_API_KEY="sk-or-..."
```

## Running the TUI

The TUI is the default — just run chaz with no other mode flag:

```bash
chaz --config config.yaml
```

You'll see a terminal interface with an input bar at the bottom and a status bar showing the current session and agent. Type a message and press Enter to chat.

You can also pass an initial prompt as a positional argument; it pre-fills the input box so you can review and send it:

```bash
chaz --config config.yaml "Summarize the last meeting notes."
```

Type `/help` to see available commands.

## Running the Matrix Bot

```bash
chaz --config config.yaml --matrix
```

The bot will log in to Matrix, accept room invites from allowed users, and respond to messages. In DMs it responds to everything; in group rooms it responds to `!chaz` prefixed messages or messages that @-mention the bot.

See [Matrix Bot](matrix.md) for details.

## Single-shot print mode

For scripted use or scheduling, run a single prompt and exit with `-p` / `--print`:

```bash
chaz --config config.yaml -p "Summarize the last meeting notes."
```

There is no interactive approval — tools requiring approval are auto-denied unless they're in the print-mode auto-approved list (default: `shell`, `write_file`; override with the `cli:` config block). Pass `--session NAME` to reuse a named session across invocations instead of creating a fresh ephemeral one each time. Logs go to a rolling file in the state directory (`chaz-cli.log`); only the agent's reply goes to stdout so the output is pipe-friendly.

## Aggregated cost / usage

```bash
chaz --config config.yaml usage           # human-readable rollup
chaz --config config.yaml usage --json    # for piping
chaz --config config.yaml usage --gateway cli --active-only
```

Walks every session in the state directory and aggregates LLM token/cost metadata. Read-only — no gateway, scheduler, or sync starts. See [Cost Tracking & Usage](usage.md).

## Troubleshooting

If something isn't working, increase log verbosity:

```bash
RUST_LOG=debug chaz --config config.yaml 2> chaz.log
```

See [Logging](logging.md) for details on log levels and filtering.

## Next Steps

- [Configuration](configuration.md) for full config reference, including the optional `embedding:` block for semantic memory recall
- [TUI Mode](tui.md) for TUI commands and features
- [Tools](tools.md) for the built-in tool set
- [Agents](agents.md) for multi-agent orchestration
- [Memory](memory.md) for self-memory, shared banks, and how `recall` ranks results (BM25 + optional embeddings)
- [Session Sharing](session_sharing.md) for syncing sessions between instances
- [Logging](logging.md) for observability and debugging
