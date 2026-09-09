# Agent State Admin Capability

`AgentStateAdmin` gives extensions access to agent-owned state such as schedules,
memory, and configuration. It is a guardrail against accidental cross-agent access,
not a security sandbox for hostile code.

## Scope

The `schedule` extension uses this capability for its schedule tools and
`/schedule` command. It receives a `ScopedAgentStateAdmin` handle rather than a
`HostedIndex` or direct database access. The handle resolves agent names or IDs
and opens only the agent databases within its configured scope.

The scope comes from the top-level `agent_state_allowlist` configuration map.
Each key is an extension name:

```yaml
agent_state_allowlist:
  schedule: [chaz, bash]
```

With this configuration, the schedule extension can access the `chaz` and `bash`
agents only.

- No `schedule` entry allows the schedule extension to access every hosted agent.
- A non-empty list allows the named agents only.
- An empty list, `schedule: []`, allows no agents.

The map is peer-local configuration read during startup. There is no runtime
command to change it. Extensions read their own map entry when they instantiate
and construct their scoped handle from it.

## Empty allowlists

An empty list makes every lookup fail as if the named agent does not exist. That
keeps agents outside the extension's scope private, but it can hide a mistaken
`[]` configuration.

At startup, an extension with an empty list logs a warning such as:

```text
extension 'schedule' has an empty `agent_state_allowlist.schedule` list; it cannot access any agent state. Remove the list to allow all agents, or add the agents it should access
```

Remove the entry to restore unrestricted access, or add the agents the extension
should access.

## Error handling

A request for an unknown agent and a request for an agent outside the allowlist
both return:

```text
No hosted agent matches '<ref>'
```

Returning the same error prevents extension tools from discovering agents outside
their scope. `open_agent_db` checks the scope again, so a caller cannot bypass it
by passing an entry that was resolved elsewhere.

## Implementation

`ScopedAgentStateAdmin` stores an optional set of allowed display names:

- `None` means no allowlist entry and permits every hosted agent.
- `Some(names)` permits only those names.
- `Some(empty)` denies every agent.

The schedule extension builds this handle from
`PeerHandles.agent_state_allowlist` during instantiation. The capability request
variant in `caps.rs` remains manifest vocabulary, but no extension currently
uses it to set agent-state scope.

## Tests

The `agent_state.rs` tests cover:

- resolution by agent name and database ID;
- rejection of agents outside the allowlist;
- a second scope check before opening a database;
- unrestricted and empty allowlists; and
- warnings for empty lists only.
