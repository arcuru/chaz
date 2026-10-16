#!/usr/bin/env python3
"""Offline tests for publish_guard.py against a real git history and a fake API."""

import os
import pathlib
import subprocess
import sys
import tempfile
import unittest
import urllib.parse

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import publish_guard as guard  # noqa: E402

REPO = "arcuru/chaz"
GIT_ENV = {
    "GIT_AUTHOR_NAME": "t",
    "GIT_AUTHOR_EMAIL": "t@example.invalid",
    "GIT_COMMITTER_NAME": "t",
    "GIT_COMMITTER_EMAIL": "t@example.invalid",
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_NOSYSTEM": "1",
}


def git(cwd, *args):
    return subprocess.run(
        ["git", "-C", str(cwd), *args],
        check=True,
        capture_output=True,
        text=True,
        env={**os.environ, **GIT_ENV},
    ).stdout.strip()


def run(sha, run_id, **overrides):
    record = {
        "id": run_id,
        "path": ".github/workflows/ci.yml",
        "head_sha": sha,
        "head_branch": "main",
        "event": "push",
        "head_repository": {"full_name": REPO},
        "status": "completed",
        "conclusion": "success",
        "run_attempt": 1,
    }
    record.update(overrides)
    return record


class FakeApi:
    """Serves run lookups and per-SHA run lists, applying the server-side filters."""

    def __init__(self, runs, fail_on=None, truncate=False, attempts=()):
        self.runs = runs
        self.attempts = {(r["id"], r["run_attempt"]): r for r in attempts}
        self.fail_on = fail_on
        self.truncate = truncate
        self.calls = []

    def __call__(self, path):
        self.calls.append(path)
        if self.fail_on and self.fail_on in path:
            raise guard.GuardError("HTTP 502")
        prefix = f"/repos/{REPO}/actions/"
        assert path.startswith(prefix), path
        rest = path[len(prefix) :]
        if rest.startswith("runs/"):
            parts = rest.split("/")
            run_id = int(parts[1])
            if len(parts) == 4 and parts[2] == "attempts":
                return self.attempts[run_id, int(parts[3])]
            return next(r for r in self.runs if r["id"] == run_id)
        endpoint, query = rest.split("?")
        assert endpoint == "workflows/ci.yml/runs", endpoint
        params = dict(urllib.parse.parse_qsl(query))
        assert "status" not in params and params["branch"] == "main"
        matched = [
            r
            for r in self.runs
            if r["head_sha"] == params["head_sha"]
            and r["head_branch"] == "main"
            and r["event"] != "pull_request"
        ]
        total = len(matched) + (150 if self.truncate else 0)
        return {"total_count": total, "workflow_runs": matched}


class GuardTest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = pathlib.Path(tmp.name)
        self.origin = root / "origin"
        self.clone = root / "clone"
        git(root, "init", "-q", "-b", "main", str(self.origin))
        self.shas = [self.commit(f"c{i}") for i in range(4)]  # oldest first
        git(root, "clone", "-q", str(self.origin), str(self.clone))

    def commit(self, name):
        (self.origin / name).write_text(name)
        git(self.origin, "add", name)
        git(self.origin, "commit", "-q", "-m", name)
        return git(self.origin, "rev-parse", "HEAD")

    def select(self, api, run_id, sha):
        return guard.select(api, REPO, str(run_id), sha, repo=str(self.clone))

    def test_tip_publishes_itself(self):
        tip = self.shas[3]
        api = FakeApi([run(tip, 4)])
        self.assertEqual(self.select(api, 4, tip), (True, tip))

    def test_completion_inversion_publishes_the_newer_commit(self):
        # c3's CI finished first; its waiting Publish was then replaced by
        # c2's late arrival. The survivor must publish c3, not c2.
        c2, c3 = self.shas[2], self.shas[3]
        api = FakeApi([run(c2, 3), run(c3, 4)])
        self.assertEqual(self.select(api, 3, c2), (True, c3))

    def test_rerun_of_an_old_commit_cannot_regress_dev_output(self):
        c0, c3 = self.shas[0], self.shas[3]
        api = FakeApi([run(c0, 1, run_attempt=2), run(c3, 4)])
        self.assertEqual(self.select(api, 1, c0), (True, c3))

    def test_newer_success_survives_a_failed_or_running_rerun(self):
        c2, c3 = self.shas[2:]
        for status, conclusion in (("completed", "failure"), ("in_progress", None)):
            with self.subTest(status=status):
                api = FakeApi(
                    [
                        run(c2, 3),
                        run(c3, 4, status=status, conclusion=conclusion, run_attempt=3),
                    ],
                    attempts=[run(c3, 4), run(c3, 4, run_attempt=2, conclusion="failure")],
                )
                self.assertEqual(self.select(api, 3, c2), (True, c3))

    def test_reruns_without_any_success_fall_back_to_the_trigger(self):
        c2, c3 = self.shas[2:]
        api = FakeApi(
            [run(c2, 3), run(c3, 4, run_attempt=3, conclusion="failure")],
            attempts=[
                run(c3, 4, run_attempt=2, conclusion="cancelled"),
                run(c3, 4, conclusion="failure"),
            ],
        )
        self.assertEqual(self.select(api, 3, c2), (True, c2))

    def test_rerun_history_uncertainty_fails_closed(self):
        c2, c3 = self.shas[2:]
        runs = [run(c2, 3), run(c3, 4, run_attempt=2, conclusion="failure")]
        cases = {
            "missing attempt": FakeApi(runs, fail_on="attempts/1"),
            "malformed attempt": FakeApi(
                [run(c2, 3), run(c3, 4, run_attempt=None, conclusion="failure")]
            ),
        }
        for label, overrides in {
            "different run": {"id": 40},
            "different attempt": {"run_attempt": 2},
            "different commit": {"head_sha": c2},
            "fork": {"head_repository": {"full_name": "mallory/chaz"}},
            "PR": {"event": "pull_request"},
        }.items():
            api = FakeApi(runs)
            api.attempts[4, 1] = run(c3, 4, **overrides)
            cases[label] = api
        for label, api in cases.items():
            with self.subTest(label):
                with self.assertRaises(guard.GuardError):
                    self.select(api, 3, c2)

    def test_newest_green_wins_when_the_tip_failed(self):
        c1, c2, c3 = self.shas[1:]
        api = FakeApi([run(c1, 2), run(c2, 3), run(c3, 4, conclusion="failure")])
        self.assertEqual(self.select(api, 2, c1), (True, c2))

    def test_falls_back_to_the_trigger_when_newer_ci_failed_or_is_running(self):
        c1, c2, c3 = self.shas[1:]
        api = FakeApi(
            [
                run(c1, 2),
                run(c2, 3, conclusion="failure"),
                run(c3, 4, status="in_progress", conclusion=None),
            ]
        )
        self.assertEqual(self.select(api, 2, c1), (True, c1))

    def test_newer_runs_that_are_not_trusted_main_ci_do_not_count(self):
        c1, c3 = self.shas[1], self.shas[3]
        untrusted = [
            run(c3, 10, event="pull_request"),
            run(c3, 11, head_repository={"full_name": "mallory/chaz"}),
            run(c3, 12, path=".github/workflows/coverage.yml"),
            run(c3, 13, event="schedule"),
        ]
        api = FakeApi([run(c1, 2), *untrusted])
        self.assertEqual(self.select(api, 2, c1), (True, c1))

    def test_authorized_manual_ci_on_main_is_trusted(self):
        c2, c3 = self.shas[2], self.shas[3]
        api = FakeApi(
            [run(c2, 3, event="workflow_dispatch"), run(c3, 4, event="workflow_dispatch")]
        )
        self.assertEqual(self.select(api, 3, c2), (True, c3))

    def test_untrusted_triggers_fail_closed(self):
        tip = self.shas[3]
        cases = {
            "fork head": {"head_repository": {"full_name": "mallory/chaz"}},
            "PR event": {"event": "pull_request"},
            "other branch": {"head_branch": "topic"},
            "failed": {"conclusion": "failure"},
            "rerun in progress": {"status": "in_progress", "conclusion": None},
            "other workflow": {"path": ".github/workflows/publish.yml"},
            "different commit": {"head_sha": self.shas[2]},
            "missing field": {"head_repository": None},
        }
        for label, override in cases.items():
            with self.subTest(label):
                api = FakeApi([run(tip, 4, **override)])
                with self.assertRaises(guard.GuardError):
                    self.select(api, 4, tip)

    def test_api_failures_fail_closed(self):
        c2, c3 = self.shas[2], self.shas[3]
        runs = [run(c2, 3), run(c3, 4)]
        for label, api in {
            "trigger lookup": FakeApi(runs, fail_on="runs/3"),
            "newer run list": FakeApi(runs, fail_on=f"head_sha={c3}"),
            "truncated list": FakeApi([run(c2, 3)], truncate=True),
        }.items():
            with self.subTest(label):
                with self.assertRaises(guard.GuardError):
                    self.select(api, 3, c2)

    def test_malformed_inputs_fail_closed(self):
        api = FakeApi([run(self.shas[3], 4)])
        for run_id, sha in (
            ("4", "HEAD"),
            ("4", self.shas[3][:12]),
            ("4; x", self.shas[3]),
            ("", self.shas[3]),
        ):
            with self.subTest((run_id, sha)):
                with self.assertRaises(guard.GuardError):
                    guard.select(api, REPO, run_id, sha, repo=str(self.clone))
        self.assertEqual(api.calls, [])

    def test_sees_commits_pushed_after_checkout(self):
        newer = self.commit("c4")
        c3 = self.shas[3]
        api = FakeApi([run(c3, 4), run(newer, 5)])
        self.assertEqual(self.select(api, 4, c3), (True, newer))

    def test_commit_rewritten_out_of_main_is_not_published(self):
        c3 = self.shas[3]
        git(self.origin, "reset", "-q", "--hard", self.shas[1])
        self.commit("replacement")
        api = FakeApi([run(c3, 4)])
        self.assertEqual(self.select(api, 4, c3)[0], False)

    def test_too_many_unverified_newer_commits_publish_nothing(self):
        for i in range(guard.MAX_NEWER + 1):
            self.commit(f"n{i}")
        c3 = self.shas[3]
        api = FakeApi([run(c3, 4)])
        self.assertEqual(self.select(api, 4, c3)[0], False)

    def test_main_writes_outputs_and_reports_refusal(self):
        c2, c3 = self.shas[2], self.shas[3]
        out = pathlib.Path(self.clone) / "out"
        env = {
            "GITHUB_REPOSITORY": REPO,
            "GITHUB_OUTPUT": str(out),
            "TRIGGER_RUN_ID": "3",
            "TRIGGER_HEAD_SHA": c2,
        }
        rc = guard.main(env, FakeApi([run(c2, 3), run(c3, 4)]), repo=str(self.clone))
        self.assertEqual(rc, 0)
        self.assertEqual(out.read_text(), f"publish=true\nsha={c3}\n")
        out.unlink()
        rc = guard.main(env, FakeApi([run(c2, 3, conclusion="failure")]), repo=str(self.clone))
        self.assertEqual(rc, 1)
        self.assertFalse(out.exists())


if __name__ == "__main__":
    unittest.main()
