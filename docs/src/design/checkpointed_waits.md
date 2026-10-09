# Checkpointed Job Waits

> **Status: Implemented.** User guide: [Resuming a checkpointed wait after restart](../user_guide/agents.md#resuming-a-checkpointed-wait-after-restart). Mechanics: [Agent jobs](../architecture/sessions.md#agent-jobs-and-worker-children).

## Problem

A parent Agent turn that calls `job_wait` spends most of its life observing a child job. Before this change, an executor restart during that observation left the parent `Interrupted`, the same as a turn stopped in the middle of an effectful tool. Someone had to inspect it and `/retry` it. A retry reruns the whole turn: it repeats every earlier model request and tool call, and it can submit a second child.

Observation is read-only, so the restart did not make anything ambiguous. The executor only lacked a durable record of which call was waiting, on what, and until when.

## Decision

Persist exactly that record, and resume only that.

- One versioned row per pending wait, keyed by attempt, model sequence and call index, in the parent session DB.
- A strict loader. It accepts an attempt only when the attempt's transcript is contiguous and paired and ends at that wait with no result. Every other started attempt remains held for `/retry`.
- The same attempt ID continues, so the transcript stays one paired group that ordinary context replay accepts after completion.
- The deadline is absolute and fixed when the wait first starts. The live wait uses it too, so restarted and uninterrupted waits behave the same way.
- Current authority is rechecked before any further effect, and each checkpoint is considered at most once per process.

## Alternatives considered

| Alternative                                                            | Why not                                                                                                                                                                                        |
| ---------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Persist the assembled model prompt with the checkpoint                 | Context can be large, and it would sync with every shared session. Rebuilding from the recorded session snapshot yields the same conversation. The system prompt is rebuilt every turn anyway. |
| Resume a wait followed by other calls in the same batch                | Running those not-yet-started calls means choosing approval and hook carryover. That choice is unmade, so such a wait is not checkpointed.                                                     |
| Carry **Approve all** across the restart                               | That widens consent beyond the process the user approved it in. A turn holding it is not checkpointed, and a resumed turn starts without it.                                                   |
| Treat any interrupted attempt with a completed tool group as resumable | An in-flight effect between records is indistinguishable from none. Only the read-only wait is known safe.                                                                                     |
| Restart the relative `timeout_seconds` on resume                       | Restarting would let repeated crashes extend an observation without bound and make timing depend on downtime.                                                                                  |
| Retry the resume on every wake                                         | A deterministic refusal would be re-evaluated forever. A run that stopped again would be resumed again in the same process.                                                                    |

## Limits

The claim on a job parent is still last-writer-wins, not a fence: two executors that both believe they own a job can overlap, as they can for any job turn. Only the home executor acts on a checkpoint. Deadlines use wall-clock time. A waiting parent remains resident while it observes; unloading it is separate work.
