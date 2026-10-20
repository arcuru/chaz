"""Offline checks of the actual workflow expressions and Bash entry points."""

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
    "github.repository == 'arcuru/chaz' && "
    "github.repository_owner == 'arcuru' && github.ref == 'refs/heads/main' && "
    f"({FULL_COVERAGE})"
)
MAIN_QUEUE = (
    f"${{{{ {TRUSTED_MAIN} && 'ci-dev-main' || "
    "format('ci-{0}-{1}', github.event_name, "
    "github.event.pull_request.number || github.run_id) }}"
)
CACHE_ACTION = "actions/cache/{operation}@0057852bfaa89a56745cba8c7296529d2fc39830"
STORE_PATH = "/nix/store/" + "a" * 32 + "-test-output"


def expression(value):
    return " ".join(value.split())


def evaluate(value, context):
    """Evaluate only the boolean/string subset used by the role/name expressions."""
    value = expression(value).removeprefix("${{ ").removesuffix(" }}")
    for key, item in context.items():
        name = key if key.startswith("steps.") else "github." + key
        value = re.sub(re.escape(name) + r"\b", repr(item), value)
    value = value.replace("&&", " and ").replace("||", " or ")
    value = re.sub(r"!(?!=)", "not ", value)
    # No functions, attributes, indexing or unknown identifiers in this subset.
    self_contained = re.sub(r"'[^']*'", "''", value)
    if re.search(r"[^\s()'!=a-z]", self_contained) or re.search(
        r"\b(?!and\b|or\b|not\b)[a-z]+", self_contained
    ):
        raise ValueError(value)
    return eval(value, {"__builtins__": {}}, {})


class Workflows(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.ci_text = (ROOT / ".github/workflows/ci.yml").read_text()
        cls.publish_text = (ROOT / ".github/workflows/publish.yml").read_text()
        cls.e2e_text = (ROOT / ".github/workflows/matrix-e2e.yml").read_text()
        cls.ci = yaml.load(cls.ci_text, Loader=yaml.BaseLoader)
        cls.publish = yaml.load(cls.publish_text, Loader=yaml.BaseLoader)
        cls.e2e = yaml.load(cls.e2e_text, Loader=yaml.BaseLoader)

    def run_script(self, script, env=None, exit_code=0, fail_call=1):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stub = root / "nix"
            stub.write_text(
                f"#!{sys.executable}\n"
                "import json, os, sys\n"
                "with open(os.environ['NIX_CALLS'], 'a') as calls:\n"
                "    calls.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                "with open(os.environ['NIX_CALLS']) as calls:\n"
                "    count = sum(1 for _ in calls)\n"
                "if count == int(os.environ['NIX_FAIL_CALL']):\n"
                "    sys.exit(int(os.environ['NIX_EXIT']))\n"
                f"print(json.dumps([{{'outputs': {{'out': {STORE_PATH!r}}}}}]))\n"
            )
            stub.chmod(0o755)
            calls = root / "calls.jsonl"
            result = subprocess.run(
                [shutil.which("bash"), "-euo", "pipefail", "-c", script],
                cwd=ROOT,
                env={
                    **os.environ,
                    "PATH": f"{root}{os.pathsep}{os.environ['PATH']}",
                    "RUNNER_TEMP": str(root),
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

    def steps(self, job):
        return {step["name"]: step for step in job["steps"]}

    def run_checks(self, event, exit_code=0):
        step = self.steps(self.ci["jobs"]["ci"])["Checks"]
        self.assertNotIn("if", step)
        result, invocations = self.run_script(
            step["run"], env={"GITHUB_EVENT_NAME": event}, exit_code=exit_code
        )
        self.assertEqual(len(invocations), 1)
        return result, invocations[0]

    def test_checks_share_one_bounded_build_graph(self):
        for event in ("pull_request", "merge_group", "push", "workflow_dispatch"):
            with self.subTest(event=event):
                result, args = self.run_checks(event)
                self.assertEqual(result.returncode, 0, result.stderr)
                expected = [
                    "build", "--fallback", "--no-link", "--keep-going", "--max-jobs", "2", "--cores", "0", "--json",
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

    def test_final_preparation_reuses_exact_test_outputs(self):
        for job in (self.ci["jobs"]["main"], self.publish["jobs"]["push-cache"]):
            step = self.steps(job)["Prepare final outputs"]
            result, invocations = self.run_script(step["run"])
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(invocations, [[
                "build", "--fallback", "--no-link", "--max-jobs", "2", "--cores", "0", "--json",
                ".#packages.x86_64-linux.default", ".#test.deps", ".#test.artifacts", ".#test.archive",
            ]])
            result, _ = self.run_script(step["run"], exit_code=42)
            self.assertEqual(result.returncode, 42, result.stderr)

    def test_required_ci_status_event_role_and_failure_truth_table(self):
        jobs = self.ci["jobs"]
        self.assertEqual(set(jobs), {"ci", "main"})
        self.assertEqual(expression(jobs["main"]["if"]), TRUSTED_MAIN)
        self.assertEqual(expression(jobs["ci"]["if"]), f"!({TRUSTED_MAIN})")
        rows = 0
        for repo, owner in (("arcuru/chaz", "arcuru"), ("arcuru/other", "arcuru"), ("fork/chaz", "fork")):
            for event in ("push", "pull_request", "merge_group", "workflow_dispatch"):
                for ref in ("refs/heads/main", "refs/heads/topic", "refs/pull/3/merge"):
                    ctx = {"repository": repo, "repository_owner": owner, "event_name": event, "ref": ref}
                    names = {key: evaluate(job["name"], ctx) for key, job in jobs.items()}
                    active = [key for key, job in jobs.items() if evaluate(job["if"], ctx)]
                    self.assertEqual(len(active), 1)
                    key = active[0]
                    self.assertEqual(names[key], "CI")
                    self.assertNotEqual(names["main" if key == "ci" else "ci"], "CI")
                    # Default step success() propagates failure/cancellation. A
                    # skipped counterpart is never another check named CI.
                    for conclusion in ("success", "failure", "cancelled", "skipped"):
                        statuses = {names[k]: conclusion if k == key else "skipped" for k in jobs}
                        self.assertEqual(statuses["CI"], conclusion)
                        if conclusion in ("failure", "cancelled"):
                            self.assertNotEqual(statuses["CI"], "success")
                    rows += 1
        self.assertEqual(rows, 36)

    def test_stage_failures_cannot_reach_final_publication(self):
        for job in (self.ci["jobs"]["main"], self.publish["jobs"]["push-cache"]):
            steps = job["steps"]
            names = [step["name"] for step in steps]
            self.assertLess(names.index("Build dependencies"), names.index("Publish dependencies"))
            self.assertLess(names.index("Publish dependencies"), names.index("Checks"))
            self.assertLess(names.index("Checks"), names.index("Prepare final outputs"))
            self.assertLess(names.index("Prepare final outputs"), names.index("Publish successful outputs"))
            for step in steps:
                if step["name"] == "Publish dependencies":
                    self.assertEqual(step["continue-on-error"], "true")
                    self.assertEqual(step["id"], "publish-deps")
                else:
                    self.assertNotIn("continue-on-error", step)
                if step["name"] == "Require dependency publication":
                    self.assertEqual(step["if"], "steps.publish-deps.outcome == 'failure'")
                    self.assertIn("exit 1", step["run"])
                    self.assertLess(names.index("Checks"), names.index(step["name"]))
                    self.assertLess(names.index(step["name"]), names.index("Prepare final outputs"))
                else:
                    self.assertNotIn("if", step)  # default success(), no always() override
            self.assertNotIn("needs", job)
        steps = self.steps(self.ci["jobs"]["ci"])
        for name in ("Dependency identity", "Prefer signed main dependencies", "Build dependencies", "Checks"):
            self.assertNotIn("if", steps[name])
            self.assertNotIn("continue-on-error", steps[name])

    def test_pr_cache_is_exact_key_early_and_optional(self):
        job = self.ci["jobs"]["ci"]
        steps = self.steps(job)
        names = list(steps)
        self.assertLess(names.index("Build dependencies"), names.index("Save PR dependencies"))
        self.assertLess(names.index("Save PR dependencies"), names.index("Checks"))
        for name, operation in (("Restore PR dependencies", "restore"), ("Save PR dependencies", "save")):
            step = steps[name]
            self.assertEqual(step["uses"], CACHE_ACTION.format(operation=operation))
            self.assertEqual(step["continue-on-error"], "true")
            self.assertEqual(step["with"], {
                "path": "${{ runner.temp }}/chaz-deps-cache",
                "key": "${{ steps.identity.outputs.key }}",
            })
            self.assertIn("github.event_name == 'pull_request'", step["if"])
        self.assertIn("steps.trusted.outputs.available != 'true'", steps["Restore PR dependencies"]["if"])
        self.assertIn("steps.restore.outputs.cache-hit == 'true'", steps["Import PR dependencies"]["if"])
        self.assertIn("steps.restore.outputs.cache-hit != 'true'", steps["Export PR dependencies"]["if"])
        self.assertEqual(steps["Save PR dependencies"]["if"], "github.event_name == 'pull_request' && steps.export.outcome == 'success'")
        self.assertNotIn("continue-on-error", steps["Import PR dependencies"])
        for name in ("Dependency identity", "Prefer signed main dependencies", "Build dependencies", "Import PR dependencies", "Export PR dependencies"):
            self.assertIn("bash .github/scripts/deps-cache.sh ", steps[name]["run"])

    def test_pr_cache_hit_miss_error_and_foreign_event_truth_table(self):
        steps = self.steps(self.ci["jobs"]["ci"])
        for event in ("pull_request", "push", "merge_group", "workflow_dispatch"):
            for trusted in ("true", "false"):
                for hit in ("true", "false", ""):
                    ctx = {"event_name": event, "steps.trusted.outputs.available": trusted,
                           "steps.restore.outputs.cache-hit": hit, "steps.export.outcome": "success"}
                    pr_miss = event == "pull_request" and trusted != "true"
                    self.assertEqual(bool(evaluate(steps["Restore PR dependencies"]["if"], ctx)), pr_miss)
                    self.assertEqual(bool(evaluate(steps["Import PR dependencies"]["if"], ctx)), pr_miss and hit == "true")
                    export = bool(evaluate(steps["Export PR dependencies"]["if"], ctx))
                    self.assertEqual(export, pr_miss and hit != "true")
                    for outcome in ("success", "failure", "skipped"):
                        ctx["steps.export.outcome"] = outcome
                        save = bool(evaluate(steps["Save PR dependencies"]["if"], ctx))
                        self.assertEqual(save, event == "pull_request" and outcome == "success")

    def test_full_coverage_is_push_or_manual_only(self):
        self.assertEqual(expression(self.e2e["jobs"]["e2e"]["if"]), f"github.repository_owner == 'arcuru' && ({FULL_COVERAGE})")
        self.assertEqual(set(self.ci["on"]), {"push", "pull_request", "merge_group", "workflow_dispatch"})
        self.assertEqual(self.ci["on"]["push"]["branches"], ["main"])
        self.assertEqual(self.ci["on"]["pull_request"]["branches"], ["main"])
        self.assertEqual(set(self.e2e["on"]), {"push", "workflow_dispatch"})
        self.assertEqual(self.e2e["on"]["push"]["branches"], ["main"])

    def test_matrix_e2e_is_independent_with_unchanged_budgets(self):
        self.assertEqual(set(self.e2e["jobs"]), {"e2e"})
        self.assertEqual(self.e2e["name"], "Matrix E2E")
        self.assertEqual(expression(self.e2e["concurrency"]["group"]), "${{ github.ref == 'refs/heads/main' && 'matrix-e2e-main' || format('matrix-e2e-{0}', github.run_id) }}")
        self.assertEqual(self.e2e["concurrency"]["cancel-in-progress"], "false")
        job = self.e2e["jobs"]["e2e"]
        self.assertNotIn("needs", job)
        self.assertEqual(job["runs-on"], "ubicloud-standard-8")
        self.assertEqual(job["timeout-minutes"], "45")
        steps = self.steps(job)
        self.assertEqual(steps["Build binaries"]["timeout-minutes"], "20")
        self.assertEqual(steps["Matrix E2E"]["timeout-minutes"], "10")
        self.assertIn("cargo build --bin chaz --bin chaz-matrix --example e2ee_probe", steps["Build binaries"]["run"])
        self.assertIn("dev/matrix-e2e/run.sh --verbose", steps["Matrix E2E"]["run"])

    def test_native_main_queue_and_ci_budgets(self):
        self.assertEqual(expression(self.ci["concurrency"]["group"]), MAIN_QUEUE)
        self.assertEqual(self.ci["concurrency"]["cancel-in-progress"], "${{ github.event_name == 'pull_request' }}")
        self.assertNotIn("concurrency", self.publish)
        for job in self.ci["jobs"].values():
            self.assertNotIn("concurrency", job)
            self.assertEqual(job["runs-on"], "ubicloud-standard-8")
            self.assertEqual(job["timeout-minutes"], "30")

    def test_publisher_retains_manual_and_reusable_interfaces(self):
        self.assertEqual(set(self.publish["on"]), {"workflow_call", "workflow_dispatch"})
        job = self.publish["jobs"]["push-cache"]
        self.assertEqual(expression(job["if"]), TRUSTED_MAIN)
        self.assertEqual(job["environment"], "publish")
        self.assertEqual(job["permissions"], {"contents": "read"})
        self.assertEqual(job["timeout-minutes"], "60")
        names = {"NIX_CACHE_ACCESS_KEY_ID", "NIX_CACHE_SECRET_ACCESS_KEY", "NIX_CACHE_SIGNING_KEY"}
        self.assertEqual(self.publish["on"]["workflow_call"]["secrets"], {name: {"required": "true"} for name in names})

    def test_credentials_only_exist_on_protected_publication_steps(self):
        for workflow in (self.ci, self.publish, self.e2e):
            self.assertEqual(workflow["permissions"], {"contents": "read"})
        for job in (self.ci["jobs"]["ci"], self.e2e["jobs"]["e2e"]):
            self.assertNotIn("secrets.", yaml.safe_dump(job))
            self.assertNotIn("environment", job)
        for job in (self.ci["jobs"]["main"], self.publish["jobs"]["push-cache"]):
            self.assertEqual(expression(job["if"]), TRUSTED_MAIN)
            self.assertEqual(job["environment"], "publish")
            self.assertNotIn("env", job)
            for step in job["steps"]:
                if step["name"] in ("Publish dependencies", "Publish successful outputs"):
                    self.assertEqual(set(step["env"]), {"AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "NIX_CACHE_SIGNING_KEY"})
                else:
                    self.assertNotIn("secrets.", yaml.safe_dump(step))
                self.assertNotIn("actions/cache", step.get("uses", ""))
                self.assertNotIn("deps-cache.sh import", step.get("run", ""))

    def test_checked_revision_and_no_artifact_promotion(self):
        for job in (*self.ci["jobs"].values(), self.publish["jobs"]["push-cache"]):
            checkout = self.steps(job)["Checkout"]
            self.assertEqual(checkout["with"]["ref"], "${{ github.sha }}")
            self.assertEqual(checkout["with"]["persist-credentials"], "false")
            for step in job["steps"]:
                self.assertNotIn("${{", step.get("run", ""))
        for text in (self.ci_text, self.publish_text):
            self.assertNotIn("download-artifact", text)
            self.assertNotIn("workflow_run", text)
            self.assertNotIn("pull_request_target", text)
        setup = (ROOT / ".github/actions/setup-nix/action.yml").read_text()
        self.assertIn("extra-trusted-public-keys = cache.eidetica.dev-1:", setup)
        self.assertNotIn("signing-key", setup)
        self.assertNotIn("require-sigs", setup)


if __name__ == "__main__":
    unittest.main()
