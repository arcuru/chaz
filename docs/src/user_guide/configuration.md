# Configuration

Chaz is configured via a YAML file passed with `--config`.

## Full Example

```yaml
# Matrix connection (not needed for TUI-only)
homeserver_url: https://matrix.org
username: "chaz"
password: "hunter2"
allow_list: "@user:matrix.org|@other:matrix.org"  # Regex

# Persistence
state_dir: "/path/to/state"  # Default: $XDG_STATE_HOME/chaz

# LLM backends (OpenAI-compatible)
backends:
  - name: openrouter
    type: openaicompatible
    api_key: "${OPENROUTER_API_KEY}"  # Env var reference
    api_base: https://openrouter.ai/api/v1
    models:
      - name: anthropic/claude-sonnet-4
      - name: google/gemini-2.5-pro
  - name: local
    type: openaicompatible
    api_key: "not-needed"
    api_base: http://localhost:11434/v1
    models:
      - name: llama3

# Agent definitions
agents:
  - name: default
    role: chaz
    max_iterations: 10
    allowed_tools: null  # null = all tools
    can_spawn: ["researcher", "coder"]
  - name: researcher
    role: researcher
    max_iterations: 20
    allowed_tools: ["web_fetch", "calculate", "get_time", "remember", "recall"]
  - name: coder
    role: coder
    max_iterations: 15
    allowed_tools: ["shell", "read_file", "write_file", "calculate"]

# Roles (system prompts)
roles:
  - name: chaz
    description: Default assistant
    prompt: "You are Chaz, a helpful AI assistant."
  - name: researcher
    description: Research agent
    prompt: "You are a research assistant. Use web_fetch to find information."
  - name: coder
    description: Coding agent
    prompt: "You are a coding assistant. Read and write files, run shell commands."

# Security
security:
  auto_approved_tools:
    - get_time
    - calculate
    - read_file
    - remember
    - recall
  shell_allowlist: ["ls", "cat", "grep", "find", "wc", "head", "tail"]
  shell_denylist: ["rm", "sudo", "chmod", "chown"]
  allowed_endpoints:
    - host: "api.example.com"
      path_prefix: "/v1"
      methods: ["GET"]
  leak_policy: "redact"  # "redact" (default) or "block"
  tool_policies:
    shell:
      approval: Always
    web_fetch:
      approval: UnlessAutoApproved
      timeout: 30
```

## Backends

Each backend requires a `name`, `type`, `api_base`, and optionally `api_key` and `models`.

The `api_key` field supports environment variable references: `"${VAR_NAME}"` or `"$VAR_NAME"`. Keys are resolved at startup and stored in eidetica's SecretStore. They are never included in LLM context.

When multiple backends are defined, model names are prefixed with the backend name (e.g., `openrouter:anthropic/claude-sonnet-4`). With a single backend, no prefix is needed.

## Agents

Agent definitions control which tools an agent can use, which other agents it can spawn, and its system prompt. See [Agents](agents.md) for details.

## Security

Security settings control tool approval, network access, shell sandboxing, and secret leak detection. See [Security](security.md) for details.

## State Directory

Chaz persists all data in the state directory:

- `eidetica.db` — SQLite database containing all sessions, memory, secrets, and registry
- Headjack session data (Matrix sync token, device keys)

The state directory defaults to `$XDG_STATE_HOME/chaz`. Override with `state_dir` in config.
