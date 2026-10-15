"""Local fixtures; never invoke a model or GitHub API."""

import copy
import io
import json
from contextlib import redirect_stdout
import os
from pathlib import Path
import sys
import tempfile
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

    def test_missing_proc_is_explicit_not_zero(self):
        with patch.object(telemetry, "read", return_value=None), patch.dict(os.environ, RUNNER_TEMP="."):
            sample = telemetry.snapshot()
        self.assertIsNone(sample["network"])
        self.assertIsNone(sample["cpu"])
        self.assertIsNone(sample["pressure"]["io"])
        self.assertGreater(sample["disks"]["."]["available_bytes"], 0)

    def test_real_command_exit_and_live_counters(self):
        for command, expected in [("echo command-output; exit 7", 7),
                                  ("false; echo should-not-run", 1),
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
                self.assertEqual(timed["exit_code"], expected)
                self.assertGreater(timed["elapsed_seconds"], 0)
                self.assertGreaterEqual(timed["child_user_seconds"], 0)
                self.assertIn("network", records[0])
                self.assertIn("cpu", records[0])
                self.assertIn("MemAvailable", records[0]["memory"])
                self.assertIn("free_inodes", records[0]["disks"]["."])
                self.assertIn("network", records[-1])


if __name__ == "__main__":
    unittest.main()
