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
        # BaseLoader preserves the YAML key `on` and expression strings.
        cls.ci = yaml.load(cls.ci_text, Loader=yaml.BaseLoader)
        cls.publish = yaml.load(cls.publish_text, Loader=yaml.BaseLoader)

    def run_checks(self, event, exit_code=0):
        step = next(s for s in self.ci["jobs"]["ci"]["steps"] if s["name"] == "Checks")
        self.assertNotIn("if", step)
        self.assertEqual(step["env"]["CI_EVENT_NAME"], "${{ github.event_name }}")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stub = root / "nix"
            stub.write_text(
                f"#!{sys.executable}\n"
                "import json, os, sys\n"
                "with open(os.environ['NIX_CALLS'], 'a') as calls:\n"
                "    calls.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                "sys.exit(int(os.environ['NIX_EXIT']))\n"
            )
            stub.chmod(0o755)
            calls = root / "calls.jsonl"
            result = subprocess.run(
                [shutil.which("bash"), "-euo", "pipefail", "-c", step["run"]],
                cwd=ROOT,
                env={
                    **os.environ,
                    "PATH": f"{root}{os.pathsep}{os.environ['PATH']}",
                    "CI_EVENT_NAME": event,
                    "NIX_CALLS": str(calls),
                    "NIX_EXIT": str(exit_code),
                },
                capture_output=True,
                text=True,
                timeout=10,
            )
            self.assertTrue(calls.exists(), result.stderr)
            invocations = [json.loads(line) for line in calls.read_text().splitlines()]
        self.assertEqual(len(invocations), 1)
        return result, invocations[0]

    def test_checks_share_one_bounded_build_graph(self):
        for event in ("pull_request", "merge_group", "push", "workflow_dispatch"):
            with self.subTest(event=event):
                result, args = self.run_checks(event)
                self.assertEqual(result.returncode, 0, result.stderr)
                expected = [
                    "build", "--no-link", "--keep-going", "--max-jobs", "2", "--cores", "4",
                    ".#checks.x86_64-linux.lint",
                    ".#checks.x86_64-linux.treefmt",
                    ".#checks.x86_64-linux.ci-policy",
                    ".#checks.x86_64-linux.test",
                    ".#checks.x86_64-linux.build",
                ]
                if event in ("push", "workflow_dispatch"):
                    expected.append(".#checks.x86_64-linux.doc")
                self.assertEqual(args, expected)

    def test_checks_propagate_build_failure(self):
        result, _ = self.run_checks("pull_request", exit_code=42)
        self.assertEqual(result.returncode, 42, result.stderr)

    def test_full_coverage_is_push_or_manual_only(self):
        self.assertEqual(
            expression(self.ci["jobs"]["e2e"]["if"]),
            f"github.repository_owner == 'arcuru' && ({FULL_COVERAGE})",
        )
        self.assertEqual(
            set(self.ci["on"]), {"push", "pull_request", "merge_group", "workflow_dispatch"}
        )
        self.assertEqual(self.ci["on"]["push"]["branches"], ["main"])

    def test_native_main_queue_covers_ci_and_publish(self):
        self.assertEqual(expression(self.ci["concurrency"]["group"]), MAIN_QUEUE)
        self.assertEqual(
            self.ci["concurrency"]["cancel-in-progress"],
            "${{ github.event_name == 'pull_request' }}",
        )
        self.assertNotIn("concurrency", self.publish)
        for job in self.ci["jobs"].values():
            self.assertNotIn("concurrency", job)

    def test_publish_requires_all_successful_main_ci_jobs(self):
        job = self.ci["jobs"]["publish"]
        self.assertEqual(job["needs"], ["ci", "e2e"])
        self.assertEqual(expression(job["if"]), TRUSTED_MAIN)
        self.assertEqual(job["uses"], "./.github/workflows/publish.yml")
        self.assertEqual(job["permissions"], {"contents": "read"})
        # No status override: GitHub's default success() gates the dependency.
        self.assertNotIn("always()", job["if"])

    def test_publisher_is_reusable_with_independent_manual_entry(self):
        self.assertEqual(set(self.publish["on"]), {"workflow_call", "workflow_dispatch"})
        job = self.publish["jobs"]["push-cache"]
        self.assertEqual(expression(job["if"]), TRUSTED_MAIN)
        self.assertEqual(job["environment"], "publish")
        self.assertEqual(job["permissions"], {"contents": "read"})

    def test_ci_checks_do_not_receive_publishing_secrets(self):
        for name in ("ci", "e2e"):
            self.assertNotIn("secrets.", yaml.safe_dump(self.ci["jobs"][name]))
        self.assertEqual(self.ci["permissions"], {"contents": "read"})
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
