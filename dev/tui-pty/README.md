# TUI PTY lifecycle regression

On Linux, from the default development shell with `uv` available:

```sh
nix develop .# -c just tui-pty
```

The recipe builds the actual `chaz` binary and disposable store/service fixtures.
`test_startup.py` puts a test-only Unix-socket relay between the TUI and a
throwaway service owner.
It holds a handshake, a constructor read, or a named transcript read before
forwarding the request.
The first frame, local help/draft editing and quit must work while the gate is
positively held.
Connection quit exits without releasing the gate; write-capable construction
and launch quit restore the terminal before joined work settles.
Login/session errors retain the live view and draft until exit.
The fixture checks persisted session/entry counts and records first-frame and
usable-session timing separately, including the deliberately imposed delay.
It does not claim production latency or a faster storage backend.
`TUI_EVIDENCE` selects the directory for raw PTY, decoded screen, held request,
log and timing artifacts.

Python's standard-library PTY facilities drive a 40×16 terminal, submit a
Unicode prompt (`café`, a wide character, and a combining accent), check that
the existing loopback model stub received it and that its final reply rendered,
resize to 72×24, wait for the wide redraw, then submit another prompt and
check another reply. Typing must wait for that redraw: crossterm's
edge-triggered poll can strand input that arrives together with `SIGWINCH`
until the next keystroke.
No production database, credentials, provider endpoint, or Matrix login is used.
Each run has a disposable HOME/XDG tree, SQLite store, and kernel-assigned port;
the child environment excludes provider keys and replaces model CLIs with
fail-loud stubs.

The same test stalls a model request, dismisses help and command completion
before Esc stops it, and checks `/stop` on another stalled request.
It stops a pending approval without running its tool, then approves a native
shell and verifies that stop prevents its descendant's delayed marker.
Each stop must produce the terminal stopped entry and allow another turn;
the cancelled native tool exchanges replay without orphan call IDs.
The final Ctrl+C while busy must exit successfully, leave the alternate screen,
and restore canonical input and echo.
This covers user-event cancellation of the active session attempt, not rollback
of effects or recursive cancellation of durable child jobs.
Widget snapshots remain the faster rendering tests.

Additional regressions stop the test-owned IPC process beyond the read timeout
while Enter, Session Settings and hub create/open actions are pending; local
Unicode input and navigation must remain live. Two queued `/name` commands are
checked against the owner's persisted metadata, not redraw-byte counts. An
actual stop/restart must retain cached transcript/name/status, reconnect the
owned client runtime without replaying submissions, and preserve the composer.
A test-only Rust entry point under a controlling PTY catches a background panic
and read timeout, draws a live frame, and restores raw mode exactly once at exit.
The screen assertions use pinned `pyte` via the script's `uv` metadata: ratatui
delta frames can split unchanged text between raw cursor moves. Raw first-glyph
and downstream request assertions remain available for latency probes.
`TUI_BINARY` can select an immutable final build for the same acceptance gate.

Two negative controls require missing text to fail when a child exits cleanly
and when a child remains alive beyond a monotonic deadline. Both assert that
cleanup reaped the child; neither treats exit zero as a successful text assertion.
Every wait is bounded, and success and assertion-failure paths close the PTY and
stub listener. Run the recipe repeatedly to exercise fresh-state cleanup.

On lifecycle failure, `target/tui-pty-failures/<timestamp>/` contains `pty.raw`,
`checkpoints.json` (command, executable SHA-256, PID, geometry and observed text),
`requests.json`, and the run's `chaz-tui` log. Inspect the original unittest traceback first; artifact
write errors are reported separately and do not replace it. Negative-control
artifacts are checked inside disposable directories and then deleted.

Interaction regressions request real approval-gated shell tools from the local
stub and explicitly deny each prompt before and after `/join`, including an
open discarded after newer navigation. Pending-exit tests stop only disposable
storage while Settings inline/picker and model edits hold drafts; the first
warning must be visible and only a second explicit quit leaves the terminal.
The job suite closes an authorized observer and reopens it through the hub,
checks draft isolation, and proves observation makes no extra resident model
calls or ordinary session registration.

## Job lifecycle acceptance map

These examples use disposable stores and recording fake backends, not live
providers. A shell call, a Worker child and an Agent job have different lifetimes:

- `shell` waits for a native process. Dropping its wait kills the Unix process
  group; it is not a durable job handle or restartable shell supervisor.
- `spawn_worker` runs a template under its calling Agent, not a separate Agent
  identity. Its synchronous observation can time out or be cancelled without
  cancelling the child; its wait does not resume after executor restart.
- `spawn_agent` publishes a durable session DB handle for the resident executor.
  Closing a parent/client view does not stop that executor. Queued work can be
  adopted after restart; a started attempt stays interrupted unless it has an
  exactly resumable wait checkpoint or receives an explicit retry.

From the workspace root, select a Rust example by the name in the table:

```sh
nix run .#test -- -E 'test(service_job_steering_tool_free_drains_before_terminal)'
```

| Check                                                                           | Existing runnable example and source                                                                                                                                                                                                                                                    |
| ------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Stable handle, inspectable status/result, queued work and executor interruption | `client_submission_publishes_one_child_then_executor_runs_it` in `crates/lib/src/server/tests.rs`: recover the published ID after losing the reply, inspect typed results, hold queued work behind capacity, then interrupt/retry the same job.                                         |
| Bounded wait is observation, not child cancellation                             | `service_job_steering_tool_free_drains_before_terminal` in `crates/lib/src/server/job_monitor_tests.rs`: expire a client wait while the child's model call is gated, inspect the same started attempt, then release it and settle that same job.                                        |
| Steering included once before terminal result; observing adds no model call     | `service_job_steering_follows_complete_native_tool_exchange` in the same file: retry the input ID, verify one inclusion after the paired tool result, preserve the tool ceiling and receipt, and refresh without another execution.                                                     |
| Publication after the closing boundary is not acceptance                        | `service_job_steering_close_race_refuses_late_publication` in the same file: show `NotApplied`, refuse a new late input and finish with only the original call. No completion-recall call is promised.                                                                                  |
| Executor interruption is not parent/client disappearance                        | `service_job_steering_restart_preserves_dispatch_uncertainty` in the same file: stop the executor during an accepted input call, retain uncertainty, and require explicit retry without replaying that input.                                                                           |
| Completed result is never replaced                                              | `branch_receipt_is_never_replaced_and_late_results_stay_evidence` and `post_delivery_discovery_appends_correction_without_mutating_the_receipt` in `crates/lib/src/session/execution_tests.rs`: preserve the delivered receipt despite a competing result or later ordering correction. |
| Worker observation timeout/cancellation does not resubmit or cancel its child   | `worker_wait_balances_capacity_after_error_timeout_cancellation_and_observed_yield` in `crates/lib/src/server/worker_capacity_tests.rs`: release the gated child after the parent observation ends and verify one child call and balanced capacity.                                     |
| Native shell stop has different process containment                             | `a_user_stop_kills_shell_descendants_and_preserves_native_replay` in `crates/lib/src/server/tests.rs`: stop an approved shell, verify its descendant cannot write a delayed marker, then replay the paired cancelled exchange on the next turn.                                         |

For the actual client-process disappearance example, build the existing fixtures
and run only the job PTY suite:

```sh
nix develop .# -c cargo build --bin chaz --example tui_fixture --example tui_service_fixture
nix develop .# -c uv run --no-project dev/tui-pty/test_jobs.py
```

It closes observer and parent tabs, exits the client with the resident job call
still blocked, then releases that call and reopens the **same** completed job.
The result and steering receipt render without another job call; attempting
input on the finished observer cannot reopen it. This is client disconnection,
not executor crash recovery. Jobs show last recorded state, not proven liveness;
these examples do not add remote queue fetching, cancellation authority, or
exactly-once side-effect guarantees.
