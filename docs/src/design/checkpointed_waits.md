# Checkpointed Job Waits

> **Status: Implemented.** User guide: [Resuming a checkpointed wait after restart](../user_guide/agents.md#resuming-a-checkpointed-wait-after-restart). Mechanics: [Agent jobs](../architecture/sessions.md#agent-jobs-and-worker-children).

## Problem

A parent Agent turn that calls `job_wait` spends most of its life observing a child job. Before this change, an executor restart during that observation left the parent `Interrupted`, the same as a turn stopped in the middle of an effectful tool. Someone had to inspect it and `/retry` it. A retry starts a new attempt at the request. The model can repeat an earlier tool effect, including submitting a second child.

Observation alone does not change the child. To repeat just that read safely, the executor needs to know which call was observing which child, whether its result was already being processed, and how much time remains.

## Decision

Checkpoint `job_wait` in the **waiting parent**, not the job being observed. `spawn_agent` has already returned the child DB ID. Resuming the parent means observing that same job and pairing the outstanding call with its result before the next model request. It never means resubmitting the job.

- One versioned `job_wait_checkpoints` row identifies the attempt, model sequence and call index. It retains the child ID, post-hook arguments, context snapshot and original deadline.
- A strict loader. It accepts an attempt only when the attempt's transcript is contiguous and paired and ends at that wait with no result. An interrupted turn without such a checkpoint stays held for `/retry`.
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

If the waiting parent is itself an Agent job, its resumed segment is a new execution branch naming its exact checkpoint and predecessor branch, so it is lineage, not a competitor. It keeps its lineage root's order against other competitors. Two resumes of the same predecessor compete, and the later one in Eidetica's entry order yields; this ordering is cooperative, not a fence, and effects can overlap before a yield is observed. Only the home executor acts on a checkpoint in a session it drives. A checkpoint is not an adoption request for a `spawn:` child. Deadlines use wall-clock time. An eligible in-process parent unloads during observation using the same checkpoint and loader; a bounded descriptor retains its session reservation. Turns with unrecorded hook/approval/input state or a Worker completion channel remain resident with capacity yield. This is exact-wait suspension, not general turn replay.

### Why a Worker wait does not resume

A synchronous `spawn_worker` waits on an in-process completion channel. A `worker_wait` on a background Worker reads the linked outcome. Both can yield capacity, but neither call writes the checkpoint that `plan_job_wait_resume` requires. A Worker transcript surviving restart therefore lets an authorized reader **inspect** the result; it does not resume the interrupted parent. See [Inspecting Worker Sessions](worker_sessions.md).
