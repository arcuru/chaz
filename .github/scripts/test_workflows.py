"""Check the workflow configuration, not a model of GitHub's scheduler."""

from pathlib import Path
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

    def test_normal_pr_checks_are_retained(self):
        steps = {step["name"]: step for step in self.ci["jobs"]["ci"]["steps"]}
        for name, target in (("Lint", "lint"), ("Test", "test"), ("Build", "build")):
            self.assertNotIn("if", steps[name])
            self.assertIn(f".#checks.x86_64-linux.{target}", steps[name]["run"])
        self.assertIn(".#checks.x86_64-linux.treefmt", steps["Lint"]["run"])
        self.assertIn(".#checks.x86_64-linux.ci-policy", steps["Lint"]["run"])

    def test_full_coverage_is_push_or_manual_only(self):
        doc = next(s for s in self.ci["jobs"]["ci"]["steps"] if s["name"] == "Doc")
        self.assertEqual(expression(doc["if"]), FULL_COVERAGE)
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

    def test_ci_does_not_receive_publishing_secrets(self):
        self.assertNotIn("secrets.", self.ci_text)
        self.assertEqual(self.ci["permissions"], {"contents": "read"})
        self.assertEqual(self.publish["permissions"], {"contents": "read"})
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
