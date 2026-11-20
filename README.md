# Chaz

Chaz is an experimental AI agent orchestrator built on [Eidetica](https://github.com/arcuru/eidetica). Run an agent from a local terminal, a script, or a Matrix or Discord conversation. The same agent can use tools, run scheduled tasks, and participate in persistent, shareable sessions.

> **Experimental.** APIs, config, and on-disk state can change. Keep backups and check the upgrade notes before opening existing stores with a new version.

## Start here

Follow [Getting Started](docs/src/user_guide/getting_started.md) to install Chaz, provision a local Eidetica login, and create your first Living Agent. Chaz does not create the Eidetica store or login on startup. Its local YAML specifies the Eidetica connection, execution role, and model backends; agent configuration lives in the Agent DB.

After setup, use the same `config.yaml` for these local entry points:

```bash
chaz --config config.yaml                         # open the TUI session hub
chaz --config config.yaml -p 'Say hello.'         # send one prompt and print the reply
chaz --config config.yaml cmd '/agent hosted'     # list agents without a model call
```

For chat transports, follow the [bridge setup](docs/src/user_guide/bridges.md). Run `chaz-matrix` or `chaz-discord` separately from the Chaz executor; do not put chat credentials in the executor config.

## Explore the docs

- [Feature guide](docs/src/index.md#what-you-can-do) — sessions, agents, tools, scheduling, transport, and related configuration.
- [Extension author reference](docs/src/architecture/extensions.md#author-contract) — hooks, scopes, capabilities, and a minimal built-in extension to run.
- [Architecture](docs/src/architecture/overview.md) — how frontends, execution, and Eidetica fit together.

## Development

```bash
nix develop .#
just doc build     # build the mdBook
just test          # run Cargo tests
just nix full      # full project gate
```

For a focused test from the current source's compiled archive, see [filtered tests](docs/src/user_guide/getting_started.md#filtered-tests-from-a-compiled-archive).

## Community

- [Matrix room: #chaz:jackson.dev](https://matrix.to/#/#chaz:jackson.dev)
- [Original announcement](https://jackson.dev/post/chaz/)
- [GitHub](https://github.com/arcuru/chaz) (primary) and [Codeberg](https://codeberg.org/arcuru/chaz) (mirror)
