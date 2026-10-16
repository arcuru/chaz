# Offline timer-to-Agent-job example

Run from the repository root:

```sh
nix run .#test -- --no-capture -E 'test(timer_example::) | test(restart_resumes_only_the_same_wait_and_survives_a_second_crash) | test(downtime_counts_against_the_original_deadline)'
```

This runs two timer-shaped examples and two existing restart tests. Nix may
fetch/build dependencies; the exercise itself makes no network or provider calls.
It uses an in-memory Eidetica service over a disposable Unix socket, separate
same-login client/executor connections, a scripted `MockBackend`, and temporary
report files. Nothing reads production configuration, installs a timer, or changes
the running Chaz. All fixture state disappears when the tests end.

The source is [timer_example.rs](../../crates/lib/src/server/timer_example.rs),
nested in the existing
[wait-resume fixture](../../crates/lib/src/server/wait_resume_tests.rs).
It is executable test code, not a new public client API or a timer wrapper.

## One tick, three acceptance checks

A scheduled caller submits work, keeps the **original job handle**, observes a
bounded wait, then checks the **actual artifact**. The Agent's successful text is
not proof that a report exists or is correct. The fixed fixture tool writes this
content through the normal `ToolHost` file capability:

```markdown
# Offline timer report

Checks: 3
Failures: 0
```

The tool takes no path or content arguments: its output is fixed inside the
disposable fixture directory. Its test-only policy is not a production file-write
approval policy. Only the executor gets the scripted backend; exhaustion fails
locally rather than falling back to a provider.

| Observation                          | Caller decision                                               |
| ------------------------------------ | ------------------------------------------------------------- |
| `Pending` / `Queued` at wait timeout | Keep the handle; this is not completion or cancellation.      |
| `Succeeded` and expected file bytes  | Accept this fixture's report.                                 |
| `Succeeded`, missing/wrong file      | Reject the output, even though the Agent finished.            |
| File exists, job not successful      | Do not accept on the artifact alone.                          |
| Zero/multiple matching catalog jobs  | Submission uncertain; inspect manually, do not auto-resubmit. |
| `Interrupted` after an effect        | Inspect transcript/effect before choosing an explicit retry.  |

## Walkthrough and fault controls

1. **Lose the publication acknowledgement.** Prepare the child DB, publish it,
   and discard the reply. Inspect the catalog and each candidate's persisted
   parent/target/task. In this isolated single-tick example there is one match,
   and the recovered ID must equal the original prepared handle. This is evidence
   inspection, **not** a stable publication key. Zero matches before publication
   is reported as uncertainty, not permission to resubmit.
2. **Observe and accept separately.** With executor capacity held, the bounded
   wait returns a nonterminal state. Release capacity; one scripted tool writes
   the report and the Agent returns `report ready`. Check the same handle's
   `Succeeded` receipt and file bytes. Expected trace:

   ```text
   timer: lost acknowledgement -> inspected original handle; bounded wait -> Succeeded; report bytes accepted
   ```

3. **Reject misleading success.** Delete the file, then replace it with wrong
   content. An exit-only check still accepts the unchanged `Succeeded` receipt;
   the application's check rejects both. The deliberately unsafe republish
   control submits the identical tick again with the executor stopped: it creates
   a second, different handle. Inspection reports two matches rather than picking
   one or publishing a third:

   ```text
   timer: negative control rejected missing/wrong artifact despite Succeeded
   timer: blind-republish control created two handles -> submission uncertain; no automatic retry
   ```

4. **Restart during an exact child wait.** The runner reuses the existing
   `restart_resumes_only_the_same_wait_and_survives_a_second_crash` test: an effect
   runs once, two executor restarts resume the same child/attempt, and the finished
   checkpoint retains the original absolute deadline. The companion
   `downtime_counts_against_the_original_deadline` case proves an expired deadline
   returns current state instead of starting a fresh wait. These are separate
   executor objects connected to the same service; `shutdown` aborts pending
   turns at the fault boundary, not a full host/process-crash or database-loss
   simulation.
5. **Retry an uncertain effect explicitly.** Hold the report tool after its file
   write but before a durable tool result. Restart: the original handle is
   `Interrupted`, the file already exists, and no backend/effect runs automatically.
   Inspect the interrupted attempt's transcript, then deliberately retry its
   request. A new attempt under the same handle writes the file again. The write
   counter is two even though identical overwrites hide the repetition:

   ```text
   timer: interrupted after write -> held; explicit retry -> same handle, new attempt, two writes (not exactly-once)
   ```

A green test run means these assertions held, not that effects are exactly-once,
that catalog matching works for concurrent identical ticks, or that a production
timer can safely replay. When identity cannot be recovered, retain uncertainty
and inspect manually. A stable-key submission contract would be separate work;
this example neither selects nor implements one. Existing
[Agent-job contracts](../../docs/src/user_guide/agents.md#agent-jobs-submit-and-observe)
still apply, including cooperative execution ordering rather than fencing.
