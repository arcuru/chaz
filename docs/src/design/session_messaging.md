# Session Messaging Primitive

## Original proposal

The original design proposed one invocation path: write a Message or Directive
into a session and let the server's write callback start the Agent. At the time,
`spawn_agent` called the runtime directly. Routing children through the server
would give bridge messages and spawned requests the same session history.

In the proposed synchronous flow, the tool registered a child runtime, wrote a
Directive, waited on a completion channel, then read the child response back to
the parent. **That is not the current `spawn_agent` contract.** Nor does the
sequence describe Worker creation: for a recorded `spawn_worker` the Directive
and parent link precede runtime installation.

## Which path runs today?

| Source                | Request and execution                                                                                                                  | Result or observation                                                                                               |
| --------------------- | -------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| Ordinary session turn | A processable Message or Directive is durable. A write wakes reconciliation; the executor records the attempt before running the turn. | Clients observe the attempt and its final entry in the session.                                                     |
| `spawn_agent`         | One local Agent job with its own child DB. Publication precedes admission; submission does not wait for execution.                     | A child DB handle. `job_status` or `job_wait` observes the job.                                                     |
| `spawn_worker`        | A child Directive under the calling Agent's Worker template. The Worker runtime is installed for that child.                           | Its reply or error by default; with `async: true`, a session handle for same-invocation `worker_wait` if available. |
| Agent-owned schedule  | `Server::fire_agent_schedule` runs the wake prompt as invocation-scoped input, without inserting it as a Directive.                    | The Agent DB records the fire on a best-effort basis; observers do not receive the wake prompt as session history.  |

The callback is a wake-up, not a receipt or an authorization check. Two writes
can arrive before the executor runs. It selects the queued request from the
persisted session state, not just the latest entry. Shared-session Agent
messages follow [mention-gated turns](autonomous_agents.md), not a rule that
all Agent messages invoke every participant.

For an executor-backed turn the model response containing tool calls commits
before the tools execute. Each full tool result commits before the next model
request. The `ToolCall` and `ToolResult` entries are for display; native paired
transcript records carry the full exchange. Neither a display entry nor a
write callback is evidence that an interrupted call can safely run again.

See [Durable turn recovery](../architecture/sessions.md#durable-turn-recovery),
[Agent jobs](../architecture/sessions.md#agent-jobs-and-worker-children),
[Inspecting Worker Sessions](worker_sessions.md) and
[Agent-Owned Schedules](agent_schedules.md) for the respective lifecycles.
