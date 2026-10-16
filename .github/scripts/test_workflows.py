#!/usr/bin/env python3
"""Offline checks of which CI and Publish work each GitHub event selects.

These evaluate the expressions in the actual workflow files under event
contexts, using a small interpreter for the GitHub expression subset the
workflows use, rather than comparing against copies of the expected strings.
"""

import pathlib
import re
import unittest

import yaml

ROOT = pathlib.Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github" / "workflows"


def load(name):
    data = yaml.safe_load((WORKFLOWS / name).read_text())
    # YAML 1.1 reads the bare key `on` as boolean true.
    data["on"] = data.pop(True)
    return data


# --- GitHub expression subset ------------------------------------------------

TOKEN = re.compile(
    r"\s*(?:(?P<str>'(?:[^']|'')*')|(?P<num>\d+)|(?P<op>==|!=|&&|\|\||[!().,])"
    r"|(?P<name>[A-Za-z_][A-Za-z0-9_-]*))"
)


def tokenize(text):
    pos, out = 0, []
    text = text.strip()
    while pos < len(text):
        match = TOKEN.match(text, pos)
        if not match:
            raise ValueError(f"cannot tokenize {text[pos:]!r}")
        pos = match.end()
        kind = match.lastgroup
        value = match.group(kind)
        if kind == "str":
            value = value[1:-1].replace("''", "'")
        elif kind == "num":
            value = int(value)
        out.append((kind, value))
    return out


def truthy(value):
    return value not in (None, False, 0, "")


def as_text(value):
    if value is None:
        return ""
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


def equal(left, right):
    if isinstance(left, str) and isinstance(right, str):
        return left.lower() == right.lower()
    return left == right


class Evaluator:
    """Recursive-descent evaluator: || and && return operands, as GitHub does."""

    def __init__(self, contexts, status="success"):
        self.contexts = contexts
        self.status = status

    def evaluate(self, text):
        self.tokens, self.pos = tokenize(text), 0
        value = self.or_expr()
        if self.pos != len(self.tokens):
            raise ValueError(f"trailing tokens in {text!r}")
        return value

    def peek(self):
        return self.tokens[self.pos] if self.pos < len(self.tokens) else (None, None)

    def take(self, op=None):
        token = self.peek()
        if op is not None and token != ("op", op):
            raise ValueError(f"expected {op!r}, got {token!r}")
        self.pos += 1
        return token

    def or_expr(self):
        value = self.and_expr()
        while self.peek() == ("op", "||"):
            self.take()
            right = self.and_expr()
            value = value if truthy(value) else right
        return value

    def and_expr(self):
        value = self.eq_expr()
        while self.peek() == ("op", "&&"):
            self.take()
            right = self.eq_expr()
            value = right if truthy(value) else value
        return value

    def eq_expr(self):
        value = self.unary()
        while self.peek() in (("op", "=="), ("op", "!=")):
            op = self.take()[1]
            right = self.unary()
            value = equal(value, right) if op == "==" else not equal(value, right)
        return value

    def unary(self):
        if self.peek() == ("op", "!"):
            self.take()
            return not truthy(self.unary())
        return self.primary()

    def primary(self):
        kind, value = self.take()
        if kind in ("str", "num"):
            return value
        if (kind, value) == ("op", "("):
            inner = self.or_expr()
            self.take(")")
            return inner
        if kind != "name":
            raise ValueError(f"unexpected token {value!r}")
        if value in ("true", "false", "null"):
            return {"true": True, "false": False, "null": None}[value]
        if self.peek() == ("op", "("):
            return self.call(value)
        result = self.contexts.get(value)
        while self.peek() == ("op", "."):
            self.take()
            key = self.take()[1]
            result = result.get(key) if isinstance(result, dict) else None
        return result

    def call(self, name):
        self.take("(")
        args = []
        while self.peek() != ("op", ")"):
            args.append(self.or_expr())
            if self.peek() == ("op", ","):
                self.take()
        self.take(")")
        if name == "format":
            return re.sub(r"\{(\d+)\}", lambda m: as_text(args[int(m.group(1)) + 1]), args[0])
        if name == "cancelled":
            return self.status == "cancelled"
        if name == "always":
            return True
        if name == "success":
            return self.status == "success"
        raise ValueError(f"unsupported function {name}")


def condition(text, contexts, status="success"):
    """Evaluate a job/step `if:` (bare or wrapped) the way the runner does."""
    if text is None:
        return status == "success"
    text = str(text).strip()
    if text.startswith("${{") and text.endswith("}}"):
        text = text[3:-2]
    # A condition without a status function implicitly requires success().
    if not re.search(r"\b(success|failure|cancelled|always)\(", text):
        text = f"success() && ({text})"
    return truthy(Evaluator(contexts, status).evaluate(text))


def interpolate(text, contexts):
    """Render a string containing `${{ }}` the way the runner does."""
    return re.sub(
        r"\$\{\{(.*?)\}\}",
        lambda m: as_text(Evaluator(contexts).evaluate(m.group(1))),
        str(text),
        flags=re.S,
    )


# --- Event contexts ----------------------------------------------------------

REPO = "arcuru/chaz"


def github(event_name, owner="arcuru", ref="refs/heads/main", run_id=101, event=None):
    return {
        "github": {
            "event_name": event_name,
            "repository_owner": owner,
            "repository": f"{owner}/chaz",
            "ref": ref,
            "sha": "a" * 40,
            "run_id": run_id,
            "event": event or {},
        }
    }


def pr(number=7, owner="arcuru"):
    ctx = github("pull_request", owner, ref=f"refs/pull/{number}/merge")
    ctx["github"]["event"] = {"pull_request": {"number": number}}
    return ctx


def ci_run(
    conclusion="success",
    branch="main",
    event="push",
    head_repo=REPO,
    run_id=900,
    head_sha="b" * 40,
):
    return {
        "workflow_run": {
            "id": run_id,
            "conclusion": conclusion,
            "head_branch": branch,
            "event": event,
            "head_sha": head_sha,
            "head_repository": {"full_name": head_repo},
        }
    }


def publish_run(run_id=500, owner="arcuru", **kw):
    return github("workflow_run", owner, run_id=run_id, event=ci_run(**kw))


# --- CI ---------------------------------------------------------------------

CORE_EVENTS = {
    "pull_request": pr(),
    "merge_group": github("merge_group", ref="refs/heads/gh-readonly-queue/main/pr-7-x"),
}
FULL_EVENTS = {
    "push to main": github("push"),
    "manual dispatch on main": github("workflow_dispatch"),
    "manual dispatch on a branch": github("workflow_dispatch", ref="refs/heads/topic"),
}


class CiCoverage(unittest.TestCase):
    def setUp(self):
        self.ci = load("ci.yml")
        self.jobs = self.ci["jobs"]

    def selected_steps(self, ctx):
        return [s["name"] for s in self.jobs["ci"]["steps"] if condition(s.get("if"), ctx)]

    def nix_targets(self, step_names):
        steps = {s["name"]: s for s in self.jobs["ci"]["steps"]}
        targets = []
        for name in step_names:
            targets += re.findall(r"\.#checks\.x86_64-linux\.([\w-]+)", steps[name].get("run", ""))
        return targets

    def test_triggers_unchanged(self):
        on = self.ci["on"]
        self.assertEqual(on["push"], {"branches": ["main"]})
        self.assertEqual(on["pull_request"], {"branches": ["main"]})
        self.assertIn("merge_group", on)
        self.assertIn("workflow_dispatch", on)
        self.assertNotIn("schedule", on)

    def test_status_names_and_timeouts_unchanged(self):
        self.assertEqual(self.jobs["ci"]["name"], "CI")
        self.assertEqual(self.jobs["ci"]["timeout-minutes"], 30)
        self.assertEqual(self.jobs["e2e"]["name"], "Matrix E2E")
        self.assertEqual(self.jobs["e2e"]["timeout-minutes"], 45)
        steps = {s["name"]: s for s in self.jobs["e2e"]["steps"]}
        self.assertEqual(steps["Build binaries"]["timeout-minutes"], 20)
        self.assertEqual(steps["Matrix E2E"]["timeout-minutes"], 10)

    def test_core_events_keep_lint_format_tests_and_build_only(self):
        for label, ctx in CORE_EVENTS.items():
            with self.subTest(label):
                self.assertTrue(condition(self.jobs["ci"].get("if"), ctx))
                targets = self.nix_targets(self.selected_steps(ctx))
                self.assertEqual(sorted(targets), ["build", "ci-policy", "lint", "test", "treefmt"])
                self.assertFalse(condition(self.jobs["e2e"].get("if"), ctx))

    def test_full_events_run_docs_and_matrix_e2e(self):
        for label, ctx in FULL_EVENTS.items():
            with self.subTest(label):
                targets = self.nix_targets(self.selected_steps(ctx))
                self.assertEqual(
                    sorted(targets), ["build", "ci-policy", "doc", "lint", "test", "treefmt"]
                )
                self.assertTrue(condition(self.jobs["e2e"].get("if"), ctx))

    def test_full_matrix_e2e_harness_is_still_the_whole_harness(self):
        runs = " ".join(s.get("run", "") for s in self.jobs["e2e"]["steps"])
        self.assertIn("nix develop .#e2e --command dev/matrix-e2e/run.sh --verbose", runs)
        self.assertNotRegex(runs, r"run\.sh[^\n]*(--group|--only|--filter|--smoke)")

    def test_e2e_stays_fenced_to_upstream(self):
        for ctx in (github("push", owner="someone"), github("workflow_dispatch", owner="someone")):
            self.assertFalse(condition(self.jobs["e2e"].get("if"), ctx))

    def test_test_check_is_cargo_test_with_doctests(self):
        # The `test` check is the flake's cargoTest derivation, which runs
        # doctests; dropping `doc` from PRs must not drop them.
        flake = (ROOT / "flake.nix").read_text()
        self.assertRegex(flake, r"test = chaz-test;")
        self.assertRegex(flake, r"chaz-test = craneLib\.cargoTest")

    def test_only_pull_requests_cancel_superseded_runs(self):
        group = self.ci["concurrency"]["group"]
        cancel = self.ci["concurrency"]["cancel-in-progress"]
        same_pr = interpolate(group, pr(7)), interpolate(group, pr(7))
        self.assertEqual(same_pr[0], same_pr[1])
        self.assertNotEqual(interpolate(group, pr(7)), interpolate(group, pr(8)))
        self.assertEqual(interpolate(cancel, pr()), "true")
        groups = set()
        for i, name in enumerate(("push", "workflow_dispatch", "merge_group")):
            ctx = github(name, run_id=1000 + i)
            self.assertEqual(interpolate(cancel, ctx), "false")
            groups.add(interpolate(group, ctx))
        groups.add(interpolate(group, github("push", run_id=2000)))
        self.assertEqual(len(groups), 4)


# --- Publish ----------------------------------------------------------------


class PublishCoalescing(unittest.TestCase):
    def setUp(self):
        self.publish = load("publish.yml")
        self.jobs = self.publish["jobs"]
        self.group = self.publish["concurrency"]["group"]

    def trusted(self):
        return {
            "push": publish_run(event="push"),
            "authorized manual CI on main": publish_run(event="workflow_dispatch"),
        }

    def untrusted(self):
        return {
            "failed CI": publish_run(conclusion="failure"),
            "cancelled CI": publish_run(conclusion="cancelled"),
            "PR CI": publish_run(event="pull_request"),
            "fork PR from a branch named main": publish_run(
                event="pull_request", head_repo="mallory/chaz"
            ),
            "fork push": publish_run(head_repo="mallory/chaz"),
            "non-main branch": publish_run(branch="topic"),
            "fork of the workflow": publish_run(owner="mallory"),
        }

    def test_trusted_dev_runs_share_one_group(self):
        groups = {
            interpolate(self.group, publish_run(run_id=500 + i, event=e))
            for i, e in enumerate(("push", "workflow_dispatch", "push"))
        }
        self.assertEqual(groups, {"publish-dev"})

    def test_untrusted_and_manual_runs_get_their_own_group(self):
        cases = dict(self.untrusted())
        cases["manual Publish"] = github("workflow_dispatch", run_id=777)
        for label, ctx in cases.items():
            with self.subTest(label):
                group = interpolate(self.group, ctx)
                self.assertNotEqual(group, "publish-dev")
                self.assertEqual(group, f"publish-{ctx['github']['run_id']}")

    def test_running_publishes_are_never_cancelled(self):
        self.assertFalse(self.publish["concurrency"]["cancel-in-progress"])
        for job in self.jobs.values():
            self.assertNotIn("concurrency", job)

    def test_guard_runs_exactly_for_runs_in_the_dev_group(self):
        cases = {
            **self.trusted(),
            **self.untrusted(),
            "manual Publish": github("workflow_dispatch"),
        }
        for label, ctx in cases.items():
            with self.subTest(label):
                in_group = interpolate(self.group, ctx) == "publish-dev"
                self.assertEqual(condition(self.jobs["guard"]["if"], ctx), in_group)

    def guard_needs(self, result="success", publish="true", sha="c" * 40):
        return {"guard": {"result": result, "outputs": {"publish": publish, "sha": sha}}}

    def push_cache(self, ctx, needs):
        job = self.jobs["push-cache"]
        status = "success" if needs["guard"]["result"] in ("success", "skipped") else "failure"
        return condition(job["if"], {**ctx, "needs": needs}, status)

    def test_dev_publish_requires_the_guard_verdict(self):
        ctx = publish_run()
        self.assertTrue(self.push_cache(ctx, self.guard_needs()))
        self.assertFalse(self.push_cache(ctx, self.guard_needs(publish="false")))
        self.assertFalse(self.push_cache(ctx, self.guard_needs(sha="")))
        self.assertFalse(self.push_cache(ctx, self.guard_needs(result="failure", publish="")))
        for label, ctx in self.untrusted().items():
            with self.subTest(label):
                skipped = self.guard_needs(result="skipped", publish="", sha="")
                self.assertFalse(self.push_cache(ctx, skipped))

    def test_dev_publish_checks_out_the_guard_selection(self):
        ref = self.jobs["push-cache"]["steps"][0]["with"]["ref"]
        ctx = {**publish_run(head_sha="b" * 40), "needs": self.guard_needs(sha="c" * 40)}
        self.assertEqual(interpolate(ref, ctx), "c" * 40)

    def test_manual_publish_is_independent_and_main_only(self):
        skipped = self.guard_needs(result="skipped", publish="", sha="")
        main = github("workflow_dispatch")
        self.assertFalse(condition(self.jobs["guard"]["if"], main))
        self.assertTrue(self.push_cache(main, skipped))
        ref = self.jobs["push-cache"]["steps"][0]["with"]["ref"]
        self.assertEqual(interpolate(ref, {**main, "needs": skipped}), main["github"]["sha"])
        topic = github("workflow_dispatch", ref="refs/heads/topic")
        self.assertFalse(self.push_cache(topic, skipped))

    def test_no_new_trust_or_secret_surface(self):
        self.assertEqual(self.publish["permissions"], {"contents": "read"})
        self.assertEqual(set(self.publish["on"]), {"workflow_run", "workflow_dispatch"})
        guard = self.jobs["guard"]
        self.assertEqual(guard["permissions"], {"actions": "read", "contents": "read"})
        self.assertNotIn("environment", guard)
        self.assertNotIn("secrets.", yaml.safe_dump(guard))
        cache = self.jobs["push-cache"]
        self.assertEqual(cache["permissions"], {"contents": "read"})
        self.assertEqual(cache["environment"], "publish")
        clean = next(s for s in cache["steps"] if s["name"] == "Build and push artifacts")
        self.assertIn("--option substituters 'https://cache.nixos.org'", clean["run"])
        for job in self.jobs.values():
            for step in job["steps"]:
                if step.get("uses", "").startswith("actions/checkout@"):
                    self.assertFalse(step["with"]["persist-credentials"])

    def test_guard_runs_the_tested_helper(self):
        runs = [s.get("run", "") for s in self.jobs["guard"]["steps"]]
        self.assertIn("python3 .github/scripts/publish_guard.py", runs)
        env = next(s for s in self.jobs["guard"]["steps"] if "run" in s)["env"]
        ctx = publish_run(run_id=500, head_sha="b" * 40)
        ctx["github"]["event"]["workflow_run"]["id"] = 900
        self.assertEqual(interpolate(env["TRIGGER_RUN_ID"], ctx), "900")
        self.assertEqual(interpolate(env["TRIGGER_HEAD_SHA"], ctx), "b" * 40)


if __name__ == "__main__":
    unittest.main()
