"""Local fixtures; never invoke a model or GitHub API."""

import copy
import io
import json
from contextlib import redirect_stdout
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
import telemetry


class TrialTests(unittest.TestCase):
    def setUp(self):
        self.event = {"pull_request": {
            "head": {"repo": {"full_name": telemetry.REPO}, "ref": telemetry.HEAD, "sha": "head"},
            "base": {"ref": telemetry.PARENT, "sha": telemetry.BASE}}}
        self.env = {"GITHUB_EVENT_NAME": "pull_request", "GITHUB_REPOSITORY": telemetry.REPO,
                    "GITHUB_SHA": "merge"}

    def check(self, **overrides):
        args = dict(event=self.event, env=self.env, checkout="merge", parents=[telemetry.BASE, "head"])
        args.update(overrides)
        return telemetry.check(**args)

    def test_exact_stack_and_synthetic_merge(self):
        self.assertEqual(self.check(), {"checkout": "merge", "head": "head", "base": telemetry.BASE})

    def test_non_pr_and_foreign_repository_refused(self):
        for key, value in [("GITHUB_EVENT_NAME", "push"), ("GITHUB_EVENT_NAME", "merge_group"),
                           ("GITHUB_EVENT_NAME", "workflow_dispatch"),
                           ("GITHUB_EVENT_NAME", "pull_request_target"),
                           ("GITHUB_REPOSITORY", "someone/chaz")]:
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                self.check(env=self.env | {key: value})

    def test_wrong_head_base_or_parent_advance_refused(self):
        for side, key, value in [("head", "ref", "other"), ("head", "sha", "advanced"),
                                 ("head", "repo", {"full_name": "fork/chaz"}),
                                 ("base", "ref", "main"), ("base", "sha", "advanced")]:
            event = copy.deepcopy(self.event)
            event["pull_request"][side][key] = value
            with self.subTest(side=side, key=key), self.assertRaises(ValueError):
                self.check(event=event)

    def test_head_checkout_and_wrong_merge_refused(self):
        for args in [{"checkout": "head"}, {"parents": []}, {"parents": ["head", telemetry.BASE]},
                     {"parents": [telemetry.BASE, "old-head"]}, {"event": {}}]:
            with self.subTest(args=args), self.assertRaises(ValueError):
                self.check(**args)

    def test_cli_checks_raw_parents_in_real_shallow_checkout(self):
        source = Path(telemetry.__file__).resolve().parents[2]
        objects = subprocess.check_output(["git", "rev-parse", "--git-path", "objects"],
                                          cwd=source, text=True).strip()
        head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def git(*args, **kwargs):
                return subprocess.check_output(["git", *args], cwd=root, text=True, **kwargs).strip()

            git("init", "-q")
            (root / ".git/objects/info/alternates").write_text(str((source / objects).resolve()) + "\n")
            identity = ["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid"]
            tree = git("rev-parse", f"{head}^{{tree}}")
            merge = git(*identity, "commit-tree", tree, "-p", telemetry.BASE, "-p", head, input="fixture\n")
            git("checkout", "-q", "--detach", merge)
            (root / ".git/shallow").write_text(merge + "\n")
            self.assertEqual(git("rev-parse", "--is-shallow-repository"), "true")
            self.assertEqual(git("rev-list", "--count", "HEAD"), "1")
            event = copy.deepcopy(self.event)
            event["pull_request"]["head"]["sha"] = head
            event_path = root / "event.json"
            event_path.write_text(json.dumps(event))
            env = os.environ | self.env | {"GITHUB_SHA": merge, "GITHUB_EVENT_PATH": str(event_path)}
            command = [sys.executable, telemetry.__file__, "check"]
            valid = subprocess.run(command, cwd=root, env=env, capture_output=True, text=True, timeout=10)
            self.assertEqual(valid.returncode, 0, valid.stderr)
            self.assertEqual(json.loads(valid.stdout.removeprefix("runner-trial "))["checkout"], merge)
            for changed in [{"GITHUB_SHA": head}, {"GITHUB_EVENT_NAME": "push"}]:
                invalid = subprocess.run(command, cwd=root, env=env | changed,
                                         capture_output=True, text=True, timeout=10)
                self.assertNotEqual(invalid.returncode, 0, "invalid checkout/event escaped CLI guard")

    def test_missing_proc_is_explicit_not_zero(self):
        with patch.object(telemetry, "read", return_value=None), patch.dict(os.environ, RUNNER_TEMP="."):
            sample = telemetry.snapshot()
        self.assertIsNone(sample["network"])
        self.assertIsNone(sample["cpu"])
        self.assertIsNone(sample["pressure"]["io"])
        self.assertGreater(sample["disks"]["."]["available_bytes"], 0)

    def test_real_command_exit_and_live_counters(self):
        for command, expected in [("echo command-output; exit 7", 7), ("exit 2", 2),
                                  ("kill -TERM $$", 143), ("false; echo should-not-run", 1),
                                  ("set +e; false; echo recovered; exit 0", 0)]:
            with self.subTest(command=command), tempfile.TemporaryDirectory() as directory:
                script = Path(directory) / "step.sh"
                script.write_text(command)
                output = io.StringIO()
                with patch.dict(os.environ, RUNNER_TEMP=directory), redirect_stdout(output):
                    rc = telemetry.run(str(script))
                self.assertEqual(rc, expected)
                records = [json.loads(line.removeprefix("runner-trial "))
                           for line in output.getvalue().splitlines()]
                timed = next(record for record in records if "exit_code" in record)
                self.assertEqual(timed["exit_code"] if timed["exit_code"] >= 0 else 128 - timed["exit_code"], expected)
                self.assertGreater(timed["elapsed_seconds"], 0)
                self.assertGreaterEqual(timed["child_user_seconds"], 0)
                self.assertIn("network", records[0])
                self.assertIn("cpu", records[0])
                self.assertIn("MemAvailable", records[0]["memory"])
                self.assertIn("free_inodes", records[0]["disks"]["."])
                self.assertIn("network", records[-1])

    def test_wrapper_cancellation_reaches_command_cleanup(self):
        for sig, expected in [(signal.SIGINT, 130), (signal.SIGTERM, 143)]:
            with self.subTest(signal=sig.name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                script = root / "step.sh"
                script.write_text('trap \'echo cleaned > "$RUNNER_TEMP/cleaned"\' EXIT\n'
                                  'trap \'exit 130\' INT\ntrap \'exit 143\' TERM\n'
                                  'echo $$ > "$RUNNER_TEMP/ready"\nwhile :; do sleep 0.1; done\n')
                with (root / "output").open("w") as output:
                    proc = subprocess.Popen([sys.executable, telemetry.__file__, "run", str(script)],
                                            env=os.environ | {"RUNNER_TEMP": directory},
                                            stdout=output, stderr=output, start_new_session=True)
                    child_group = proc.pid
                    try:
                        deadline = time.monotonic() + 5
                        while time.monotonic() < deadline:
                            ready = telemetry.read(root / "ready")
                            if ready:
                                break
                            time.sleep(0.01)
                        self.assertTrue(ready, "step did not start")
                        child_group = os.getpgid(int(ready))
                        proc.send_signal(sig)
                        self.assertEqual(proc.wait(timeout=5), expected)
                        self.assertTrue((root / "cleaned").exists(), "command cleanup did not run")
                    finally:
                        for group in {proc.pid, child_group}:
                            try:
                                os.killpg(group, signal.SIGKILL)
                            except ProcessLookupError:
                                pass
                        proc.wait(timeout=3)

    def test_periodic_sampling_stops_without_touching_command_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            script = root / "step.sh"
            script.write_text('trap \'rm -f "$RUNNER_TEMP/owned"\' EXIT\n'
                              'printf fixture > "$RUNNER_TEMP/owned"\nsleep 16\n'
                              'test "$(cat "$RUNNER_TEMP/owned")" = fixture\n')
            threads = set(threading.enumerate())
            output = io.StringIO()
            with patch.dict(os.environ, RUNNER_TEMP=directory), redirect_stdout(output):
                self.assertEqual(telemetry.run(str(script)), 0)
            records = [json.loads(line.removeprefix("runner-trial "))
                       for line in output.getvalue().splitlines()]
            snapshots = [record for record in records if "monotonic" in record]
            self.assertGreaterEqual(len(snapshots), 3, "periodic sample missing")
            self.assertGreaterEqual(snapshots[1]["monotonic"] - snapshots[0]["monotonic"], 14)
            self.assertEqual(set(threading.enumerate()), threads, "sampler survived command")
            self.assertFalse((root / "owned").exists(), "command's cleanup was changed")


if __name__ == "__main__":
    unittest.main()
