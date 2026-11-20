# Getting Started

This guide starts with one local executor, a fresh SQLite store, and no Matrix or Discord account. Run the setup and local commands sequentially; for concurrent clients, use the [shared Eidetica service layout](bridges.md#connection-settings).

## Install the CLIs

You need `chaz`, a compatible `eidetica` CLI, and an OpenAI-compatible endpoint with a model you can use. With Nix, `nix shell github:arcuru/chaz github:arcuru/eidetica#eidetica-bin` puts both commands on PATH. When working from a Chaz checkout, use an Eidetica CLI built from the dependency revision in `Cargo.toml` if the latest CLIs no longer match.

To build Chaz from a checkout, run `nix develop .#`, then `cargo build --release`; the binary is `target/release/chaz`. The dev shell provides the toolchain and system library dependencies.

## Provision a local store

In a private working directory, create a fresh state directory and a config file only your account can read:

```bash
umask 077
mkdir -p state
eidetica daemon --backend sqlite --data-dir "$PWD/state" init --username chaz
touch config.yaml
chmod 600 config.yaml
```

Enter a non-empty password when prompted. `daemon init` creates `state/eidetica.db` and its first account; it does not start a daemon. Do not reinitialize an existing store. Chaz opens and logs into the store you name; it does not provision or migrate it for you.

Save this as `config.yaml`, replacing `/absolute/path/to/state` with the full path to the directory you just created and `your-eidetica-password` with the password you entered:

```yaml
execution: executor
eidetica:
  connection: "sqlite:///absolute/path/to/state/eidetica.db"
  login:
    username: chaz
    password: "your-eidetica-password"
state_dir: "/absolute/path/to/state"

backends:
  - name: openrouter
    type: openaicompatible
    api_key: "${OPENROUTER_API_KEY}"
    api_base: https://openrouter.ai/api/v1
    models:
      - name: anthropic/claude-sonnet-4
```

Set `OPENROUTER_API_KEY` in your shell, or substitute the endpoint, key reference and model for your provider. The backend key supports environment references; `eidetica.login.password` does not. Do not commit this config. If startup reports `SQLite Eidetica store … does not exist`, check the absolute path and provision a fresh store if needed. If login fails, verify the account and its password against the existing store rather than initializing it again.

## Create your first agent

`config.yaml` defines local runner settings, not agents. A fresh store hosts no agents. Create one and check that Chaz can reopen it; these commands do not run a model turn:

```bash
chaz --config config.yaml cmd '/agent new chaz model=anthropic/claude-sonnet-4'
chaz --config config.yaml cmd '/agent hosted'
```

Use the model name from your backend config if you chose a different one. Success reports `Created Living Agent 'chaz'` followed by its DB ID; `/agent hosted` lists it under `Living Agents hosted on this peer:`. With no configured defaults, new sessions attach the first hosted agent by name. For prompts and managed folders, see [Startup and the first agent](agents.md#startup-and-the-first-agent).

## Running the TUI

The TUI is the default. Pass the config explicitly, or save it as `$XDG_CONFIG_HOME/chaz/config.yaml` (usually `~/.config/chaz/config.yaml`) and omit `--config`:

```bash
chaz --config config.yaml
```

It opens on the session hub, a list of your sessions with a pinned **New session** row; launching creates nothing. Press `n` (or select **New session**) to start a conversation, then type a message and press Enter to chat. See [Session Hub](tui.md#session-hub).

You can also pass an initial prompt as a positional argument; it opens a new conversation with the prompt pre-filled, unsent, so you can review and send it. Add `--session NAME` to open (or create) a named session instead:

```bash
chaz --config config.yaml "Summarize the last meeting notes."
chaz --config config.yaml --session standup "Summarize the last meeting notes."
```

Type `/help` to see available commands. Quit the direct-owner TUI before running a separate Chaz command against its local store.

## Running as a daemon

The TUI needs a terminal — it puts one into raw mode and takes over the
alternate screen. To run the same peer without one, use the `daemon`
subcommand:

```bash
chaz --config config.yaml daemon
```

With `execution: executor`, this runs schedules, the routine engine, and the
agent loop with no user interface, logging to stdout for systemd, a container,
or a supervisor to collect. It stops cleanly on Ctrl-C or `SIGTERM`. A daemon
using a `unix://` Eidetica service reconnects after transient service restarts,
tearing down its old runtime before creating the replacement.

Direct-owner transport bridges sync against this process. Service-client
bridges instead connect to the same Eidetica service login.

## Running the Matrix Bot

The Matrix bridge is its own binary, `chaz-matrix`, started separately from
the agent process rather than spawned inside it. After configuring sync and the bridge login using the [Matrix setup](matrix.md#setup), a direct-owner deployment runs these processes separately:

```bash
chaz --config config.yaml daemon &          # the peer that runs agents
chaz-matrix --config matrix-bridge.yaml     # pure Matrix transport
```

The bridge holds the Matrix login and reaches the agent's database through
an access ticket. It runs no agents and calls no LLM itself: inbound messages
are written into session databases, and the daemon's replies sync back out
to the room. For a shared-service setup without a ticket, see
[Connection settings](bridges.md#connection-settings).

The bot logs in to Matrix, accepts room invites from allowed users, and responds to messages. In DMs it responds to everything; in group rooms it responds to `!chaz` prefixed messages or messages that @-mention the bot.

See [Matrix Bot](matrix.md) for setup, including a scripted bring-up.

## Running one command

Run slash commands such as these without opening the TUI:

```bash
chaz --config config.yaml cmd '/agents'
chaz --config config.yaml cmd '/sharing requests'
```

The result goes to stdout, and a command that reports an error exits non-zero,
so scripts can branch on it. Configure `execution: client` with the same
`unix://` service connection to run commands concurrently with the executor.
Daemon-owned ticket/bootstrap and sync-administration commands must instead be
run on the Eidetica owner and fail before mutation from a service client. This
includes memory/skill bank share, unshare, import, and ticket-aware attach;
ordinary bank list, attach-by-name, detach, and configuration remain native
session data operations.

## Single-shot print mode

For scripted use or scheduling, run a single prompt and exit with `-p` / `--print`:

```bash
chaz --config config.yaml -p "Say hello."
chaz --config config.yaml --session demo -p "Remember that this is the demo session."
```

The second command opens or creates a named session. After it exits, `chaz --config config.yaml cmd --session demo '/info'` checks that session without sending another prompt.

There is no interactive approval — tools requiring approval are auto-denied unless they're in the print-mode auto-approved list (default: `shell`, `write_file`; override with the `cli:` config block). Pass `--session NAME` to reuse a named session across invocations instead of creating a fresh ephemeral one each time. Logs go to a rolling file in the state directory (`chaz-cli.log`); only the agent's reply goes to stdout so the output is pipe-friendly.

With `execution: client`, print mode submits the stable session entry and waits
for the configured executor to answer; it never calls the model or tools in the
client process. If the executor restarts after the client writes the prompt but
before it adopts the session, its startup scan recovers the queued turn. If a
prior executor instead stopped after recording an attempt start,
inspect it with `chaz --config config.yaml cmd '/interrupted'` and retry only
after deciding that replay is safe:

```bash
chaz --config config.yaml cmd '/retry <request_id>'
```

`/compact` follows the same topology: the client writes a stable typed command,
the executor performs the one summarization call, and the client prints the
persisted result. A disconnect or 30-second wait timeout reports the command ID;
inspect that durable command rather than submitting a second intent blindly.

## Aggregated cost / usage

```bash
chaz --config config.yaml usage           # human-readable rollup
chaz --config config.yaml usage --json    # for piping
chaz --config config.yaml usage --gateway cli --active-only
```

Walks accessible sessions in the configured Eidetica store and aggregates LLM token/cost metadata. Read-only — no bridge, scheduler, or sync starts. See [Cost Tracking & Usage](usage.md).

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

## Filtered tests from a compiled archive

Run these commands from the root of the Chaz checkout you are editing.

| Command                                                       | Purpose                                                        |
| ------------------------------------------------------------- | -------------------------------------------------------------- |
| `nix run .#test -- -E 'test(test_error_size)'`                | Run matching tests from the current source's compiled archive. |
| `nix run .#test -- -E 'binary(service_client_targeted)' -j 2` | Select a test binary and limit concurrent tests.               |
| `nix run .#test -- --help`                                    | Show nextest's filter, output, and execution options.          |
| `nix build .#test.archive`                                    | Realize the archive without running tests.                     |
| `nix build .#test.default`                                    | Run the full Cargo test check, including doctests.             |

### Compile once, filter many times

The first invocation realizes a nextest archive for this tree, compiling the workspace's tests with the existing Nix toolchain, default features, and Cargo profile.
It reuses Crane's dependency artifacts where available.
Changing only the filter reuses that immutable archive: nextest extracts and runs binaries without invoking Cargo or rebuilding the project.
Changes to Rust sources, Cargo inputs, or snapshot fixtures invalidate the archive; a new invocation builds the changed tree.
New files must be tracked with `git add` before Nix can see them.

Use `.#test` in the actual worktree, not a pinned remote flake or a runner saved from another revision.
The runner remaps source paths and snapshot lookup to the current workspace, supplies Python, TLS certificates, and the OpenSSL library path, and uses a disposable writable home directory.
The CLI integration test uses nextest's relocated binary path when running from an archive.
No writable Cargo target is shared between worktrees, and this app does not seed the dev shell's target directory.

The archive runs unit and integration tests, not doctests.
The full Cargo check reuses its compiled artifacts and still runs doctests.
Filtered runs do **not** replace the final gates:

```bash
nix develop .# -c just fix
nix develop .# -c just nix full
```

### A filtered iteration

1. From the workspace root, select a known test:

   ```bash
   nix run .#test -- -E 'test(=error::tests::test_error_size)' --show-progress=none
   ```

   After the initial build and extraction, nextest reports `1 test run: 1 passed` (plus skipped tests).

2. Run another filter without editing source:

   ```bash
   nix run .#test -- -E 'test(=error::tests::test_display_formatting)' -j 1 --show-progress=none
   ```

   This extracts the same archive and reports `1 test run: 1 passed`, with no compilation.
   Edit source and repeat either command to test the newly compiled version instead.

3. A misspelled filter is a failure, not a successful test run:

   ```bash
   nix run .#test -- -E 'test(=archive_runner_no_such_test)' --show-progress=none
   ```

   Nextest reports `0 tests run: 0 passed` and `error: no tests to run`, exiting non-zero.
   Correct the name or broaden the filter, then check that tests actually ran.
