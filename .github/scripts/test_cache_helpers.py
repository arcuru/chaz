"""Exercise actual publication commands with fake keys and executable Nix stubs."""

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
OUT = "/nix/store/" + "a" * 32 + "-checked"
OTHER = "/nix/store/" + "c" * 32 + "-wrapper"


class Publication(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="cache test ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.calls_file = self.root / "calls.jsonl"
        stub = self.root / "nix"
        stub.write_text(
            f"#!{sys.executable}\n"
            "import json, os, stat, sys\n"
            "args = sys.argv[1:]\n"
            "with open(os.environ['NIX_CALLS'], 'a') as f:\n"
            "    f.write(json.dumps(args) + '\\n')\n"
            "if args[0] in ('build', 'log') or args[1].startswith('.#'):\n"
            "    assert 'NIX_CACHE_SIGNING_KEY' not in os.environ\n"
            "    assert not any(n.startswith('AWS_') for n in os.environ)\n"
            "if args[0] == 'build':\n"
            "    if '--print-out-paths' in args:\n"
            f"        print({OTHER!r})\n"
            "    sys.exit(int(os.environ.get('BUILD_EXIT', '0')))\n"
            "elif args[0] == 'log':\n"
            "    print('retained check output')\n"
            "elif args[0] == 'path-info':\n"
            f"    print({OUT!r})\n"
            "    sys.exit(int(os.environ.get('LOCAL_EXIT', '0')))\n"
            "elif args[:2] == ['store', 'sign']:\n"
            "    key = args[args.index('--key-file')+1]\n"
            "    assert stat.S_IMODE(os.stat(key).st_mode) == 0o600\n"
            "    assert open(key).read() == 'FAKE-TEST-KEY\\n'\n"
            "    assert 'NIX_CACHE_SIGNING_KEY' not in os.environ\n"
            "    sys.exit(int(os.environ.get('SIGN_EXIT', '0')))\n"
            "elif args[0] == 'copy':\n"
            "    assert args[1] == '--to' and args[2].startswith('s3://cache-eidetica-dev?')\n"
            "    sys.exit(int(os.environ.get('COPY_EXIT', '0')))\n"
            "else: sys.exit(99)\n"
        )
        stub.chmod(0o755)
        self.env = {
            k: v for k, v in os.environ.items()
            if not k.startswith(("AWS_", "NIX_CACHE_"))
        }
        self.env.update({
            "PATH": f"{self.root}{os.pathsep}{os.environ['PATH']}",
            "RUNNER_TEMP": str(self.root), "NIX_CALLS": str(self.calls_file),
            "GITHUB_EVENT_NAME": "push", "CI_EVENT_NAME": "push",
            "GITHUB_REF": "refs/heads/main", "GITHUB_REPOSITORY": "arcuru/chaz",
            "GITHUB_REPOSITORY_OWNER": "arcuru",
        })
        (self.root / "chaz-checks-paths").write_text(f"{OUT}\n{OUT}\n")
        (self.root / "chaz-final-paths").write_text(f"{OTHER}\n")

    def run_helper(self, **changes):
        return self.run_script(
            f"bash {PUBLISH}", NIX_CACHE_SIGNING_KEY="FAKE-TEST-KEY", **changes
        )

    def run_script(self, script, **changes):
        return subprocess.run(
            [shutil.which("bash"), "-euo", "pipefail", "-c", script],
            cwd=ROOT, env={**self.env, **changes}, capture_output=True,
            text=True, timeout=10,
        )

    def calls(self):
        return [json.loads(line) for line in self.calls_file.read_text().splitlines()] if self.calls_file.exists() else []

    def test_signs_only_realized_deduplicated_outputs_and_cleans_key(self):
        result = self.run_helper()
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertEqual(calls[0], ["path-info", OUT, OTHER])
        self.assertEqual(calls[1][:4], ["store", "sign", "--recursive", "--key-file"])
        self.assertEqual(calls[1][5:], [OUT, OTHER])
        self.assertEqual(calls[2][:2], ["copy", "--to"])
        self.assertIn("compression=zstd", calls[2][2])
        self.assertEqual(calls[2][3:], [OUT, OTHER])
        self.assertFalse(list(self.root.glob("chaz-cache-key.*")))
        self.assertNotIn("FAKE-TEST-KEY", result.stdout + result.stderr)
        self.assertFalse(any(c[0] in ("build", "eval") for c in calls))

    def test_publication_failures_stay_fatal_and_key_is_removed(self):
        for failure in ("LOCAL_EXIT", "SIGN_EXIT", "COPY_EXIT"):
            with self.subTest(failure=failure):
                self.calls_file.unlink(missing_ok=True)
                result = self.run_helper(**{failure: "42"})
                self.assertEqual(result.returncode, 42, result.stderr)
                self.assertFalse(list(self.root.glob("chaz-cache-key.*")))
                if failure != "COPY_EXIT":
                    self.assertFalse(any(c[0] == "copy" for c in self.calls()))

    def test_requires_exact_main_repository_event_and_ref(self):
        for changes in (
            {"GITHUB_EVENT_NAME": "pull_request"}, {"GITHUB_EVENT_NAME": "merge_group"},
            {"GITHUB_EVENT_NAME": "workflow_call"}, {"GITHUB_EVENT_NAME": "pull_request_target"},
            {"GITHUB_REF": "refs/heads/topic"}, {"GITHUB_REPOSITORY": "arcuru/other"},
            {"GITHUB_REPOSITORY_OWNER": "other"},
        ):
            with self.subTest(changes=changes):
                self.calls_file.unlink(missing_ok=True)
                result = self.run_helper(**changes)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("refused", result.stderr)
                self.assertEqual(self.calls(), [])
        self.assertEqual(self.run_helper(GITHUB_EVENT_NAME="workflow_dispatch").returncode, 0)

    def test_empty_missing_or_foreign_path_lists_fail_before_signing(self):
        for name in ("checks", "final"):
            path = self.root / f"chaz-{name}-paths"
            valid = path.read_text()
            for content in (None, "", "/tmp/foreign\n", f"{OUT}\ninvalid\n"):
                with self.subTest(name=name, content=content):
                    self.calls_file.unlink(missing_ok=True)
                    path.unlink(missing_ok=True)
                    if content is not None:
                        path.write_text(content)
                    result = self.run_helper()
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(self.calls(), [])
            path.write_text(valid)

    def test_actual_main_steps_stop_before_publication_on_failure(self):
        workflow = yaml.load((ROOT / ".github/workflows/ci.yml").read_text(), Loader=yaml.BaseLoader)
        steps = [s for s in workflow["jobs"]["main"]["steps"] if "run" in s]
        for failed in (None, "Checks", "Prepare final outputs", "Publish successful outputs"):
            with self.subTest(failed=failed):
                executed = []
                self.calls_file.unlink(missing_ok=True)
                for step in steps:
                    changes = {}
                    if step["name"] == "Publish successful outputs":
                        changes["NIX_CACHE_SIGNING_KEY"] = "FAKE-TEST-KEY"
                    if step["name"] == failed:
                        changes["COPY_EXIT" if failed == "Publish successful outputs" else "BUILD_EXIT"] = "42"
                    result = self.run_script(step["run"], **changes)
                    executed.append(step["name"])
                    if result.returncode:
                        break
                self.assertEqual(result.returncode, 42 if failed else 0, result.stderr)
                self.assertEqual(executed[-1], failed or "Publish successful outputs")
                if failed and failed != "Publish successful outputs":
                    self.assertFalse(any(c[0] == "copy" for c in self.calls()))


if __name__ == "__main__":
    unittest.main()
