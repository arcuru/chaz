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
      approval: Always # Always ask, even if auto-approved
      timeout: 60 # Seconds before execution times out
    web_fetch:
      approval: UnlessAutoApproved
      timeout: 30
```

Approval levels:

- **Never** -- runs without asking
- **UnlessAutoApproved** -- runs if in `auto_approved_tools`, asks otherwise
- **Always** -- always asks the user

In the TUI, approval is an inline y/n/a prompt. In Matrix, unapproved tools time out (Matrix approval UX is planned).

## Capability Grants

Tools access system resources through the **ToolHost** trait — a sandboxed capability boundary. Grants configure _what_ each tool is allowed to do; the host enforces those grants at execution time. The default `NativeToolHost` enforces grants in-process; future hosts (WASM, bubblewrap) will add stronger sandboxing without changing any tool code.

### Shell grants

The `shell` tool's commands are filtered by `allow`/`deny` lists in its grant:

```yaml
security:
  tool_policies:
    shell:
      grants:
        shell:
          allow:
            - ls
            - cat
            - grep
            - find
            - wc
          deny:
            - rm
            - sudo
            - chmod
            - chown
            - dd
```

If `allow` is non-empty, only commands starting with an allowed prefix are permitted. The `deny` list blocks commands regardless of the allowlist.

> **Deprecated**: `security.shell_allowlist` and `security.shell_denylist` are legacy fields. They still work but are converted to shell grants at startup with a deprecation warning. Use `tool_policies.shell.grants.shell` for new configs.

### Network grants

The `web_fetch` tool's HTTP requests are filtered by endpoint patterns:

```yaml
security:
  tool_policies:
    web_fetch:
      grants:
        network:
          endpoints:
            - host: "api.example.com"
              path_prefix: "/v1"
              methods: ["GET", "POST"]
            - host: "httpbin.org"
          allow_private: false
```

Private IP addresses (RFC 1918, loopback, link-local) are always blocked unless `allow_private: true`. Wildcard hosts (`"*.example.com"`) are supported.

> **Deprecated**: `security.allowed_endpoints` is a legacy field. It still works but is converted to a network grant at startup with a deprecation warning. Use `tool_policies.web_fetch.grants.network.endpoints` for new configs.

### Filesystem grants

File read/write path restrictions are configured but enforcement is a stub (not yet active):

```yaml
security:
  tool_policies:
    read_file:
      grants:
        fs:
          allow_read: ["/tmp", "/home/user/projects"]
    write_file:
      grants:
        fs:
          allow_write: ["/tmp", "/home/user/projects"]
```

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
  leak_policy: "redact" # or "block"
```

## XML Tool Output Wrapping

Tool results fed back to the LLM are wrapped in XML delimiters:

```xml
<tool_output tool="shell">
file1.txt
file2.txt
</tool_output>
```

Angle brackets (`<`, `>`) in the tool output are escaped to `&lt;`/`&gt;`, preventing injection attacks where malicious content could break out of the delimiter and inject system-level instructions.

## Prompt Injection Detection

Tool outputs are scanned for prompt injection patterns (role markers, instruction overrides, chat template tokens). Currently warning-only -- detections are logged but not blocked.

## Tool Rate Limiting

Per-tool call frequency can be limited via the `rate_limit` field in tool policies:

```yaml
security:
  tool_policies:
    shell:
      rate_limit: 5 # max 5 calls per minute
    web_fetch:
      rate_limit: 20
```

A sliding-window rate limiter tracks call timestamps per tool within each agent turn. When a tool exceeds its limit, the call is rejected with an informative message including the retry-after time. The LLM receives this as a tool error and can adjust its behavior.

## Secret Management

API keys are stored in eidetica's SecretStore and resolved at the HTTP client boundary. Config supports environment variable references (`"${VAR_NAME}"`). Secrets are never included in LLM context or session entries.

## Agent-Level Controls

- **Tool narrowing**: Each agent definition can restrict available tools via `allowed_tools` (supports glob patterns like `"filesystem.*"`)
- **Transitive narrowing**: Child agents can never have more tools than their parent
- **Spawn permissions**: `can_spawn` controls which agents can be delegated to
- **Depth limiting**: Spawn depth is capped to prevent infinite recursion
- **Concurrency**: Global semaphore limits concurrent LLM calls to 10
- **Memory isolation**: Each agent's own memory lives in its own Living Agent DB (keyed access, enforced by eidetica). Cross-agent sharing requires an explicit `/memory grant` on a shared bank — there is no peer-wide "global" memory
- **Per-session serialization**: Only one agent task runs per session at a time, preventing duplicate responses from concurrent writes
