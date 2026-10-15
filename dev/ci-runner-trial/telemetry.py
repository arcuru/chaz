"""One-off Linux runner trial: validate inputs and log passive host counters."""

import hashlib
import json
import os
from pathlib import Path
import resource
import subprocess
import sys
import threading
import time

BASE = "654a25be1d95d471948e3cb4a85ebd3bc4faccb0"
PARENT = "ci/cache-cost-trials-20261015"
HEAD = "ci/runner-comparison-20261015"
REPO = "arcuru/chaz"


def check(event, env, checkout, parents):
    pr = event.get("pull_request", {})
    head, base = pr.get("head", {}), pr.get("base", {})
    if not (
        env.get("GITHUB_EVENT_NAME") == "pull_request"
        and env.get("GITHUB_REPOSITORY") == REPO
        and head.get("repo", {}).get("full_name") == REPO
        and head.get("ref") == HEAD
        and base.get("ref") == PARENT
        and base.get("sha") == BASE
        and checkout == env.get("GITHUB_SHA")
        and parents == [BASE, head.get("sha")]
    ):
        raise ValueError("runner trial requires the exact same-repository head/base and PR merge checkout")
    return {"checkout": checkout, "head": head["sha"], "base": BASE}


def read(path):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return None


def snapshot():
    # Raw cumulative counters permit deltas; no active network probe or Nix GC.
    disks = {}
    for path in (".", "/nix/store", os.environ["RUNNER_TEMP"]):
        try:
            fs = os.statvfs(path)
            disks[path] = {"available_bytes": fs.f_bavail * fs.f_frsize,
                           "free_inodes": fs.f_favail}
        except OSError:
            disks[path] = None
    return {"utc": time.time(), "monotonic": time.monotonic(), "disks": disks,
            "cpu": read("/proc/stat"), "memory": read("/proc/meminfo"),
            "load": read("/proc/loadavg"), "network": read("/proc/net/dev"),
            "io": read("/proc/diskstats"),
            "pressure": {kind: read(f"/proc/pressure/{kind}")
                         for kind in ("cpu", "memory", "io")}}


def emit(record):
    print("runner-trial " + json.dumps(record), flush=True)


def run(script):
    done = threading.Event()

    def sample():
        # A daemon thread cannot outlive this shell step. The workflow's
        # original step/job deadlines still bound the actual command.
        while not done.wait(15):
            emit(snapshot())

    emit(snapshot())
    sampler = threading.Thread(target=sample, daemon=True)
    sampler.start()
    started = time.monotonic()
    try:
        # Match the original unspecified Linux shell, including errexit.
        rc = subprocess.call(["bash", "-e", script])
    finally:
        done.set()
        sampler.join(timeout=1)
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    emit({"elapsed_seconds": time.monotonic() - started, "exit_code": rc,
          "child_user_seconds": usage.ru_utime, "child_system_seconds": usage.ru_stime,
          "max_child_rss_kib": usage.ru_maxrss})
    emit(snapshot())
    return rc if rc >= 0 else 128 - rc


def main():
    mode = sys.argv[1]
    if mode == "check":
        event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
        checkout = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
        # cat-file retains parent headers even in checkout's depth-one clone.
        commit = subprocess.check_output(["git", "cat-file", "-p", "HEAD"], text=True)
        parents = [line.split()[1] for line in commit.split("\n\n", 1)[0].splitlines()
                   if line.startswith("parent ")]
        record = check(event, os.environ, checkout, parents)
        record.update({key: os.environ.get(key) for key in
                       ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "RUNNER_NAME", "ImageOS", "ImageVersion")})
        record["cpus"] = os.cpu_count()
        record["flake_sha256"] = {name: hashlib.sha256(Path(name).read_bytes()).hexdigest()
                                  for name in ("flake.nix", "flake.lock")}
        emit(record)
    elif mode == "run":
        sys.exit(run(sys.argv[2]))
    else:
        raise ValueError(f"unknown mode: {mode}")


if __name__ == "__main__":
    main()
