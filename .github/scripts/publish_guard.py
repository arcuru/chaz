#!/usr/bin/env python3
"""Choose which CI-verified main commit a dev Publish run builds.

Dev Publish runs share one concurrency group: one runs, and a newer arrival
replaces the pending one. Arrival follows CI *completion* order, which can
invert commit order, so the run that survives may have been triggered by an
older commit than one whose Publish was just replaced. Building the trigger
as-is would then let an older commit overwrite newer dev output, and the newer
commit would never publish.

This guard keeps dev output monotonic. It confirms the triggering CI run
through the API, then walks main's first-parent history above that commit,
newest first, and selects the newest descendant that also has a successful
trusted CI run. Only when none exists does it select the triggering commit
itself, so a failed or still-running newer CI never blocks the newest green
commit. Every selected commit passed trusted main CI; the guard never picks
"whatever main is now".

Trusted CI means: the CI workflow file in this repository, triggered by a push
or a manual dispatch on main, from this repository, completed successfully.
Any API error or response it cannot vouch for fails the run closed instead of
publishing.

Writes `publish=true|false` and `sha=<commit>` to $GITHUB_OUTPUT. Standard
library only, so it runs on a bare runner without installing anything.
"""

import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request

CI_WORKFLOW_PATH = ".github/workflows/ci.yml"
TRUSTED_EVENTS = ("push", "workflow_dispatch")
MAIN = "main"
MAIN_REF = "refs/remotes/origin/main"
# Newer main commits to examine before giving up. The walk stops at the first
# newer green commit, so this is reached only when that many consecutive newer
# commits lack a green CI run; publishing nothing is the safe answer then.
MAX_NEWER = 30
SHA_RE = re.compile(r"^[0-9a-f]{40}$")


class GuardError(Exception):
    """The guard cannot vouch for a publish; fail closed."""


def _field(obj, *path):
    for key in path:
        if not isinstance(obj, dict) or key not in obj:
            raise GuardError(f"API response is missing {'.'.join(path)}")
        obj = obj[key]
    return obj


def is_trusted_ci_run(run, repository, sha):
    """Whether `run` is a successful trusted main CI run of `sha`."""
    return (
        _field(run, "path") == CI_WORKFLOW_PATH
        and _field(run, "head_sha") == sha
        and _field(run, "head_branch") == MAIN
        and _field(run, "event") in TRUSTED_EVENTS
        and _field(run, "head_repository", "full_name") == repository
        and _field(run, "status") == "completed"
        and _field(run, "conclusion") == "success"
    )


def has_trusted_ci_success(api_get, repository, sha):
    query = urllib.parse.urlencode(
        {
            "head_sha": sha,
            "branch": MAIN,
            "status": "success",
            "exclude_pull_requests": "true",
            "per_page": "100",
        }
    )
    page = api_get(
        f"/repos/{repository}/actions/workflows/{os.path.basename(CI_WORKFLOW_PATH)}/runs?{query}"
    )
    runs = _field(page, "workflow_runs")
    total = _field(page, "total_count")
    if not isinstance(runs, list) or not isinstance(total, int):
        raise GuardError(f"malformed run list for {sha}")
    if any(is_trusted_ci_run(run, repository, sha) for run in runs):
        return True
    if total > len(runs):
        raise GuardError(f"run list for {sha} was truncated")
    return False


def _git(repo, *args):
    return subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True, check=False)


def _is_ancestor(repo, older, newer):
    result = _git(repo, "merge-base", "--is-ancestor", older, newer)
    if result.returncode not in (0, 1):
        raise GuardError(f"git ancestry check failed: {result.stderr.strip()}")
    return result.returncode == 0


def select(api_get, repository, run_id, head_sha, repo=".", fetch=True):
    """Return (publish, sha) for the dev Publish triggered by CI `run_id`."""
    if not SHA_RE.match(head_sha or ""):
        raise GuardError(f"triggering head SHA is malformed: {head_sha!r}")
    if not str(run_id).isdigit():
        raise GuardError(f"triggering run id is malformed: {run_id!r}")

    trigger = api_get(f"/repos/{repository}/actions/runs/{run_id}")
    if _field(trigger, "id") != int(run_id):
        raise GuardError("API returned a different run than the trigger")
    if not is_trusted_ci_run(trigger, repository, head_sha):
        raise GuardError(f"run {run_id} is not a successful trusted main CI run")

    if fetch:
        # Refresh after the trigger lookup, so main includes every commit
        # whose CI could already have finished.
        result = _git(
            repo, "fetch", "--no-tags", "--quiet", "origin", f"+refs/heads/{MAIN}:{MAIN_REF}"
        )
        if result.returncode != 0:
            raise GuardError(f"git fetch failed: {result.stderr.strip()}")

    if not _is_ancestor(repo, head_sha, MAIN_REF):
        # Rewritten out of main; something else is the latest dev revision.
        return False, head_sha

    listed = _git(repo, "rev-list", "--first-parent", f"{head_sha}..{MAIN_REF}")
    if listed.returncode != 0:
        raise GuardError(f"git rev-list failed: {listed.stderr.strip()}")
    newer = [c for c in listed.stdout.split() if _is_ancestor(repo, head_sha, c)]
    for commit in newer[:MAX_NEWER]:
        if has_trusted_ci_success(api_get, repository, commit):
            return True, commit
    if len(newer) > MAX_NEWER:
        return False, head_sha
    return True, head_sha


def github_api_get(base_url, token):
    def get(path):
        request = urllib.request.Request(
            base_url.rstrip("/") + path,
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {token}",
                "X-GitHub-Api-Version": "2022-11-28",
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                return json.load(response)
        except (urllib.error.URLError, OSError, ValueError) as err:
            raise GuardError(f"GitHub API request failed: {err}") from err

    return get


def main(env=os.environ, api_get=None, repo="."):
    try:
        if api_get is None:
            api_get = github_api_get(
                env.get("GITHUB_API_URL", "https://api.github.com"), env["GITHUB_TOKEN"]
            )
        publish, sha = select(
            api_get,
            env["GITHUB_REPOSITORY"],
            env.get("TRIGGER_RUN_ID", ""),
            env.get("TRIGGER_HEAD_SHA", ""),
            repo=repo,
        )
    except (GuardError, KeyError) as err:
        print(f"::error::publish guard refused: {err}")
        return 1
    if not publish:
        print(f"::notice::{sha} is no longer on main or is too far behind; not publishing")
    elif sha != env.get("TRIGGER_HEAD_SHA"):
        print(f"::notice::newer CI-verified main commit {sha} supersedes the trigger")
    with open(env["GITHUB_OUTPUT"], "a", encoding="utf-8") as out:
        out.write(f"publish={'true' if publish else 'false'}\nsha={sha}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
