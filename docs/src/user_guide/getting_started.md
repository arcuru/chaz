# Getting Started

This guide walks you through setting up and running chaz.

## Prerequisites

- An OpenAI-compatible LLM API key (e.g., [OpenRouter](https://openrouter.ai/))
- For Matrix mode: a Matrix account for the bot
- Rust toolchain or Nix (for building from source)

## Installation

### Nix (Recommended)

```bash
nix run github:arcuru/chaz -- --config config.yaml --tui
```

### From Source

```bash
git clone https://github.com/arcuru/chaz
cd chaz
nix develop .#   # or install pkg-config, openssl, sqlite manually
cargo build --release
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

The TUI is the easiest way to get started:

```bash
chaz --config config.yaml --tui
```

You'll see a terminal interface with an input bar at the bottom and a status bar showing the current session and agent. Type a message and press Enter to chat.

Type `/help` to see available commands.

## Running the Matrix Bot

```bash
chaz --config config.yaml
```

The bot will log in to Matrix, accept room invites from allowed users, and respond to messages. In DMs it responds to everything; in group rooms it responds to `!chaz` prefixed messages.

See [Matrix Bot](matrix.md) for details.

## Troubleshooting

If something isn't working, increase log verbosity:

```bash
RUST_LOG=debug chaz --config config.yaml --tui 2> chaz.log
```

See [Logging](logging.md) for details on log levels and filtering.

## Next Steps

- [Configuration](configuration.md) for full config reference
- [TUI Mode](tui.md) for TUI commands and features
- [Tools](tools.md) for the built-in tool set
- [Agents](agents.md) for multi-agent orchestration
- [Session Sharing](session_sharing.md) for syncing sessions between instances
- [Logging](logging.md) for observability and debugging
