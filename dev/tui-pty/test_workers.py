# /// script
# dependencies = ["pyte==0.8.2"]
# ///
"""Split daemon/TUI Worker discovery and non-executing inspection regression."""
import sys
sys.dont_write_bytecode = True
import contextlib
import json
import os
import shutil
from pathlib import Path
import re
import select
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from test_jobs import JobPtyProcess
from test_lifecycle import BINARY, FIXTURE, ROOT

CHAZ = str(BINARY)
SERVICE = os.environ.get("TUI_FIXTURE", str(FIXTURE))
OUT = Path(os.environ.get("TUI_EVIDENCE", ROOT / "target/tui-worker-evidence"))
OUT.mkdir(parents=True, exist_ok=True)
calls = []
entered = threading.Event()
release = threading.Event()
parent_entered = threading.Event()
parent_release = threading.Event()

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass
    def do_GET(self):
        self.reply({"data": [{"id": "stub", "context_length": 8192}]})
    def reply(self, value):
        payload = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)
    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        calls.append(request)
        messages = request["messages"]
        users = [m.get("content", "") for m in messages if m["role"] == "user"]
        if any(m.get("content") == "Deterministic worker." for m in messages):
            entered.set()
            if not release.wait(90):
                self.send_error(504, "bounded test release expired")
                return
            message = {"role": "assistant", "content": "CHILD-RESULT"}
        elif any(m["role"] == "tool" for m in messages):
            message = {"role": "assistant", "content": "PARENT-RESULT"}
        elif "PARENT-REQUEST" in users:
            parent_entered.set()
            if not parent_release.wait(45):
                self.send_error(504, "parent release expired")
                return
            message = {"role": "assistant", "content": None, "tool_calls": [{
                "id": f"native-spawn-{i}", "type": "function", "function": {
                    "name": "spawn_worker", "arguments": json.dumps({"name": "helper", "task": f"CHILD-ASSIGNMENT-{i}"})}}
                for i in [1, 2]]}
        else:
            raise AssertionError("unexpected model request: " + json.dumps(request))
        self.reply({"id": "stub-only", "model": "stub", "choices": [{"index": 0, "message": message, "finish_reason": "tool_calls" if message.get("tool_calls") else "stop"}], "usage": {"prompt_tokens": 20, "completion_tokens": 5, "total_tokens": 25}})

http = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
http.daemon_threads = True
thread = threading.Thread(target=http.serve_forever, daemon=True)
processes = []
tuis = []
home_owner = tempfile.TemporaryDirectory(prefix="worker-discovery-")
try:
    with contextlib.nullcontext(home_owner.name) as directory:
        home = Path(directory)
        (home / "bin").mkdir()
        env = {"HOME": directory, "PATH": str(home / "bin"), "TERM": "xterm-256color", "LANG": "C.UTF-8", "RUST_LOG": "info"}
        for name in ["CONFIG", "DATA", "STATE", "CACHE", "RUNTIME"]:
            path = home / name.lower()
            path.mkdir(mode=0o700)
            env[f"XDG_{name}_HOME" if name != "RUNTIME" else "XDG_RUNTIME_DIR"] = str(path)
        for cli in ["claude", "pi", "codex", "chaz", "gemini", "aichat"]:
            path = home / "bin" / cli
            path.write_text("#!/bin/sh\necho unexpected-model-cli >&2\nexit 97\n")
            path.chmod(0o700)
        if "LD_LIBRARY_PATH" in os.environ:
            env["LD_LIBRARY_PATH"] = os.environ["LD_LIBRARY_PATH"]
        socket = home / "service.sock"
        service = subprocess.Popen([SERVICE, home / "fixture.db", socket, "controlled"], env=env, cwd=home, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=(OUT / "service.log").open("wb"))
        processes.append(service)
        assert select.select([service.stdout], [], [], 20)[0], "service did not become ready"
        assert service.stdout.readline().strip() == b"fixture service ready"
        def worker_records(label):
            service.stdin.write(b"worker-records\n")
            service.stdin.flush()
            assert select.select([service.stdout], [], [], 20)[0], "metadata read deadline"
            line = service.stdout.readline().decode().strip()
            assert line.startswith("WORKER-RECORDS "), line
            records = json.loads(line.removeprefix("WORKER-RECORDS "))
            records.sort(key=lambda record: record["spawn"]["spawn_id"])
            (OUT / f"{label}-records.json").write_text(json.dumps(records, indent=2))
            return records
        shared = f'''unlock_password: pty-fixture-only
eidetica:
  connection: "unix://{socket}"
  login: {{ username: pty-test, password: test-password }}
backends:
  - name: stub
    type: openaicompatible
    api_base: "http://127.0.0.1:{http.server_port}/v1"
    api_key: synthetic
    models:
      - name: stub
agents:
  - name: chaz
    model: stub
    tools: [spawn_worker]
    system_prompt: Deterministic parent.
    workers:
      - name: helper
        system_prompt: Deterministic worker.
        tools: []
default_agents: [chaz]
security:
  auto_approved_tools: [spawn_worker]
context:
  reserved_output_tokens: 512
'''
        configs = {}
        for role in ["client", "executor", "executor-inspector", "rebind-inspector"]:
            config = home / f"{role}.yaml"
            execution = "client" if role == "client" else "executor"
            config.write_text(f'execution: {execution}\nstate_dir: "{home}/{role}-state"\n' + shared)
            configs[role] = config
        thread.start()
        log = (OUT / "executor.log").open("wb")
        executor = subprocess.Popen([CHAZ, "--config", configs["executor"], "daemon"], env=env, cwd=home, stdout=log, stderr=log)
        processes.append(executor)
        deadline = time.monotonic() + 30
        while "chaz daemon ready" not in (OUT / "executor.log").read_text():
            assert executor.poll() is None
            assert time.monotonic() < deadline, "executor startup"
            time.sleep(.05)
        def open_tui(label, role="client"):
            p = JobPtyProcess([CHAZ, "--config", configs[role]], env, home)
            tuis.append((label, p))
            p.resize(140, 38)
            p.expect("New session")
            return p
        p = open_tui("original")
        p.expect("No saved sessions yet")
        assert not calls
        p.send("n")
        p.expect(" > ")
        deadline = time.monotonic() + 20
        while not any("TUI session composer ready" in f.read_text() for f in (home / "client-state").glob("*.log")):
            p.drain(.05)
            assert time.monotonic() < deadline, "composer operation not settled"
        p.drain(.2)
        # Prime catalog while only the parent exists, then resume it.
        p.send("\x10")
        p.expect("[tui]")
        assert "[spawn]" not in "\n".join(p.screen.display)
        p.screen_capture(OUT, "before-spawn")
        p.send("\x10")
        p.expect("agent: chaz")
        # Keep another already-connected client in Chat while the daemon
        # creates the child, so its account snapshot lacks that child's hint.
        old = open_tui("old-inspector")
        old.expect("[tui]")
        old.send("\x1b[B\r")
        old.expect("agent: chaz")
        p.send("PARENT-REQUEST\r")
        assert parent_entered.wait(20), "parent model not entered"
        p.send("\x10")
        p.expect("[tui]")
        p.expect_absent("Loading sessions")
        assert "[spawn]" not in "\n".join(p.screen.display)
        parent_release.set()
        deadline = time.monotonic() + 35
        while not entered.wait(.05):
            p.drain(0)
            assert time.monotonic() < deadline, "worker model not entered"
        p.expect("[spawn]")
        p.expect("[tui]")
        p.screen_capture(OUT, "original-live-refreshed")
        live_records = worker_records("live")
        assert len(live_records) == 1
        assert live_records[0]["outcome"] is None
        assert live_records[0]["spawn"]["call"]["call_id"] == "native-spawn-1"
        assert live_records[0]["origin"]["parent_session_id"] == live_records[0]["parent"]
        # The hub was loaded before child creation and has not been reopened.
        clean_log = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", (OUT / "executor.log").read_text())
        child = re.search(r"child_session_db_id=(bafyr[a-z0-9]+)", clean_log).group(1)
        old.send(f"/join {child}\r")
        old.expect("Worker transcript")
        old.expect("CHILD-ASSIGNMENT")
        old.screen_capture(OUT, "old-client-live-child")
        old.expect("Parent (Alt+P")
        before = len(calls)
        old.send("must-not-submit\r/stop\r")
        old.drain(1)
        assert "must-not-submit" not in "\n".join(old.screen.display)
        assert len(calls) == before == 2, f"inspection executed a call: {len(calls)}"
        old.send("/parent\r")
        old.expect("1 linked Workers")
        old.expect("Worker helper")
        old.expect("Open child")
        old.expect("activity recorded")
        old.screen_capture(OUT, "live-parent-summary")
        def click_text(p, text):
            p.expect(text)
            for y, row in enumerate(p.screen.display):
                if text in row:
                    x = row.index(text) + 2
                    p.send(f"\x1b[<0;{x};{y+1}M\x1b[<0;{x};{y+1}m")
                    return
            raise AssertionError("click target not rendered: " + text)
        click_text(old, "Open child")
        old.expect("Worker transcript")
        old.expect("CHILD-ASSIGNMENT")
        assert len(calls) == before, "summary navigation executed a model call"
        release.set()
        old.expect("CHILD-RESULT")
        old.screen_capture(OUT, "old-client-completed-child")
        old.send("\x17")
        old.expect("PARENT-RESULT")
        deadline = time.monotonic() + 10
        while "\n".join(p.screen.display).count("[spawn]") != 2:
            p.drain(.05)
            assert time.monotonic() < deadline, "visible hub did not discover second child"
        p.screen_capture(OUT, "multiple-children")
        completed_records = worker_records("completed")
        assert len(completed_records) == 2
        assert {r["spawn"]["call"]["call_index"] for r in completed_records} == {0, 1}
        for record in completed_records:
            outcome, final_entry = record["outcome"]
            assert outcome["directive_id"] == record["spawn"]["directive_id"]
            assert outcome["terminal"] == "reply"
            assert final_entry["content"] == "CHILD-RESULT"
        old.expect("2 linked Workers")
        old.expect("Child result: CHILD-RESULT")
        old.screen_capture(OUT, "completed-parent-summaries")
        old.send("/workers\r")
        old.expect("Workers (parent-filtered)")
        old.expect("[spawn]")
        assert "[tui]" not in "\n".join(old.screen.display)
        old.screen_capture(OUT, "parent-filtered-workers")
        old.send("\x1b[B\r")
        old.expect("Worker transcript")
        old.expect("Parent (Alt+P")
        old.send("\x1bp")
        old.expect("PARENT-RESULT")
        old.send("unsent-parent-draft")
        old.expect("unsent-parent-draft")
        old.send("\x10")
        old.expect("[tui]")  # Wait for the unfiltered catalog, not cached /workers rows.
        old.expect("[spawn]")
        click_text(old, "[spawn]")
        click_text(old, "[spawn]")
        old.expect("Worker transcript")
        old.expect("CHILD-RESULT")
        old.screen_capture(OUT, "hub-reopened-child")
        old.resize(40, 16)
        old.expect("Worker transcript")
        old.expect("CHILD-RESULT")
        old.screen_capture(OUT, "narrow-child")
        old.send("\x1bp")  # Several children may be open; return by exact parent, not tab adjacency.
        old.expect("unsent-parent-draft")
        executor.terminate()
        executor.wait(timeout=5)
        restart_log = (OUT / "executor-restart.log").open("wb")
        restarted = subprocess.Popen([CHAZ, "--config", configs["executor"], "daemon"], env=env, cwd=home, stdout=restart_log, stderr=restart_log)
        processes.append(restarted)
        deadline = time.monotonic() + 30
        while "chaz daemon ready" not in (OUT / "executor-restart.log").read_text():
            assert restarted.poll() is None
            assert time.monotonic() < deadline
            time.sleep(.05)
        # Even an executor-capable TUI must not register a Worker using
        # ordinary Agent defaults instead of its original narrowed template.
        again = open_tui("fresh-completed", "executor-inspector")
        again.expect("[spawn]")
        again.expect("[tui]")
        again.screen_capture(OUT, "reopen-completed-visible")
        click_text(again, "[spawn]")
        click_text(again, "[spawn]")
        again.expect("Worker transcript")
        again.expect("CHILD-RESULT")
        again.screen_capture(OUT, "after-executor-restart-child")
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            again.drain(.05)
        assert len(calls) == 4
        assert worker_records("after-restart") == completed_records, "inspection/restart rewrote native references or results"
        def fixture_command(command, prefix):
            service.stdin.write((command + "\n").encode())
            service.stdin.flush()
            assert select.select([service.stdout], [], [], 20)[0], command + " deadline"
            line = service.stdout.readline().decode().strip()
            assert line.startswith(prefix + " "), line
            return json.loads(line.removeprefix(prefix + " "))
        # A closed ordinary parent is deliberately ineligible for automatic
        # resident adoption. Any watcher/claim in this fresh executor-capable
        # inspector would therefore have been caused by viewing or rebinding it.
        closed = fixture_command("close-worker-parent", "CLOSED-PARENTS")
        assert closed == [completed_records[0]["parent"]]
        rebind = open_tui("rebind", "rebind-inspector")
        click_text(rebind, "[spawn]")
        click_text(rebind, "[spawn]")
        rebind.expect("Parent (Alt+P")
        rebind.send("/parent\r")
        rebind.expect("Worker transcript")
        rebind.expect("PARENT-RESULT")
        rebind.screen_capture(OUT, "new-parent-inspector")
        rebind_tips = fixture_command("worker-cases-audit", "WORKER-AUDIT")
        service.stdin.write(b"stop\n")
        service.stdin.flush()
        assert select.select([service.stdout], [], [], 20)[0]
        assert service.stdout.readline().strip() == b"fixture service stopped"
        rebind.expect("Backend disconnected", timeout=30)
        service.stdin.write(b"start\n")
        service.stdin.flush()
        assert select.select([service.stdout], [], [], 20)[0]
        assert service.stdout.readline().strip() == b"fixture service ready"
        rebind.expect_absent("Backend disconnected", timeout=30)
        rebind.expect_absent("refresh failed", timeout=30)
        rebind.expect("Worker transcript")
        rebind.expect("PARENT-RESULT")
        rebind.screen_capture(OUT, "new-parent-inspector-after-service-rebind")
        rebind.drain(1)
        assert fixture_command("worker-cases-audit", "WORKER-AUDIT") == rebind_tips
        rebind_logs = "\n".join(f.read_text() for f in (home / "rebind-inspector-state").glob("*.log"))
        for line in rebind_logs.splitlines():
            assert not (closed[0] in line and ("Server watching session" in line or "Server claimed session runtime" in line)), "parent observer acquired execution on service rebind: " + line
        assert len(calls) == 4
        cases = fixture_command("worker-cases", "WORKER-CASES")
        (OUT / "synthetic-cases.json").write_text(json.dumps(cases, indent=2))
        # Use a fresh client after the deliberate service outage; other TUIs
        # can still be recovering their own bindings. Select the fixture parent
        # from its actual rendered hub rather than submitting during recovery.
        old = open_tui("synthetic-cases")
        old.resize(160, 75)
        old.expect("evidence-parent")
        click_text(old, "evidence-parent")
        click_text(old, "evidence-parent")
        old.expect("Worker transcript")
        old.expect("10 linked Workers")
        click_text(old, "R1 chaz")
        for text in ["Open child · failed", "SYNTHETIC-FAILURE", "completed attempt; result unavailable",
                     "Open child · outcome unknown", "stop request recorded; outcome not recorded",
                     "cancelled (acknowledged)", "LATER-CHILD-REPLY", "not started; no attempt recorded",
                     "completed without reply", "transcript unavailable", "Worker origin mismatch"]:
            old.expect(text)
        assert "DENIED-SECRET" not in "\n".join(old.screen.display)
        old.screen_capture(OUT, "synthetic-status-boundaries")
        before_tips = fixture_command("worker-cases-audit", "WORKER-AUDIT")
        # Opening a missing/denied reference must not acquire it or change tabs.
        rows = old.screen.display
        denied_row = next(y for y, row in enumerate(rows) if "CASE-denied" in row) + 1
        old.send(f"\x1b[<0;8;{denied_row+1}M\x1b[<0;8;{denied_row+1}m")
        old.expect("read failed; cached data retained")
        old.screen_capture(OUT, "denied-navigation-visible-refusal")
        assert "DENIED-SECRET" not in "\n".join(old.screen.display)
        old.send("/workers\r")
        old.expect("Workers (parent-filtered)")
        old.expect("referenced transcript unavailable locally")
        old.screen_capture(OUT, "unavailable-child-reference")
        old.send("\x1b")
        old.expect("Worker transcript")
        old.send("\x10")
        old.expect("independent-root")
        old.expect("legacy-worker")
        old.expect("legacy ancestry unknown")
        old.screen_capture(OUT, "roots-legacy-grouping")
        # Stop and restart only the disposable executor while interrupted child
        # evidence exists: inspection must not reconstruct narrowed Workers.
        restarted.terminate()
        restarted.wait(timeout=5)
        last_log = (OUT / "executor-cases-restart.log").open("wb")
        last = subprocess.Popen([CHAZ, "--config", configs["executor"], "daemon"], env=env, cwd=home, stdout=last_log, stderr=last_log)
        processes.append(last)
        deadline = time.monotonic() + 30
        while "chaz daemon ready" not in (OUT / "executor-cases-restart.log").read_text():
            assert last.poll() is None and time.monotonic() < deadline
            time.sleep(.05)
        old.send("\x10")
        old.expect("outcome unknown")
        old.expect("completed attempt; result unavailable")
        old.screen_capture(OUT, "interrupted-after-executor-restart")
        old.drain(1)
        after_tips = fixture_command("worker-cases-audit", "WORKER-AUDIT")
        (OUT / "synthetic-tips-before.json").write_text(json.dumps(before_tips, indent=2))
        (OUT / "synthetic-tips-after.json").write_text(json.dumps(after_tips, indent=2))
        # Ordinary roots can receive legitimate resident-adoption/schema writes
        # from the restarted executor. The non-executing synthetic parent and
        # its available children must remain byte-identical; do not mistake
        # unrelated runtime startup for an inspection effect.
        immutable = {cases["parent"], *cases["children"].values()} & before_tips.keys()
        changed = [sid for sid in immutable if after_tips.get(sid) != before_tips[sid]]
        assert not changed, "inspection/restart mutated synthetic Worker sessions: " + repr(changed)
        assert len(calls) == 4, "synthetic evidence triggered a model request"
        (OUT / "summary.json").write_text(json.dumps({"result": "passed", "cached_live": "parent and child", "model_calls": len(calls), "inspection_calls": 0, "old_client_join": "inspection-only child", "after_executor_restart": "completed child readable; no calls"}, indent=2))
        print("PASS: same-service split daemon/TUI discovers live child, stale-client /join and hub reopen are inspection-only, completion/restart readable, parent draft preserved; exact native links/outcomes and summary-child-parent navigation, grouped hub and /workers survive restart; new ordinary-parent inspector survives service rebind without watcher/runtime; failure/missing/interrupted/stop/cancelled/legacy/denied fixtures retained native tips; 4 stub calls, 0 inspection calls")
finally:
    parent_release.set()
    release.set()
    (OUT / "requests.json").write_text(json.dumps(calls, indent=2))
    for label, p in tuis:
        p.capture(OUT / label)
        p.close()
    for process in reversed(processes):
        if process.poll() is None:
            process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
    if thread.is_alive():
        http.shutdown()
    http.server_close()
    for role in ["client", "executor", "executor-inspector", "rebind-inspector"]:
        for log in (Path(home_owner.name) / f"{role}-state").glob("*.log*"):
            target = OUT / f"{role}-logs" / log.name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(log, target)
    home_owner.cleanup()
