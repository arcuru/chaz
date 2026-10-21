"""Exercise publication with fake credentials and executable Nix stubs only."""

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
PUBLISH = ROOT / ".github/scripts/publish-cache.sh"
OUT = "/nix/store/" + "a" * 32 + "-deps"
OTHER = "/nix/store/" + "c" * 32 + "-other"


class Publication(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.temp_path = Path(self.temp.name)
        self.calls_path = self.temp_path / "calls.jsonl"
        stub = self.temp_path / "nix"
        stub.write_text(
            f"#!{sys.executable}\n"
            "import json, os, stat, sys\n"
            "args = sys.argv[1:]\n"
            "with open(os.environ['NIX_CALLS'], 'a') as f:\n"
            "    f.write(json.dumps(args) + '\\n')\n"
            "if args[0] == 'path-info':\n"
            "    sys.exit(int(os.environ.get('LOCAL_EXIT', '0')))\n"
            "elif args[0] == 'copy':\n"
            "    assert args[1] == '--to' and args[2].startswith('s3://cache-eidetica-dev?')\n"
            "    sys.exit(int(os.environ.get('COPY_EXIT', '0')))\n"
            "elif args[:2] == ['store', 'sign']:\n"
            "    key = args[args.index('--key-file')+1]\n"
            "    assert stat.S_IMODE(os.stat(key).st_mode) == 0o600\n"
            "    assert open(key).read() == 'FAKE-TEST-KEY\\n'\n"
            "    assert 'NIX_CACHE_SIGNING_KEY' not in os.environ\n"
            "    sys.exit(int(os.environ.get('SIGN_EXIT', '0')))\n"
            "elif args[0] == 'build':\n"
            f"    print(json.dumps([{{'outputs': {{'out': {OUT!r}}}}}]))\n"
            "    sys.exit(int(os.environ.get('BUILD_EXIT', '0')))\n"
            "else: sys.exit(99)\n"
        )
        stub.chmod(0o755)
        self.env = {
            **os.environ,
            "PATH": f"{self.temp_path}{os.pathsep}{os.environ['PATH']}",
            "RUNNER_TEMP": str(self.temp_path),
            "GITHUB_EVENT_NAME": "push",
            "GITHUB_REF": "refs/heads/main",
            "GITHUB_REPOSITORY": "arcuru/chaz",
            "GITHUB_REPOSITORY_OWNER": "arcuru",
            "NIX_CALLS": str(self.calls_path),
        }
        for name in list(self.env):
            if name.startswith(("AWS_", "NIX_CACHE_")):
                del self.env[name]
        self.env["NIX_CACHE_SIGNING_KEY"] = "FAKE-TEST-KEY"
        for name, path in (("deps", OUT), ("checks", OUT), ("final", OTHER)):
            (self.temp_path / f"chaz-{name}.json").write_text(json.dumps([{"outputs": {"out": path}}]))

    def run_helper(self, action, **env):
        return subprocess.run(
            [shutil.which("bash"), str(PUBLISH), action],
            cwd=ROOT, env={**self.env, **env}, capture_output=True, text=True, timeout=10,
        )

    def calls(self):
        if not self.calls_path.exists():
            return []
        return [json.loads(line) for line in self.calls_path.read_text().splitlines()]

    def clear_calls(self):
        self.calls_path.unlink(missing_ok=True)

    def test_publication_signs_only_realized_outputs_and_cleans_key(self):
        for action, paths in (("deps", [OUT]), ("final", [OUT, OTHER])):
            self.clear_calls()
            result = self.run_helper(action)
            self.assertEqual(result.returncode, 0, result.stderr)
            calls = self.calls()
            self.assertEqual(calls[0], ["path-info", *paths])
            self.assertEqual(calls[1][:4], ["store", "sign", "--recursive", "--key-file"])
            self.assertEqual(calls[1][5:], paths)
            self.assertEqual(calls[2][:2], ["copy", "--to"])
            self.assertIn("compression=zstd", calls[2][2])
            self.assertEqual(calls[2][3:], paths)
            self.assertFalse(list(self.temp_path.glob("chaz-cache-key.*")))
            self.assertFalse(any(call[0] in ("build", "eval") for call in calls))

    def test_publication_failures_stay_fatal_and_key_is_removed(self):
        for failure in ("LOCAL_EXIT", "SIGN_EXIT", "COPY_EXIT"):
            self.clear_calls()
            result = self.run_helper("final", **{failure: "42"})
            self.assertEqual(result.returncode, 42, result.stderr)
            self.assertFalse(list(self.temp_path.glob("chaz-cache-key.*")))
            if failure != "COPY_EXIT":
                self.assertFalse(any(call[0] == "copy" for call in self.calls()))

    def test_publication_requires_exact_main_repository_event_and_ref(self):
        for changes in (
            {"GITHUB_EVENT_NAME": "pull_request"}, {"GITHUB_EVENT_NAME": "merge_group"},
            {"GITHUB_EVENT_NAME": "workflow_call"}, {"GITHUB_EVENT_NAME": "pull_request_target"},
            {"GITHUB_REF": "refs/heads/topic"}, {"GITHUB_REPOSITORY": "arcuru/other"},
            {"GITHUB_REPOSITORY": "fork/chaz", "GITHUB_REPOSITORY_OWNER": "fork"},
        ):
            self.clear_calls()
            result = self.run_helper("final", **changes)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("refused", result.stderr)
            self.assertEqual(self.calls(), [])
        result = self.run_helper("final", GITHUB_EVENT_NAME="workflow_dispatch")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_actual_main_workflow_bash_failure_truth_table(self):
        workflow = yaml.load((ROOT / ".github/workflows/ci.yml").read_text(), Loader=yaml.BaseLoader)
        steps = [s for s in workflow["jobs"]["main"]["steps"] if "run" in s]
        for failure in (None, "Build dependencies", "Publish dependencies", "Checks", "Prepare final outputs", "Publish successful outputs"):
            with self.subTest(failure=failure):
                executed = []
                deps_failed = False
                for step in steps:
                    if step["name"] == "Require dependency publication" and not deps_failed:
                        continue
                    env = dict(self.env)
                    if "env" not in step:
                        del env["NIX_CACHE_SIGNING_KEY"]
                    if step["name"] == failure:
                        env["COPY_EXIT" if "Publish" in failure else "BUILD_EXIT"] = "42"
                    result = subprocess.run(
                        [shutil.which("bash"), "-euo", "pipefail", "-c", step["run"]],
                        cwd=ROOT, env=env, capture_output=True, text=True, timeout=10,
                    )
                    executed.append(step["name"])
                    if result.returncode:
                        if step.get("continue-on-error") == "true":
                            deps_failed = True
                            continue
                        break
                self.assertEqual(result.returncode, 1 if failure == "Publish dependencies" else 42 if failure else 0, result.stderr)
                self.assertEqual(executed[-1], "Require dependency publication" if deps_failed else failure or "Publish successful outputs")
                if deps_failed:
                    self.assertIn("Checks", executed)
                    self.assertNotIn("Prepare final outputs", executed)
                if failure and failure != "Publish successful outputs":
                    self.assertNotIn("Publish successful outputs", executed)

    def test_publication_rejects_empty_malformed_and_foreign_paths(self):
        for content in ("not json", "[]", '[{"outputs":{"out":"/tmp/wrong"}}]'):
            for name in ("deps", "checks", "final"):
                (self.temp_path / f"chaz-{name}.json").write_text(content)
            for action in ("deps", "final"):
                self.clear_calls()
                result = self.run_helper(action)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.calls(), [])


if __name__ == "__main__":
    unittest.main()
