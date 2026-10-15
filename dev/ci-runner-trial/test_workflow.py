# /// script
# dependencies = ["PyYAML==6.0.3"]
# ///
"""Compare the trial contract to its fixed parent; no remote calls."""

import json
from pathlib import Path
import re
import subprocess
import sys
import unittest

import yaml

sys.dont_write_bytecode = True
from telemetry import BASE, HEAD, PARENT, REPO

ROOT = Path(__file__).resolve().parents[2]


def load(text):
    # BaseLoader avoids YAML 1.1 interpreting Actions' `on` key as a boolean.
    return yaml.load(text, Loader=yaml.BaseLoader)


def original(path):
    return load(subprocess.check_output(["git", "show", f"{BASE}:{path}"], cwd=ROOT, text=True))


def runners(expression, context):
    # Evaluate only this trial's flat equality conjunction and JSON alternatives,
    # not a general Actions expression engine. Actionlint checks native syntax.
    condition, yes, no = re.fullmatch(
        r"\$\{\{ fromJSON\((.*?) && '(\[.*?\])' \|\| '(\[.*?\])'\) \}\}", expression
    ).groups()
    allowed = True
    for atom in condition.split(" && "):
        left, right = atom.split(" == ")
        value = right[1:-1] if right.startswith("'") else context[right]
        allowed = allowed and context[left] == value
    return json.loads(yes if allowed else no)


class WorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.ci = load((ROOT / ".github/workflows/ci.yml").read_text())
        cls.old = original(".github/workflows/ci.yml")

    def test_original_commands_actions_deadlines_and_concurrency(self):
        self.assertEqual(self.ci["concurrency"], self.old["concurrency"])
        self.assertEqual(self.ci["permissions"], self.old["permissions"])
        # No aggregate may hide an alternative runner's failure behind the control.
        self.assertEqual(set(self.ci["jobs"]), set(self.old["jobs"]))
        for name, old in self.old["jobs"].items():
            job = self.ci["jobs"][name]
            self.assertEqual(job["timeout-minutes"], old["timeout-minutes"])
            self.assertEqual(job.get("if"), old.get("if"))
            self.assertNotIn("continue-on-error", job)
            self.assertNotIn("needs", job)
            self.assertEqual(job["runs-on"], "${{ matrix.runner }}")
            self.assertEqual(set(job["strategy"]["matrix"]), {"runner"})
            self.assertNotIn("max-parallel", job["strategy"])
            self.assertEqual(job["strategy"]["fail-fast"], "false")
            current = {step["name"]: step for step in job["steps"]}
            for step in old["steps"]:
                actual = dict(current[step["name"]])
                if step["name"] == "Cache E2E dependencies":
                    prefix = actual["with"].pop("prefix-key")
                    self.assertEqual(prefix, "${{ github.base_ref == '" + PARENT + "' && " +
                                     "format('v0-rust-runner-trial-{0}', matrix.runner) || 'v0-rust' }}")
                self.assertEqual(actual, step)
            self.assertEqual(job["defaults"]["run"]["shell"], "${{ github.base_ref == '" + PARENT +
                             "' && 'python3 dev/ci-runner-trial/telemetry.py run {0}' || 'bash -e {0}' }}")
            added = {"Validate runner trial", "Runner disk footprint (logs only)"}
            self.assertEqual([step for step in current if step not in added],
                             [step["name"] for step in old["steps"]])
            footprint = current["Runner disk footprint (logs only)"]
            self.assertEqual(footprint["if"], f"always() && github.base_ref == '{PARENT}'")
            self.assertEqual(footprint["timeout-minutes"], "1")
            self.assertEqual(footprint["continue-on-error"], "true")
            self.assertEqual(current["Validate runner trial"]["if"], f"github.base_ref == '{PARENT}'")
            self.assertNotIn("continue-on-error", current["Validate runner trial"])

    def test_matrix_scope_and_status_names(self):
        context = {"github.event_name": "pull_request", "github.repository": REPO,
                   "github.event.pull_request.head.repo.full_name": REPO,
                   "github.head_ref": HEAD, "github.base_ref": PARENT,
                   "github.event.pull_request.base.sha": BASE}
        for name, job in self.ci["jobs"].items():
            expression = job["strategy"]["matrix"]["runner"]
            self.assertEqual(runners(expression, context),
                             ["ubicloud-standard-8", "ubicloud-standard-4", "ubuntu-24.04"])
            for key in context:
                with self.subTest(job=name, field=key):
                    self.assertEqual(runners(expression, context | {key: "wrong"}), ["ubicloud-standard-8"])
            for event in ("push", "workflow_dispatch", "merge_group", "pull_request_target"):
                self.assertEqual(runners(expression, context | {"github.event_name": event}),
                                 ["ubicloud-standard-8"])
            status = "CI" if name == "ci" else "Matrix E2E"
            self.assertEqual(job["name"], "${{ matrix.runner == 'ubicloud-standard-8' && '" + status +
                             "' || format('" + status + " (trial {0})', matrix.runner) }}")

    def test_trigger_and_trust_boundaries(self):
        self.assertEqual(self.ci["on"]["push"], self.old["on"]["push"])
        self.assertEqual(self.ci["on"]["merge_group"], self.old["on"]["merge_group"])
        self.assertEqual(self.ci["on"]["workflow_dispatch"], self.old["on"]["workflow_dispatch"])
        self.assertEqual(self.ci["on"]["pull_request"]["branches"], ["main", PARENT])
        hold_path = ".github/workflows/dependency-hold.yml"
        hold = load((ROOT / hold_path).read_text())
        old_hold = original(hold_path)
        old_hold["on"]["pull_request"]["branches"].append(PARENT)
        self.assertEqual(hold, old_hold)
        for path in (".github/workflows/publish.yml", ".github/workflows/release-plz.yml",
                     ".github/actions/setup-nix/action.yml", "flake.nix", "flake.lock",
                     "dev/matrix-e2e/run.sh", "dev/matrix-e2e/stub_llm.py"):
            expected = subprocess.check_output(["git", "show", f"{BASE}:{path}"], cwd=ROOT)
            self.assertEqual((ROOT / path).read_bytes(), expected, path)
        text = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertNotIn("upload-artifact", text)
        self.assertNotIn("cache-on-failure", text)
        self.assertNotIn("secrets.", text)

    def test_full_timeout_envelope(self):
        # Per-job configured ceilings, not observed timings or a billing cap.
        jobs = self.ci["jobs"]
        minutes = int(jobs["ci"]["timeout-minutes"]) + int(jobs["e2e"]["timeout-minutes"])
        self.assertEqual(minutes, 75)
        self.assertEqual(jobs["e2e"]["steps"][-3]["timeout-minutes"], "20")
        self.assertEqual(jobs["e2e"]["steps"][-2]["timeout-minutes"], "10")
        # Both paid arms plus Hold's inherited 360-minute reusable default.
        self.assertAlmostEqual(minutes * (0.008 + 0.004) + 360 * 0.002, 1.62)
        self.assertAlmostEqual(minutes * (0.005 + 0.0025) + 360 * 0.00125, 1.0125)


if __name__ == "__main__":
    unittest.main()
