# TUI PTY lifecycle regression

On Linux, from the default development shell with `uv` available:

```sh
nix develop .# -c just tui-pty
```

The recipe builds the actual `chaz` binary and disposable store/service fixtures.
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

The final idle Ctrl+C must exit successfully, leave the alternate screen, and
restore canonical input and echo. This covers the documented **quit** behavior,
not in-flight cancellation or the semantics of cancelling a durable job.
Widget tests remain the faster rendering checks.

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
