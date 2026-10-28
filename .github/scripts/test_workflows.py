"""Check the workflow configuration, not a model of GitHub's scheduler."""

import glob
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[2]
FULL_COVERAGE = "github.event_name == 'push' || github.event_name == 'workflow_dispatch'"
TRUSTED_MAIN = (
    "github.repository == 'arcuru/chaz' && github.repository_owner == 'arcuru' && "
    "github.ref == 'refs/heads/main' && "
    f"({FULL_COVERAGE})"
)
MAIN_QUEUE = (
    "${{ github.repository_owner == 'arcuru' && github.ref == 'refs/heads/main' && "
    f"({FULL_COVERAGE}) && 'ci-dev-main' || "
    "format('ci-{0}-{1}', github.event_name, "
    "github.event.pull_request.number || github.run_id) }}"
)


def expression(value):
    return " ".join(value.split())


class Workflows(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.ci_text = (ROOT / ".github/workflows/ci.yml").read_text()
        cls.e2e_text = (ROOT / ".github/workflows/matrix-e2e.yml").read_text()
        # BaseLoader preserves the YAML key `on` and expression strings.
        cls.ci = yaml.load(cls.ci_text, Loader=yaml.BaseLoader)
        cls.e2e = yaml.load(cls.e2e_text, Loader=yaml.BaseLoader)

    def run_script(self, script, command="nix", env=None, exit_code=0, fail_call=1):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stub = root / command
            stub.write_text(
                f"#!{sys.executable}\n"
                "import json, os, sys\n"
                "with open(os.environ['NIX_CALLS'], 'a') as calls:\n"
                "    calls.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                "with open(os.environ['NIX_CALLS']) as calls:\n"
                "    call_count = sum(1 for _ in calls)\n"
                "if sys.argv[1] == 'path-info':\n"
                "    print('/nix/store/' + 'a' * 32 + '-checked')\n"
                "    sys.exit(int(os.environ.get('NIX_PATH_EXIT', '0')))\n"
                "if sys.argv[1] == 'log':\n"
                "    print('retained check output')\n"
                "    sys.exit(int(os.environ.get('NIX_LOG_EXIT', '0')))\n"
                "sys.exit(int(os.environ['NIX_EXIT']) if call_count == "
                "int(os.environ['NIX_FAIL_CALL']) else 0)\n"
            )
            stub.chmod(0o755)
            calls = root / "calls.jsonl"
            result = subprocess.run(
                [shutil.which("bash"), "-euo", "pipefail", "-c", script],
                cwd=ROOT,
                env={
                    **os.environ,
                    "PATH": f"{root}{os.pathsep}{os.environ['PATH']}",
                    "NIX_CALLS": str(calls),
                    "NIX_EXIT": str(exit_code),
                    "NIX_FAIL_CALL": str(fail_call),
                    "RUNNER_TEMP": str(root),
                    **(env or {}),
                },
                capture_output=True,
                text=True,
                timeout=10,
            )
            self.assertTrue(calls.exists(), result.stderr)
            invocations = [json.loads(line) for line in calls.read_text().splitlines()]
        return result, invocations

    def run_checks(self, event, exit_code=0, log_exit=0, job="ci", info_exit=0):
        step = next(s for s in self.ci["jobs"][job]["steps"] if s["name"] == "Checks")
        self.assertNotIn("if", step)
        if job in ("ci", "main"):
            self.assertEqual(step["env"]["CI_EVENT_NAME"], "${{ github.event_name }}")
        result, invocations = self.run_script(
            step["run"],
            env={"CI_EVENT_NAME": event, "NIX_LOG_EXIT": str(log_exit),
                 "NIX_PATH_EXIT": str(info_exit)},
            exit_code=exit_code,
        )
        main = job != "ci-arm" and event in ("push", "workflow_dispatch")
        system = "x86_64-linux" if main else "aarch64-linux"
        names = ["lint", "treefmt", "ci-policy", "test", "build"]
        if main:
            names.append("doc")
        logs = invocations[1:]
        if job != "ci-arm" and not exit_code:
            self.assertEqual(logs.pop(0), [
                "path-info", *(f".#checks.{system}.{name}" for name in names),
            ])
        self.assertEqual(logs, [
            ["log", f".#legacyPackages.{system}.lint.chaz-clippy"
             if name == "lint" else f".#checks.{system}.{name}"]
            for name in names
        ])
        for name in names:
            self.assertIn(
                f"::group::{name} — replaying stored derivation log; "
                "checks are not being rerun", result.stdout
            )
        self.assertEqual(result.stdout.count("::endgroup::"), len(names))
        return result, invocations[0]

    def test_shared_actions_use_one_release(self):
        pins = []
        for path in (ROOT / ".github").rglob("*"):
            if path.suffix not in (".yml", ".yaml"):
                continue
            for line in path.read_text().splitlines():
                if "uses: arcuru/actions/" not in line:
                    continue
                with self.subTest(path=path, line=line):
                    pin = re.fullmatch(
                        r"\s*uses: arcuru/actions/[^@]+@([0-9a-f]{40}) # (v\d+\.\d+\.\d+)",
                        line,
                    )
                    self.assertIsNotNone(pin)
                    pins.append(pin.groups())
        self.assertTrue(pins)
        self.assertEqual(len(set(pins)), 1, pins)

    def test_hold_expiry_keeps_the_pat_backed_verification_contract(self):
        workflow = yaml.load(
            (ROOT / ".github/workflows/update-hold.yml").read_text(),
            Loader=yaml.BaseLoader,
        )
        self.assertEqual(workflow["permissions"], {
            "contents": "read", "pull-requests": "write",
        })
        job = workflow["jobs"]["update-hold"]
        self.assertEqual(job["if"], "github.repository_owner == 'arcuru'")
        self.assertEqual(job["secrets"], "inherit")
        self.assertEqual(job["with"]["environment"], "automation")
        self.assertNotIn("verify-pins", job["with"])

    def test_prepare_reuses_check_and_test_outputs_before_secrets(self):
        steps = self.ci["jobs"]["main"]["steps"]
        self.assertEqual([s["name"] for s in steps], [
            "Checkout", "Setup Nix", "Checks", "Prepare final outputs",
            "Publish successful outputs",
        ])
        self.assertEqual(steps[2]["run"], self.ci["jobs"]["ci"]["steps"][2]["run"])
        self.assertEqual(steps[2]["timeout-minutes"], "30")
        for status in (0, 42):
            result, calls = self.run_script(steps[3]["run"], exit_code=status)
            self.assertEqual(result.returncode, status, result.stderr)
            self.assertEqual(calls, [[
                "build", "-L", "--no-link", "--max-jobs", "2", "--cores", "0",
                "--print-out-paths", ".#packages.x86_64-linux.default",
                *(f".#legacyPackages.x86_64-linux.test.{name}"
                  for name in ("deps", "artifacts", "archive")),
            ]])

    def test_path_listing_failure_remains_fatal_after_log_replay(self):
        result, _ = self.run_checks("push", job="main", info_exit=42)
        self.assertEqual(result.returncode, 42, result.stderr)
        self.assertEqual(result.stdout.count("::endgroup::"), 6)

    def test_checks_share_one_build_graph_per_platform(self):
        for event in ("pull_request", "merge_group", "push", "workflow_dispatch"):
            with self.subTest(event=event):
                result, args = self.run_checks(event)
                self.assertEqual(result.returncode, 0, result.stderr)
                main = event in ("push", "workflow_dispatch")
                system = "x86_64-linux" if main else "aarch64-linux"
                names = ["lint", "treefmt", "ci-policy", "test", "build"]
                if main:
                    names.append("doc")
                limits = ["--max-jobs", "2", "--cores", "0"] if main else []
                self.assertEqual(args, [
                    "build", "-L", "--no-link", "--keep-going", *limits,
                    *(f".#checks.{system}.{name}" for name in names),
                ])

    def test_pull_requests_check_on_the_persistent_arm_runner(self):
        # GitHub evaluates these; pin the main/other split and both platforms.
        job = self.ci["jobs"]["ci"]
        self.assertEqual(
            expression(job["runs-on"]),
            f"${{{{ fromJSON(({FULL_COVERAGE}) && "
            "'\"ubicloud-standard-8\"' || "
            "'[\"self-hosted\", \"Linux\", \"ARM64\", \"zinc\", \"nix\"]') }}",
        )
        self.assertEqual(
            expression(job["timeout-minutes"]),
            f"${{{{ ({FULL_COVERAGE}) && 30 || 60 }}}}",
        )
        # The ARM runner's preinstalled daemon and store are reused as an
        # untrusted client: only hosted main installs Nix.
        steps = {step["name"]: step for step in job["steps"]}
        self.assertEqual(list(steps), ["Checkout", "Setup Nix", "Checks"])
        self.assertEqual(steps["Checkout"]["with"]["persist-credentials"], "false")
        self.assertEqual(steps["Setup Nix"]["if"], FULL_COVERAGE)
        self.assertEqual(steps["Setup Nix"]["uses"], "./.github/actions/setup-nix")
        self.assertNotIn("with", steps["Setup Nix"])
        for forbidden in ("sudo", "--option"):
            self.assertNotIn(forbidden, steps["Checks"]["run"])

    def test_main_adds_arm_without_changing_required_pr_status(self):
        self.assertEqual(self.ci["jobs"]["ci"]["name"], "CI")
        self.assertNotIn("strategy", self.ci["jobs"]["ci"])
        self.assertNotIn("needs", self.ci["jobs"]["ci"])
        arm = self.ci["jobs"]["ci-arm"]
        self.assertEqual(expression(arm["if"]), TRUSTED_MAIN)
        self.assertEqual(arm["runs-on"], ["self-hosted", "Linux", "ARM64", "zinc", "nix"])
        self.assertEqual(arm["timeout-minutes"], "60")
        self.assertNotIn("needs", arm)  # ARM succeeds before the hosted job starts.
        self.assertEqual([s["name"] for s in arm["steps"]], ["Checkout", "Checks"])
        for job in (self.ci["jobs"]["ci"], arm):
            checkout = job["steps"][0]
            self.assertEqual(checkout["with"], {
                "ref": "${{ github.sha }}", "persist-credentials": "false",
            })
        checks = arm["steps"][1]
        self.assertNotIn("if", checks)
        for code in (0, 42):
            result, args = self.run_checks("push", exit_code=code, job="ci-arm")
            self.assertEqual(result.returncode, code, result.stderr)
            self.assertEqual(args, [
                "build", "-L", "--no-link", "--keep-going",
                *(f".#checks.aarch64-linux.{name}" for name in
                  ("lint", "treefmt", "ci-policy", "test", "build")),
            ])

    def test_main_arm_replays_logs_and_preserves_build_status(self):
        for event in ("push", "workflow_dispatch"):
            for build_status in (0, 1, 42, 100):
                for log_status in (0, 17):
                    with self.subTest(event=event, build=build_status, log=log_status):
                        result, _ = self.run_checks(
                            event, exit_code=build_status,
                            log_exit=log_status, job="ci-arm",
                        )
                        self.assertEqual(result.returncode, build_status, result.stderr)
                        if log_status:
                            self.assertEqual(result.stdout.count("::warning::"), 5)
                            self.assertIn("no test result inferred", result.stdout)
                        else:
                            self.assertEqual(result.stdout.count("retained check output"), 5)
                            self.assertNotIn("::warning::", result.stdout)

    def test_publishing_stays_on_hosted_x86_runners(self):
        main = self.ci["jobs"]["main"]
        self.assertEqual(main["runs-on"], "ubicloud-standard-8")
        self.assertIn(".#packages.x86_64-linux.default", yaml.safe_dump(main))
        self.assertEqual(self.e2e["jobs"]["e2e"]["runs-on"], "ubicloud-standard-8")

    def test_checks_propagate_build_failure(self):
        for event in ("pull_request", "merge_group", "push", "workflow_dispatch"):
            with self.subTest(event=event):
                result, _ = self.run_checks(event, exit_code=42)
                self.assertEqual(result.returncode, 42, result.stderr)

    def test_missing_logs_warn_without_changing_build_status(self):
        for build_status in (0, 1, 42, 100):
            with self.subTest(build_status=build_status):
                result, _ = self.run_checks(
                    "pull_request", exit_code=build_status, log_exit=17
                )
                self.assertEqual(result.returncode, build_status, result.stderr)
                self.assertEqual(result.stdout.count("::warning::"), 5)
                self.assertIn("no test result inferred", result.stdout)

    def test_stored_logs_are_visible_after_success_and_failure(self):
        for build_status in (0, 42):
            with self.subTest(build_status=build_status):
                result, _ = self.run_checks("push", exit_code=build_status)
                self.assertEqual(result.returncode, build_status, result.stderr)
                self.assertEqual(result.stdout.count("retained check output"), 6)
                self.assertNotIn("::warning::", result.stdout)

    def test_full_coverage_is_push_or_manual_only(self):
        self.assertEqual(
            expression(self.e2e["jobs"]["e2e"]["if"]),
            f"github.repository_owner == 'arcuru' && ({FULL_COVERAGE})",
        )
        self.assertEqual(
            set(self.ci["on"]), {"push", "pull_request", "merge_group", "workflow_dispatch"}
        )
        self.assertEqual(self.ci["on"]["push"]["branches"], ["main"])
        self.assertEqual(set(self.e2e["on"]), {"push", "workflow_dispatch"})
        self.assertEqual(self.e2e["on"]["push"]["branches"], ["main"])

    def test_matrix_e2e_is_independent_with_unchanged_budgets(self):
        self.assertEqual(set(self.ci["jobs"]), {"ci", "ci-arm", "main"})
        self.assertEqual(set(self.e2e["jobs"]), {"e2e"})
        self.assertEqual(self.e2e["name"], "Matrix E2E")
        self.assertEqual(
            expression(self.e2e["concurrency"]["group"]),
            "${{ github.ref == 'refs/heads/main' && 'matrix-e2e-main' || "
            "format('matrix-e2e-{0}', github.run_id) }}",
        )
        self.assertEqual(self.e2e["concurrency"]["cancel-in-progress"], "false")
        job = self.e2e["jobs"]["e2e"]
        self.assertNotIn("needs", job)
        self.assertEqual(job["runs-on"], "ubicloud-standard-8")
        self.assertEqual(job["timeout-minutes"], "45")
        steps = {step["name"]: step for step in job["steps"]}
        self.assertEqual(steps["Build binaries"]["timeout-minutes"], "20")
        self.assertEqual(steps["Matrix E2E"]["timeout-minutes"], "10")
        self.assertIn(
            "cargo build --bin chaz --bin chaz-matrix --example e2ee_probe",
            steps["Build binaries"]["run"],
        )
        self.assertIn("dev/matrix-e2e/run.sh --keep --verbose", steps["Matrix E2E"]["run"])

    def test_failed_matrix_run_retains_diagnostics_without_changing_limits(self):
        job = self.e2e["jobs"]["e2e"]
        steps = {step["name"]: step for step in job["steps"]}
        run = steps["Matrix E2E"]
        self.assertEqual(job["timeout-minutes"], "45")
        self.assertEqual(run["timeout-minutes"], "10")
        self.assertIn('env TMPDIR="$RUNNER_TEMP/matrix-e2e"', run["run"])
        self.assertIn("dev/matrix-e2e/run.sh --keep --verbose", run["run"])
        self.assertIn('tee "$RUNNER_TEMP/matrix-e2e/harness.log"', run["run"])
        self.assertIn("rc=${PIPESTATUS[0]}", run["run"])
        self.assertIn('exit "$rc"', run["run"])
        upload = steps["Retain failed Matrix diagnostics"]
        self.assertEqual(upload["if"], "failure()")
        self.assertRegex(upload["uses"], r"^actions/upload-artifact@[0-9a-f]{40}$")
        self.assertEqual(upload["with"]["retention-days"], "7")

    def test_matrix_artifact_selects_logs_and_both_peer_databases_only(self):
        upload = next(
            s for s in self.e2e["jobs"]["e2e"]["steps"]
            if s["name"] == "Retain failed Matrix diagnostics"
        )
        base = "matrix-e2e/chaz-matrix-e2e-fixture/"
        wanted = {
            "matrix-e2e/harness.log",
            *(base + name for name in (
                "register-agent.log", "register-puppet.log", "register-stranger.log",
                "daemon.log", "daemon-after-snapshot.log", "daemon-participation.log",
                "bridge.log", "bridge-restarted.log", "synapse.log", "stub-llm.log",
                "synapse/homeserver.log", "synapse/homeserver.log.1",
                "state-daemon/eidetica.db", "state-daemon/eidetica.db-wal",
                "state-daemon/eidetica.db-shm", "state-bridge/eidetica.db",
                "state-bridge/eidetica.db-wal", "state-bridge/eidetica.db-shm",
            )),
        }
        excluded = {
            base + name for name in (
                "daemon.yaml", "bridge.yaml", "synapse/homeserver.yaml",
                "state-daemon/eidetica.db.eidetica-owner",
                "state-bridge/eidetica.db.eidetica-owner",
                "synapse/homeserver.db", "synapse/e2e.test.signing.key",
                "state-bridge/matrix/_agent_e2e_test/session",
                "state-bridge/matrix/_agent_e2e_test/crypto-store/matrix-sdk-crypto.sqlite3",
                "probe-signed/session", "probe-unsigned/session",
                "xdg-config/chaz/shares/ticket", "home/.config/credentials",
            )
        } | {"unrelated/eidetica.db", "unrelated/production.log"}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in wanted | excluded:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.touch()
            selected = {
                str(Path(path).relative_to(root))
                for pattern in upload["with"]["path"].splitlines()
                for path in glob.glob(pattern.replace("${{ runner.temp }}", directory))
            }
            self.assertEqual(selected, wanted)

    def test_native_main_queue_covers_ci_and_publish(self):
        self.assertEqual(expression(self.ci["concurrency"]["group"]), MAIN_QUEUE)
        self.assertEqual(
            self.ci["concurrency"]["cancel-in-progress"],
            "${{ github.event_name == 'pull_request' }}",
        )
        for job in self.ci["jobs"].values():
            self.assertNotIn("concurrency", job)

    def test_main_requires_successful_arm_before_any_hosted_step(self):
        job = self.ci["jobs"]["main"]
        self.assertEqual(job["needs"], ["ci-arm"])
        self.assertEqual(expression(job["if"]), TRUSTED_MAIN)
        self.assertEqual(job["environment"], "publish")
        self.assertEqual(job["permissions"], {"contents": "read"})
        self.assertEqual(job["timeout-minutes"], "60")
        # No always()/status override: failed, skipped or cancelled ARM cannot
        # start x86. Ordinary step success then gates final preparation and copy.
        for check in (job, self.ci["jobs"]["ci-arm"]):
            self.assertNotIn("continue-on-error", check)
            for step in check["steps"]:
                self.assertNotIn("continue-on-error", step)
                self.assertNotIn("if", step)
        condition = self.ci["jobs"]["ci"]["if"]
        self.assertEqual(condition, condition.strip())
        self.assertEqual(expression(condition), f"${{{{ !({TRUSTED_MAIN}) }}}}")

    def test_publisher_has_no_separate_manual_or_artifact_bypass(self):
        self.assertFalse((ROOT / ".github/workflows/publish.yml").exists())
        self.assertNotIn("workflow_call", self.ci["on"])
        self.assertNotIn("workflow_run", self.ci["on"])
        self.assertNotIn("download-artifact", self.ci_text)
        steps = self.ci["jobs"]["main"]["steps"]
        self.assertEqual(steps[0]["with"], {
            "ref": "${{ github.sha }}", "persist-credentials": "false",
        })
        self.assertEqual(steps[-1]["run"], "bash .github/scripts/publish-cache.sh")

    def test_ci_checks_do_not_receive_publishing_secrets(self):
        for job in (self.ci["jobs"]["ci"], self.ci["jobs"]["ci-arm"], self.e2e["jobs"]["e2e"]):
            self.assertNotIn("secrets.", yaml.safe_dump(job))
            self.assertNotIn("environment", job)
        self.assertEqual(self.ci["permissions"], {"contents": "read"})
        self.assertEqual(self.e2e["permissions"], {"contents": "read"})
        main = self.ci["jobs"]["main"]
        self.assertNotIn("env", main)
        for step in main["steps"][:-1]:
            self.assertNotIn("secrets.", yaml.safe_dump(step))
        self.assertEqual(main["steps"][-1]["env"], {
            "AWS_ACCESS_KEY_ID": "${{ secrets.NIX_CACHE_ACCESS_KEY_ID }}",
            "AWS_SECRET_ACCESS_KEY": "${{ secrets.NIX_CACHE_SECRET_ACCESS_KEY }}",
            "NIX_CACHE_SIGNING_KEY": "${{ secrets.NIX_CACHE_SIGNING_KEY }}",
        })
        setup = yaml.load(
            (ROOT / ".github/actions/setup-nix/action.yml").read_text(),
            Loader=yaml.BaseLoader,
        )
        self.assertNotIn("inputs", setup)
        self.assertNotIn("outputs", setup)
        self.assertNotIn("secrets.", yaml.safe_dump(setup))


if __name__ == "__main__":
    unittest.main()
