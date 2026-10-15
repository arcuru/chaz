# Temporary runner comparison

This is a trial, not a runner migration. It adds two CI/Matrix E2E arms beside
the existing 8-core control, using the same commands and source checkout.

## Scope and check names

The three-runner matrix requires a same-repository `arcuru/chaz` pull request:

| Field                          | Required value                                               |
| ------------------------------ | ------------------------------------------------------------ |
| Head                           | `ci/runner-comparison-20261015`                              |
| Base                           | `ci/cache-cost-trials-20261015`                              |
| Base SHA                       | `654a25be1d95d471948e3cb4a85ebd3bc4faccb0`                   |
| Runners                        | `ubicloud-standard-8`, `ubicloud-standard-4`, `ubuntu-24.04` |
| CI / E2E ceilings              | 30 / 45 minutes per runner                                   |
| E2E compile / harness ceilings | 20 / 10 minutes, inside the 45-minute job                    |

Checkout still uses the normal PR merge ref. The validator compares its SHA and
both raw commit parents with the event's head/base SHAs, including depth-one
checkouts. An unexpected head or advanced parent on this base fails the control
before Nix setup; it does not report a successful skipped check. Other events
and PRs targeting `main` retain only the original 8-core jobs.

The control keeps the exact `CI` and `Matrix E2E` status names. The other four
checks are `CI (trial <runner>)` and `Matrix E2E (trial <runner>)`. Hold also
subscribes to the parent base, preserving `hold / Dependency Hold`. This code
**does not configure branch protection** or make the alternative statuses
required. Inspect all six results and Hold before accepting a comparison;
a green control alone says nothing about either alternative. Do not merge this
temporary trial as a permanent policy change.

The original PR-only workflow cancellation rule stays unchanged. Both jobs run
concurrently on each runner type; there is no serialization, reduced Cargo/Nix
parallelism, skipped check, or disk cleanup to manufacture a fit. Matrix
`fail-fast: false` prevents a resource failure on one arm from cancelling others.

## What the logs measure

The trial shell invokes each original run script with `bash -e`, matching
GitHub's unspecified Linux shell. It preserves exit status, including the
harness's exit-2 diagnostic, and streams JSON lines prefixed `runner-trial`:

- Event head/base/merge identities, flake hashes, runner image, CPU count and attempt.
- Filesystem available bytes/inodes, RAM/swap, per-CPU counters, load, network
  bytes (separate interfaces, including loopback), disk I/O and pressure.
- Samples before/after each run step and every 15 seconds during it, plus wall
  time, child user/system CPU and largest individual child RSS (not aggregate RSS).
  Nix daemon work is outside child rusage: use host counters, not client CPU/RSS,
  to assess Nix builds.
- A final store/target `du` in logs only, bounded to one minute and non-gating.

Nix setup and rust-cache are actions, not shell steps: use their GitHub step
boundaries and native fetch/restore/save logs. The next shell snapshot exposes
cumulative counter changes across those actions, not a detailed time profile.
Use complete post-action logs for cache save bytes/duration; the sampler does
not run through the cache post action. Samples already streamed survive a job
timeout, but final snapshots/footprints may be absent. Sampling can miss brief
resource peaks. Host counters and network bytes do not identify a particular
server or establish causality; correlate them with Nix fetch/build markers and
Cargo output to test network-versus-CPU hypotheses.

Each trial runner has its own rust-cache prefix, including fallback restores,
while the flake/compiler/dependency keys and GitHub PR branch scope still apply.
The ordinary prefix is unchanged outside the stack. Record hit/miss, full key,
compiler and saved bytes; do not assume a cold or warm cache. Signed Nix
substituters can change between attempts. A successful compile followed by a
failed Matrix assertion **does not save** the dependency cache. No artifacts are
uploaded, no cache quota is changed, and publishing/release jobs are untouched.

## Local checks and a trial sequence

1. Before publication, run `uv run --no-project dev/ci-runner-trial/test_telemetry.py -v`
   and `uv run --no-project dev/ci-runner-trial/test_workflow.py -v`, plus actionlint
   and the normal Nix gate. The fixtures make no remote calls.
2. An authorized publisher verifies the parent SHA, reserves the complete event
   envelope, then opens the child against the exact parent branch. A topic push
   alone triggers no workflow; opening, synchronizing or reopening this PR runs
   six CI/E2E jobs plus Hold. Labels/unlabels run only Hold. This workflow does
   not limit the number of remote attempts: each push/rerun needs a new budget
   decision. Local verification is not permission to publish or run it.
3. Retain checkout identities, all job/step times, counter samples, cache evidence
   and the full unchanged Matrix results. Disk exhaustion, OOM, deadline or the
   known Matrix failure is a real result, not permission to skip tests or save
   failure caches. Keep installed runner resources intact.
4. If a job succeeds and demonstrably saves a dependency cache, a separately
   authorized same-input rerun can measure warm reuse. If the parent advances,
   the control fails validation: stop and explicitly re-pin/re-verify the trial,
   rather than silently comparing different inputs. No retry is automatic.

At conservative planning rates, the full PR event envelope is
`75 × ($0.008 + $0.004 + $0) + 360 × $0.002 = $1.62`.
At Standard modeled rates it is `$1.0125`. The 360-minute Hold default is
intentionally pessimistic; observed Hold time is normally seconds. A whole CI
rerun without Hold is `$0.90 / $0.5625`, respectively. Ceil each positive job's
actual duration separately when settling. These are configured-timeout planning
bounds, not provider-enforced caps or invoices. Queue/teardown, billing policy,
other attempts and unrelated work still require monitoring and headroom.
