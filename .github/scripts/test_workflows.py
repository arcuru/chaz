"""Check the workflow configuration, not a model of GitHub's scheduler."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[2]
FULL_COVERAGE = "github.event_name == 'push' || github.event_name == 'workflow_dispatch'"
TRUSTED_MAIN = (
    "github.repository_owner == 'arcuru' && github.ref == 'refs/heads/main' && "
    f"({FULL_COVERAGE})"
)
MAIN_QUEUE = (
    f"${{{{ {TRUSTED_MAIN} && 'ci-dev-main' || "
    "format('ci-{0}-{1}', github.event_name, "
    "github.event.pull_request.number || github.run_id) }}"
)


def expression(value):
    return " ".join(value.split())


class Workflows(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.ci_text = (ROOT / ".github/workflows/ci.yml").read_text()
        cls.publish_text = (ROOT / ".github/workflows/publish.yml").read_text()
        cls.e2e_text = (ROOT / ".github/workflows/matrix-e2e.yml").read_text()
        # BaseLoader preserves the YAML key `on` and expression strings.
        cls.ci = yaml.load(cls.ci_text, Loader=yaml.BaseLoader)
        cls.publish = yaml.load(cls.publish_text, Loader=yaml.BaseLoader)
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
                    **(env or {}),
                },
                capture_output=True,
                text=True,
                timeout=10,
            )
            self.assertTrue(calls.exists(), result.stderr)
            invocations = [json.loads(line) for line in calls.read_text().splitlines()]
        return result, invocations

    def run_checks(self, event, exit_code=0, log_exit=0):
        step = next(s for s in self.ci["jobs"]["ci"]["steps"] if s["name"] == "Checks")
        self.assertNotIn("if", step)
        self.assertEqual(step["env"]["CI_EVENT_NAME"], "${{ github.event_name }}")
        result, invocations = self.run_script(
            step["run"],
            env={"CI_EVENT_NAME": event, "NIX_LOG_EXIT": str(log_exit)},
            exit_code=exit_code,
        )
        main = event in ("push", "workflow_dispatch")
        system = "x86_64-linux" if main else "aarch64-linux"
        names = ["lint", "treefmt", "ci-policy", "test", "build"]
        if main:
            names.append("doc")
        self.assertEqual(invocations[1:], [
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

    def run_warm_cache(self, exit_code=0, fail_call=1):
        step = next(
            s for s in self.publish["jobs"]["push-cache"]["steps"]
            if s["name"] == "Warm CI cache"
        )
        self.assertNotIn("if", step)
        script = step["run"].replace(
            "${{ steps.nix.outputs.push-args }}", "--copy-to file:///unused-cache"
        )
        return self.run_script(
            script, command="nix-fast-build", exit_code=exit_code, fail_call=fail_call
        )

    def test_publisher_exports_reusable_test_artifacts(self):
        result, invocations = self.run_warm_cache()
        self.assertEqual(result.returncode, 0, result.stderr)
        common = [
            "--skip-cached", "--no-nom", "--no-link",
            "--copy-to", "file:///unused-cache",
        ]
        self.assertEqual(invocations, [
            common,
            common + ["-f", ".#legacyPackages.x86_64-linux.test"],
        ])

    def test_publisher_propagates_each_warm_failure(self):
        for fail_call in (1, 2):
            with self.subTest(fail_call=fail_call):
                result, invocations = self.run_warm_cache(exit_code=42, fail_call=fail_call)
                self.assertEqual(result.returncode, 42, result.stderr)
                self.assertEqual(len(invocations), fail_call)

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
        self.assertNotIn("needs", arm)  # Parallel platforms, one workflow revision.
        self.assertEqual([s["name"] for s in arm["steps"]], ["Checkout", "Checks"])
        for job in (self.ci["jobs"]["ci"], arm):
            checkout = job["steps"][0]
            self.assertEqual(checkout["with"], {
                "ref": "${{ github.sha }}", "persist-credentials": "false",
            })
        checks = arm["steps"][1]
        self.assertNotIn("if", checks)
        for code in (0, 42):
            result, invocations = self.run_script(checks["run"], exit_code=code)
            self.assertEqual(result.returncode, code, result.stderr)
            self.assertEqual(invocations, [[
                "build", "--no-link", "--keep-going",
                *(f".#checks.aarch64-linux.{name}" for name in
                  ("lint", "treefmt", "ci-policy", "test", "build")),
            ]])

    def test_publishing_stays_on_hosted_x86_runners(self):
        publish = self.publish["jobs"]["push-cache"]
        self.assertEqual(publish["runs-on"], "ubicloud-standard-8")
        self.assertIn(".#packages.x86_64-linux.default", yaml.safe_dump(publish))
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
        self.assertEqual(set(self.ci["jobs"]), {"ci", "ci-arm", "publish"})
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
        self.assertIn("dev/matrix-e2e/run.sh --verbose", steps["Matrix E2E"]["run"])

    def test_native_main_queue_covers_ci_and_publish(self):
        self.assertEqual(expression(self.ci["concurrency"]["group"]), MAIN_QUEUE)
        self.assertEqual(
            self.ci["concurrency"]["cancel-in-progress"],
            "${{ github.event_name == 'pull_request' }}",
        )
        self.assertNotIn("concurrency", self.publish)
        for job in self.ci["jobs"].values():
            self.assertNotIn("concurrency", job)

    def test_publish_requires_successful_ordinary_main_ci_only(self):
        job = self.ci["jobs"]["publish"]
        self.assertEqual(job["needs"], ["ci", "ci-arm"])
        self.assertEqual(expression(job["if"]), TRUSTED_MAIN)
        self.assertEqual(job["uses"], "./.github/workflows/publish.yml")
        self.assertEqual(job["permissions"], {"contents": "read"})
        # Exact guard has no status override: default success() requires BOTH
        # dependencies to succeed. Failure, cancellation, skip and missing
        # results therefore cannot enter the reusable publishing workflow.
        self.assertNotIn("continue-on-error", job)
        for dependency in job["needs"]:
            check = self.ci["jobs"][dependency]
            self.assertNotIn("continue-on-error", check)
            for step in check["steps"]:
                self.assertNotIn("continue-on-error", step)

    def test_publisher_has_no_manual_bypass(self):
        self.assertEqual(set(self.publish["on"]), {"workflow_call"})
        job = self.publish["jobs"]["push-cache"]
        self.assertEqual(expression(job["if"]), TRUSTED_MAIN)
        self.assertEqual(job["environment"], "publish")
        self.assertEqual(job["permissions"], {"contents": "read"})

    def test_ci_checks_do_not_receive_publishing_secrets(self):
        for job in (self.ci["jobs"]["ci"], self.ci["jobs"]["ci-arm"], self.e2e["jobs"]["e2e"]):
            self.assertNotIn("secrets.", yaml.safe_dump(job))
        self.assertEqual(self.ci["permissions"], {"contents": "read"})
        self.assertEqual(self.e2e["permissions"], {"contents": "read"})
        self.assertEqual(self.publish["permissions"], {"contents": "read"})
        names = {"NIX_CACHE_ACCESS_KEY_ID", "NIX_CACHE_SECRET_ACCESS_KEY", "NIX_CACHE_SIGNING_KEY"}
        self.assertEqual(
            self.ci["jobs"]["publish"]["secrets"],
            {name: "${{ secrets." + name + " }}" for name in names},
        )
        self.assertEqual(
            self.publish["on"]["workflow_call"]["secrets"],
            {name: {"required": "true"} for name in names},
        )
        steps = self.publish["jobs"]["push-cache"]["steps"]
        setup = next(s for s in steps if s["name"] == "Setup Nix")
        self.assertEqual(setup["with"]["signing-key"], "${{ secrets.NIX_CACHE_SIGNING_KEY }}")

    def test_publisher_rebuilds_the_checked_revision_from_trusted_source(self):
        steps = self.publish["jobs"]["push-cache"]["steps"]
        checkout = next(s for s in steps if s["name"] == "Checkout")
        self.assertEqual(checkout["with"]["ref"], "${{ github.sha }}")
        self.assertEqual(checkout["with"]["persist-credentials"], "false")
        build = next(s for s in steps if s["name"] == "Build and push artifacts")
        self.assertIn("--option substituters 'https://cache.nixos.org'", build["run"])
        self.assertNotIn("download-artifact", self.publish_text)
        self.assertNotIn("workflow_run", self.publish_text)


if __name__ == "__main__":
    unittest.main()
