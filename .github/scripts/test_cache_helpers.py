"""Exercise cache/publication helpers without keys, models or remote writers.

Run with the workflow tests through the Nix ci-policy check, which supplies jq
and Python/PyYAML. All Nix operations use executable stubs.
"""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
DEPS = ROOT / ".github/scripts/deps-cache.sh"
PUBLISH = ROOT / ".github/scripts/publish-cache.sh"
OUT = "/nix/store/" + "a" * 32 + "-deps"
DRV = "/nix/store/" + "b" * 32 + "-deps.drv"
OTHER = "/nix/store/" + "c" * 32 + "-other"


class CacheHelpers(unittest.TestCase):
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
            "if args[0] == 'eval':\n"
            "    if 'builtins.currentSystem' in args: print(os.environ.get('TEST_SYSTEM', 'x86_64-linux'))\n"
            f"    elif args[-1].endswith('.drvPath'): print(os.environ.get('TEST_DRV', {DRV!r}))\n"
            f"    else: print(os.environ.get('TEST_ROOT', {OUT!r}))\n"
            "elif args[0] == 'path-info':\n"
            "    if '--store' in args: sys.exit(int(os.environ.get('REMOTE_EXIT', '1')))\n"
            "    sys.exit(int(os.environ.get('LOCAL_EXIT', '0')))\n"
            "elif args[0] == 'copy':\n"
            "    if '--to' in args:\n"
            "        dest = args[args.index('--to')+1]\n"
            "        if dest.startswith('file://'):\n"
            "            os.makedirs(dest[7:], exist_ok=True)\n"
            "        elif not dest.startswith('s3://cache-eidetica-dev?'): sys.exit(99)\n"
            "    sys.exit(int(os.environ.get('COPY_EXIT', '0')))\n"
            "elif args[:2] == ['store', 'sign']:\n"
            "    key = args[args.index('--key-file')+1]\n"
            "    assert stat.S_IMODE(os.stat(key).st_mode) == 0o600\n"
            "    assert open(key).read() == 'FAKE-TEST-KEY\\n'\n"
            "    assert 'NIX_CACHE_SIGNING_KEY' not in os.environ\n"
            "    sys.exit(int(os.environ.get('SIGN_EXIT', '0')))\n"
            "elif args[0] == 'build':\n"
            "    if '--json' in args: print(json.dumps([{'outputs': {'out': os.environ['TEST_OUTPUT']}}]))\n"
            "    sys.exit(int(os.environ.get('BUILD_EXIT', '0')))\n"
            "else: sys.exit(99)\n"
        )
        stub.chmod(0o755)
        self.env = {
            **os.environ,
            "PATH": f"{self.temp_path}{os.pathsep}{os.environ['PATH']}",
            "RUNNER_TEMP": str(self.temp_path),
            "GITHUB_OUTPUT": str(self.temp_path / "outputs"),
            "GITHUB_EVENT_NAME": "pull_request",
            "GITHUB_REF": "refs/pull/3/merge",
            "GITHUB_REPOSITORY": "arcuru/chaz",
            "GITHUB_REPOSITORY_OWNER": "arcuru",
            "NIX_CALLS": str(self.calls_path),
            "TEST_OUTPUT": OUT,
        }
        # No accidental credential inheritance, even on a developer machine.
        for name in list(self.env):
            if name.startswith(("AWS_", "NIX_CACHE_")):
                del self.env[name]

    def run_helper(self, action, helper=DEPS, **env):
        return subprocess.run(
            [shutil.which("bash"), str(helper), action],
            cwd=ROOT, env={**self.env, **env}, capture_output=True, text=True, timeout=10,
        )

    def calls(self):
        if not self.calls_path.exists():
            return []
        return [json.loads(line) for line in self.calls_path.read_text().splitlines()]

    def clear_calls(self):
        self.calls_path.unlink(missing_ok=True)

    def resolve(self, **env):
        result = self.run_helper("resolve", **env)
        self.assertEqual(result.returncode, 0, result.stderr)
        return (self.temp_path / "outputs").read_text().splitlines()[-1]

    def payload(self):
        self.resolve()
        path = self.temp_path / "chaz-deps-cache"
        path.mkdir()
        shutil.copyfile(self.temp_path / "chaz-deps/identity.json", path / "identity.json")
        self.clear_calls()
        return path

    def test_identity_is_exact_and_deterministic(self):
        key = self.resolve()
        self.assertEqual(self.resolve(), key)
        self.assertRegex(key, r"^key=chaz-deps-v1-x86_64-linux-[0-9a-f]{64}$")
        self.assertNotEqual(self.resolve(TEST_ROOT=OTHER), key)
        self.assertNotEqual(self.resolve(TEST_DRV=OTHER + ".drv"), key)

    def test_identity_rejects_foreign_system_or_non_store_paths(self):
        for env in ({"TEST_SYSTEM": "aarch64-linux"}, {"TEST_ROOT": "/tmp/untrusted"}, {"TEST_DRV": "bad.drv"}):
            result = self.run_helper("resolve", **env)
            self.assertNotEqual(result.returncode, 0)

    def test_trusted_local_hit_does_not_contact_cache(self):
        self.resolve()
        self.clear_calls()
        result = self.run_helper("trusted")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls(), [["path-info", OUT]])
        self.assertIn("available=true", (self.temp_path / "outputs").read_text())

    def test_trusted_remote_hit_retains_signature_checks(self):
        self.resolve()
        self.clear_calls()
        result = self.run_helper("trusted", LOCAL_EXIT="1", REMOTE_EXIT="0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls()[-1], ["copy", "--from", "https://cache.eidetica.dev", OUT])
        self.assertNotIn("--no-check-sigs", str(self.calls()))

    def test_signed_cache_trust_failure_is_fatal(self):
        self.resolve()
        result = self.run_helper("trusted", LOCAL_EXIT="1", REMOTE_EXIT="0", COPY_EXIT="42")
        self.assertEqual(result.returncode, 42, result.stderr)
        self.assertNotIn("available=true", (self.temp_path / "outputs").read_text())

    def test_cache_miss_or_service_failure_does_not_skip_build(self):
        self.resolve()
        for status in ("1", "42"):
            self.clear_calls()
            result = self.run_helper("trusted", LOCAL_EXIT="1", REMOTE_EXIT=status)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("unavailable", result.stdout)
            self.assertIn("available=false", (self.temp_path / "outputs").read_text())
            result = self.run_helper("build")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(self.calls()[-2:], [
                ["build", "--fallback", "--no-link", "--max-jobs", "2", "--cores", "0", ".#test.deps"],
                ["path-info", OUT],
            ])

    def test_dependency_build_failure_is_fatal(self):
        self.resolve()
        self.clear_calls()
        result = self.run_helper("build", BUILD_EXIT="42")
        self.assertEqual(result.returncode, 42, result.stderr)
        self.assertEqual(len(self.calls()), 1)

    def test_export_is_only_selected_closure_and_identity(self):
        self.resolve()
        self.clear_calls()
        result = self.run_helper("export")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls(), [["copy", "--to", f"file://{self.temp_path}/chaz-deps-cache", OUT]])
        self.assertEqual(
            (self.temp_path / "chaz-deps/identity.json").read_bytes(),
            (self.temp_path / "chaz-deps-cache/identity.json").read_bytes(),
        )

    def test_unsigned_import_is_exact_root_pr_only(self):
        payload = self.payload()
        result = self.run_helper("import")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls(), [["copy", "--no-check-sigs", "--from", f"file://{payload}", OUT]])

    def test_malformed_missing_or_wrong_payload_is_not_imported(self):
        payload = self.payload()
        for content in ("{bad json", "{}", json.dumps({"root": OTHER}), ""):
            (payload / "identity.json").write_text(content)
            self.clear_calls()
            result = self.run_helper("import")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("identity mismatch", result.stdout)
            self.assertEqual(self.calls(), [])
        (payload / "identity.json").unlink()
        self.assertEqual(self.run_helper("import").returncode, 0)
        self.assertEqual(self.calls(), [])

    def test_corrupt_nar_falls_back_with_visible_warning(self):
        self.payload()
        result = self.run_helper("import", COPY_EXIT="42")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("import failed; rebuilding", result.stdout)
        result = self.run_helper("build")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_main_merge_queue_manual_and_foreign_ref_cannot_import_or_export(self):
        self.payload()
        for event, ref in (
            ("push", "refs/heads/main"), ("merge_group", "refs/heads/gh-readonly-queue/main/x"),
            ("workflow_dispatch", "refs/heads/topic"), ("pull_request_target", "refs/heads/main"),
            ("pull_request", "refs/heads/main"), ("pull_request", "refs/pull/3/head"),
        ):
            for action in ("import", "export"):
                self.clear_calls()
                result = self.run_helper(action, GITHUB_EVENT_NAME=event, GITHUB_REF=ref)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("refused", result.stderr)
                self.assertEqual(self.calls(), [])

    def publish_env(self):
        self.resolve()
        (self.temp_path / "chaz-checks.json").write_text(json.dumps([{"outputs": {"out": OUT}}]))
        (self.temp_path / "chaz-final.json").write_text(json.dumps([{"outputs": {"out": OTHER}}]))
        self.clear_calls()
        return {"GITHUB_EVENT_NAME": "push", "GITHUB_REF": "refs/heads/main", "NIX_CACHE_SIGNING_KEY": "FAKE-TEST-KEY"}

    def test_publication_signs_only_realized_outputs_and_cleans_key(self):
        env = self.publish_env()
        for action, paths in (("deps", [OUT]), ("final", [OUT, OTHER])):
            self.clear_calls()
            result = self.run_helper(action, helper=PUBLISH, **env)
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
        env = self.publish_env()
        for failure in ("LOCAL_EXIT", "SIGN_EXIT", "COPY_EXIT"):
            self.clear_calls()
            result = self.run_helper("final", helper=PUBLISH, **env, **{failure: "42"})
            self.assertEqual(result.returncode, 42, result.stderr)
            self.assertFalse(list(self.temp_path.glob("chaz-cache-key.*")))
            if failure != "COPY_EXIT":
                self.assertFalse(any(call[0] == "copy" for call in self.calls()))

    def test_publication_requires_exact_main_repository_event_and_ref(self):
        env = self.publish_env()
        for changes in (
            {"GITHUB_EVENT_NAME": "pull_request"}, {"GITHUB_EVENT_NAME": "merge_group"},
            {"GITHUB_EVENT_NAME": "workflow_call"}, {"GITHUB_EVENT_NAME": "pull_request_target"},
            {"GITHUB_REF": "refs/heads/topic"}, {"GITHUB_REPOSITORY": "arcuru/other"},
            {"GITHUB_REPOSITORY": "fork/chaz", "GITHUB_REPOSITORY_OWNER": "fork"},
        ):
            self.clear_calls()
            result = self.run_helper("final", helper=PUBLISH, **{**env, **changes})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("refused", result.stderr)
            self.assertEqual(self.calls(), [])
        result = self.run_helper("final", helper=PUBLISH, **{**env, "GITHUB_EVENT_NAME": "workflow_dispatch"})
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_actual_main_workflow_bash_failure_truth_table(self):
        import yaml

        workflow = yaml.load((ROOT / ".github/workflows/ci.yml").read_text(), Loader=yaml.BaseLoader)
        steps = [s for s in workflow["jobs"]["main"]["steps"] if "run" in s]
        for failure in (None, "Build dependencies", "Publish dependencies", "Checks", "Prepare final outputs", "Publish successful outputs"):
            with self.subTest(failure=failure):
                executed = []
                deps_failed = False
                for step in steps:
                    if step["name"] == "Require dependency publication" and not deps_failed:
                        continue
                    env = {**self.env, "GITHUB_EVENT_NAME": "push", "GITHUB_REF": "refs/heads/main"}
                    if "env" in step:
                        # These placeholders never come from the real environment.
                        env.update(NIX_CACHE_SIGNING_KEY="FAKE-TEST-KEY", AWS_ACCESS_KEY_ID="FAKE", AWS_SECRET_ACCESS_KEY="FAKE")
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
                        break  # normal success() step semantics
                self.assertEqual(result.returncode, 1 if failure == "Publish dependencies" else 42 if failure else 0, result.stderr)
                self.assertEqual(executed[-1], "Require dependency publication" if deps_failed else failure or "Publish successful outputs")
                if deps_failed:
                    self.assertIn("Checks", executed)
                    self.assertNotIn("Prepare final outputs", executed)
                if failure and failure != "Publish successful outputs":
                    self.assertNotIn("Publish successful outputs", executed)

    def test_publication_rejects_empty_malformed_and_foreign_paths(self):
        env = self.publish_env()
        for content in ("not json", "[]", '[{"outputs":{"out":"/tmp/wrong"}}]'):
            for file in ("chaz-checks.json", "chaz-final.json"):
                (self.temp_path / file).write_text(content)
            self.clear_calls()
            result = self.run_helper("final", helper=PUBLISH, **env)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(self.calls(), [])


if __name__ == "__main__":
    unittest.main()
