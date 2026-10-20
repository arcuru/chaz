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
- Observation completion retires the checkpoint durably before result hooks run. A crash during a hook or result commit stays held: the missing result does not prove the hook had no effects.

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

A resumed segment is a new execution branch naming its exact checkpoint and predecessor branch, so it is lineage, not a competitor. It keeps its lineage root's order against other competitors. Two resumes of the same predecessor compete, and the later one in Eidetica's entry order yields; this ordering is cooperative, not a fence, and effects can overlap before a yield is observed. Only the home executor acts on a checkpoint. Deadlines use wall-clock time. An eligible in-process parent unloads during observation using the same checkpoint and loader; a bounded descriptor retains its session reservation. Turns with unrecorded hook/approval/input state or a Worker completion channel remain resident with capacity yield. This is exact-wait suspension, not general turn replay.
