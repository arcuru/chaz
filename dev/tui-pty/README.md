# TUI PTY lifecycle regression

On Linux, from the default development shell with `uv` available:

```sh
nix develop .# -c just tui-pty
```

The recipe builds the actual `chaz` binary and a test-only store provisioner.
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
Widget snapshots remain the faster rendering tests.

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
