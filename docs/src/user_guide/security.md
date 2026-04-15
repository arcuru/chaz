# Security

Chaz includes multiple security layers to control what agents can do.

## Tool Approval

Each tool has a default approval requirement that can be overridden in config:

```yaml
security:
  # Tools that never need approval (even if their default requires it)
  auto_approved_tools:
    - get_time
    - calculate
    - read_file
    - remember
    - recall

  # Per-tool policy overrides
  tool_policies:
    shell:
      approval: Always     # Always ask, even if auto-approved
      timeout: 60          # Seconds before execution times out
    web_fetch:
      approval: UnlessAutoApproved
      timeout: 30
```

Approval levels:
- **Never** -- runs without asking
- **UnlessAutoApproved** -- runs if in `auto_approved_tools`, asks otherwise
- **Always** -- always asks the user

In the TUI, approval is an inline y/n/a prompt. In Matrix, unapproved tools time out (Matrix approval UX is planned).

## Shell Sandboxing

The `shell` tool filters commands against allowlist and denylist patterns:

```yaml
security:
  shell_allowlist:
    - ls
    - cat
    - grep
    - find
    - wc
  shell_denylist:
    - rm
    - sudo
    - chmod
    - chown
    - dd
```

If an allowlist is defined, only commands starting with an allowed prefix are permitted. The denylist blocks commands regardless of the allowlist.

## Network Controls

The `web_fetch` tool enforces endpoint allowlisting and SSRF protection:

```yaml
security:
  allowed_endpoints:
    - host: "api.example.com"
      path_prefix: "/v1"
      methods: ["GET", "POST"]
    - host: "httpbin.org"
```

Private IP addresses (RFC 1918, loopback, link-local) are always blocked to prevent SSRF attacks.

## Leak Detection

All tool outputs are scanned for secret patterns before entering the LLM context. The detector recognizes 12 patterns including:

- API keys (OpenAI, Anthropic, OpenRouter, GitHub, AWS, Google)
- SSH private keys
- PEM-encoded certificates
- Bearer tokens
- Generic high-entropy strings matching key formats

When a secret is detected:

- **Redact** (default): The secret is replaced with `[REDACTED]` and the output proceeds
- **Block**: The entire tool output is rejected

```yaml
security:
  leak_policy: "redact"  # or "block"
```

## Prompt Injection Detection

Tool outputs are scanned for prompt injection patterns (role markers, instruction overrides, chat template tokens). Currently warning-only -- detections are logged but not blocked.

## Secret Management

API keys are stored in eidetica's SecretStore and resolved at the HTTP client boundary. Config supports environment variable references (`"${VAR_NAME}"`). Secrets are never included in LLM context or session entries.

## Agent-Level Controls

- **Tool narrowing**: Each agent definition can restrict available tools via `allowed_tools`
- **Transitive narrowing**: Child agents can never have more tools than their parent
- **Spawn permissions**: `can_spawn` controls which agents can be delegated to
- **Depth limiting**: Spawn depth is capped to prevent infinite recursion
- **Concurrency**: Global semaphore limits concurrent LLM calls to 10
