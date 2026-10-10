#!/usr/bin/env python3
# /// script
# requires-python = ">=3.12"
# dependencies = ["pyte==0.8.2"]
# ///
"""Real client-role TUI + resident executor + disposable Eidetica service."""
import json
import html
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pyte

sys.dont_write_bytecode = True
from test_lifecycle import BINARY, FIXTURE, ROOT, PtyProcess


class JobPtyProcess(PtyProcess):
    def __init__(self, *args):
        self.screen = pyte.Screen(40, 16)
        self.stream = pyte.ByteStream(self.screen)
        super().__init__(*args)

    def resize(self, columns, rows):
        super().resize(columns, rows)
        self.screen.resize(rows, columns)

    def drain(self, timeout):
        before = len(self.raw)
        super().drain(timeout)
        self.stream.feed(bytes(self.raw[before:]))

    def expect(self, text, timeout=20, offset=0):
        # Ratatui sends cursor deltas, not full frames. Assert the actual
        # terminal screen rather than concatenate fragments into fake text.
        needle = "".join(text.split())
        deadline = time.monotonic() + timeout
        while True:
            self.drain(0.05)
            rendered = "".join("".join(self.screen.display).split())
            if needle in rendered:
                self.checkpoints.append({"seen": text, "time": time.monotonic()})
                return
            if self.child.poll() is not None:
                raise AssertionError(f"child exited {self.child.returncode} before {text!r}")
            if time.monotonic() >= deadline:
                raise AssertionError(f"deadline waiting for {text!r}; screen: {self.screen.display}")

    def screen_capture(self, evidence, label):
        self.drain(0.1)
        dest = evidence / label
        self.capture(dest)
        (dest / "screen.txt").write_text("\n".join(self.screen.display) + "\n")
        # Cell positions/styles come from the live terminal emulator; no
        # reconstructed fixture screenshot or concatenated cursor deltas.
        cols, rows = self.screen.columns, self.screen.lines
        svg = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{cols * 9}" height="{rows * 18}" viewBox="0 0 {cols * 9} {rows * 18}">',
               '<rect width="100%" height="100%" fill="#101218"/>',
               '<g font-family="monospace" font-size="14">']
        palette = {"default": "#ededed", "black": "#101218", "white": "#ededed"}
        def color(value):
            return palette.get(value, "#" + value if len(value) == 6 else "#ededed")
        for y in range(rows):
            for x in range(cols):
                cell = self.screen.buffer[y][x]
                if cell.bg != "default":
                    svg.append(f'<rect x="{x * 9}" y="{y * 18}" width="9" height="18" fill="{color(cell.bg)}"/>')
                if cell.data.strip():
                    svg.append(f'<text x="{x * 9}" y="{y * 18 + 14}" fill="{color(cell.fg)}" font-weight="{"bold" if cell.bold else "normal"}">{html.escape(cell.data)}</text>')
        svg.append('</g></svg>')
        (dest / "screen.svg").write_text("\n".join(svg))

    def readability_sizes(self, evidence, phase, status, result=None):
        for cols, rows in [(80, 24), (120, 35), (180, 45)]:
            self.resize(cols, rows)
            self.expect("original job task")
            self.expect(status)
            self.expect("1 jobs")
            if result:
                self.expect(result)
            screen = "\n".join(self.screen.display)
            for hidden in ["Some(", "StartedUnknown", "Succeeded {", "attempt_id", "bafyr4i", "ed25519:"]:
                if hidden in screen:
                    raise AssertionError(f"default leaked {hidden}: {screen}")
            self.screen_capture(evidence, f"{phase}-{cols}x{rows}")
        self.resize(110, 35)
        self.expect(status)

    def expect_absent(self, text, timeout=20):
        deadline = time.monotonic() + timeout
        while True:
            self.drain(0.05)
            if text not in "\n".join(self.screen.display):
                self.checkpoints.append({"absent": text, "time": time.monotonic()})
                return
            if time.monotonic() >= deadline:
                raise AssertionError(f"deadline waiting for {text!r} to disappear")


class Jobs(unittest.TestCase):
    def test_service_job_observer_and_steering(self):
        calls = []
        entered = threading.Event()
        release = threading.Event()
        continued = threading.Event()
        second_release = threading.Event()
        job_calls = []

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_GET(self):
                payload = b'{"data": [{"id": "stub", "context_length": 4096}]}'
                self.send_response(200)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                messages = request["messages"]
                calls.append(request)
                users = [m.get("content", "") for m in messages if m.get("role") == "user"]
                tools = [m for m in messages if m.get("role") == "tool"]
                is_job = "original job task" in users
                if is_job:
                    job_calls.append(request)
                    if len(job_calls) == 1:
                        entered.set()
                        if not release.wait(90):
                            self.send_error(504, "test gate timed out")
                            return
                        message = {"role": "assistant", "content": None, "tool_calls": [{
                            "id": "calc", "type": "function", "function": {
                                "name": "calculate", "arguments": '{"expression":"2+2"}'
                            }
                        }]}
                    else:
                        continued.set()
                        if not second_release.wait(45):
                            self.send_error(504, "continuation test gate timed out")
                            return
                        message = {"role": "assistant", "content": "continued job terminal"}
                elif not tools:
                    message = {"role": "assistant", "content": None, "tool_calls": [{
                        "id": "spawn", "type": "function", "function": {
                            "name": "spawn_agent", "arguments": json.dumps({"agent_ref": "chaz", "task": "original job task"})
                        }
                    }]}
                else:
                    message = {"role": "assistant", "content": "parent submitted job"}
                payload = json.dumps({"id": "local-test", "model": "stub", "choices": [{
                    "index": 0, "message": message, "finish_reason": "tool_calls" if message.get("tool_calls") else "stop"
                }], "usage": {"prompt_tokens": 20, "completion_tokens": 5, "total_tokens": 25}}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        http = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        http.daemon_threads = True
        thread = threading.Thread(target=http.serve_forever, daemon=True)
        process = None
        service = None
        executor = None
        evidence = ROOT / "target/job-monitor-pty" / str(time.time_ns())
        evidence.mkdir(parents=True)
        with tempfile.TemporaryDirectory(prefix="chaz-jobs-pty-") as directory:
            home = Path(directory)
            (home / "bin").mkdir()
            env = {"PATH": str(home / "bin"), "HOME": directory, "TERM": "xterm-256color", "LANG": "C.UTF-8", "RUST_LOG": "info"}
            for name in ["CONFIG", "DATA", "STATE", "CACHE", "RUNTIME"]:
                path = home / name.lower()
                path.mkdir(mode=0o700)
                env[f"XDG_{name}_HOME" if name != "RUNTIME" else "XDG_RUNTIME_DIR"] = str(path)
            for cli in ["claude", "pi", "codex", "chaz", "gemini"]:
                path = home / "bin" / cli
                path.write_text("#!/bin/sh\necho 'unexpected model CLI' >&2\nexit 97\n")
                path.chmod(0o700)
            if "LD_LIBRARY_PATH" in os.environ:
                env["LD_LIBRARY_PATH"] = os.environ["LD_LIBRARY_PATH"]
            socket = home / "service.sock"
            shared = f'''unlock_password: pty-fixture-only
eidetica:
  connection: "unix://{socket}"
  login:
    username: pty-test
    passwordless: true
backends:
  - name: stub
    type: openaicompatible
    api_base: "http://127.0.0.1:{http.server_port}/v1"
    api_key: not-a-real-key
    models:
      - name: stub
agents:
  - name: chaz
    model: stub
    tools: [spawn_agent, calculate]
    system_prompt: You are a deterministic test fixture.
default_agents: [chaz]
security:
  auto_approved_tools: [spawn_agent]
'''
            configs = {}
            for role in ["client", "executor"]:
                config = home / f"{role}.yaml"
                config.write_text(f'execution: {role}\nstate_dir: "{home}/{role}-state"\n' + shared)
                configs[role] = config
            log = (evidence / "executor.log").open("wb")
            try:
                service = subprocess.Popen([FIXTURE, home / "fixture.db", socket], env=env, cwd=home, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                self.assertTrue(select.select([service.stdout], [], [], 20)[0], "fixture service did not become ready")
                self.assertEqual(service.stdout.readline().strip(), b"fixture service ready")
                thread.start()
                executor = subprocess.Popen([BINARY, "--config", configs["executor"], "daemon"], env=env, cwd=home, stdout=log, stderr=log)
                deadline = time.monotonic() + 20
                while "chaz daemon ready" not in (evidence / "executor.log").read_text():
                    self.assertIsNone(executor.poll(), "executor exited before readiness")
                    self.assertLess(time.monotonic(), deadline, "executor bootstrap did not finish")
                    time.sleep(0.05)
                process = JobPtyProcess([BINARY, "--config", configs["client"]], env, home)
                process.resize(110, 35)
                process.expect("New session")
                process.send("\x07")  # Jobs also works with no conversation open.
                process.expect("Tasks")
                time.sleep(5.2)  # Exercise the activity tick with no active tab.
                process.drain(0)
                self.assertIsNone(process.child.poll(), "sessionless Jobs crashed")
                self.assertEqual(calls, [], "opening Jobs must not call the model")
                process.send("\x1b")
                process.expect("New session")
                process.send("\x07\x10")  # Ctrl+P from Jobs opens the hub.
                process.expect("New session")
                process.send("n")
                process.expect(" > ")
                process.send("parent-monitor\r")
                deadline = time.monotonic() + 60
                while not entered.wait(0.05):
                    process.drain(0)
                    self.assertLess(time.monotonic(), deadline, "resident job model call did not start")
                self.assertTrue(any(m.get("role") == "tool" and "session_db_id" in m.get("content", "")
                    for request in calls for m in request["messages"]), "parent must have published the real child handle")
                process.send("unsent-parent-draft")
                process.expect("unsent-parent-draft")
                process.send("\x07")  # Ctrl+G: observe Jobs, not a new turn.
                process.expect("Tasks")
                process.expect("Context · not a job")  # Wait for the source-backed rows, not the loading frame.
                process.send("\x1b[B")  # Context root first, claimed child second.
                process.expect("Started · recent activity")
                process.expect("original job task")
                process.readability_sizes(evidence, "started", "Started · recent activity")
                process.resize(80, 24)
                process.expect("Started · recent activity")
                process.send("d")
                process.expect("Session:")
                process.expect("bafyr4i")
                process.expect("Attempt:")
                process.screen_capture(evidence, "started-details")
                process.resize(40, 16)  # Force overflow of the real diagnostic fields.
                process.send("\x1b[F")  # End reads to the diagnostic tail, not the tree.
                process.expect("Source:")
                process.expect_absent("Session:")
                process.screen_capture(evidence, "started-details-scrolled-40x16")
                process.send("\x1b[H")
                process.expect("Session:")
                process.resize(80, 24)
                process.send("d")
                process.expect("Task · Started · recent activity")
                process.expect_absent("Session:")
                process.send("\r")
                process.expect("Job observer")
                process.expect("next-call input")
                process.resize(80, 24)
                process.expect("Started · recent activity")
                process.screen_capture(evidence, "observer-started-80x24")
                process.send("\x1bOQ")  # F2: observer diagnostics, not a typed job message.
                process.expect("Observer details")
                process.expect("Session:")
                process.expect("bafyr4i")
                process.send("\x1bOQ")
                process.expect_absent("Observer details")
                self.assertNotIn("unsent-parent-draft", "\n".join(process.screen.display),
                                 "opening an observer inherited the parent's draft")
                process.send("\x08")  # Ctrl+H restores the ordinary tab's draft.
                process.expect("unsent-parent-draft")
                process.send("\x0c")  # Ctrl+L returns to the observer's empty draft.
                process.expect("Job observer")
                self.assertNotIn("unsent-parent-draft", "\n".join(process.screen.display))
                with self.assertRaisesRegex(AssertionError, "deadline"):
                    process.expect("deliberate-absent-job-marker", timeout=0.2)
                self.assertEqual(len(job_calls), 1)
                process.send("PTY steering input\r")
                process.expect("Queued · published, awaiting acceptance")
                process.screen_capture(evidence, "observer-queued-80x24")
                self.assertEqual(len(job_calls), 1, "viewer/steering must not run or interrupt a job")
                release.set()
                self.assertTrue(continued.wait(20), "steering must reach the actual next request")
                process.expect("calculate")  # Persisted live job tool activity, not a fixture row.
                process.expect("Accepted · dispatch recorded")
                process.expect("inclusion unconfirmed")
                process.screen_capture(evidence, "observer-dispatch-80x24")
                # Refresh/navigation/close while the continued resident call is blocked.
                process.send("\x07r")  # overview and refresh
                process.expect("Tasks")
                process.send("\x1b")
                process.expect("Job observer")
                process.send("\x17")  # Close only the observer view.
                process.expect("unsent-parent-draft")
                process.send("\x17")  # Last view closes to the upstream hub, not a new session.
                process.expect("New session")
                self.assertEqual(len(job_calls), 2)
                process.send("\x03")
                self.assertEqual(process.child.wait(timeout=10), 0)
                process.capture(evidence / "before-disconnect")
                process.close()
                process = None
                self.assertIsNone(executor.poll(), "client disconnect stopped executor")
                second_release.set()
                self.assertTrue(continued.is_set(), "accepted steering did not reach a continuation")
                users = [m.get("content") for m in job_calls[1]["messages"] if m.get("role") == "user"]
                self.assertEqual(users.count("PTY steering input"), 1)
                messages = job_calls[1]["messages"]
                input_index = next(i for i, message in enumerate(messages) if message.get("content") == "PTY steering input")
                self.assertEqual(messages[input_index - 1].get("role"), "tool")
                self.assertEqual(messages[input_index - 1].get("tool_call_id"), "calc")
                self.assertEqual(job_calls[0]["tools"], job_calls[1]["tools"], "steering changed the tool ceiling")
                self.assertEqual(len(job_calls), 2)
                process = JobPtyProcess([BINARY, "--config", configs["client"], "inspect-only"], env, home)
                process.resize(110, 35)
                process.expect("inspect-only")
                process.send("\x07")
                process.expect("Tasks")
                process.expect("Context · not a job")
                process.send("\x1b[B")
                process.expect("Completed")
                process.readability_sizes(evidence, "completed", "Completed", "continued job terminal")
                process.send("\x1b[6~")  # Read recent activity below the task/result.
                process.expect("Tool result: calculate")
                process.send("\x1b[5~")
                process.expect("Result")
                process.send("\r")
                process.expect("continued job terminal")
                process.expect("Included · call 1 returned")
                process.resize(80, 24)
                process.expect("continued job terminal")
                process.screen_capture(evidence, "observer-completed-80x24")
                process.send("must not reopen\r")
                process.expect("Finished job")
                self.assertEqual(len(job_calls), 2, "completed observer cannot execute again")
                process.send("\x07")
                process.expect("Tasks")
                service.send_signal(signal.SIGSTOP)
                process.send("r")
                process.expect("Stale · refresh failed/timed out", timeout=15)
                process.expect("original job task")  # Retained, explicitly stale snapshot.
                process.send("\x07")
                process.expect("Unavailable", timeout=10)  # Tab reads are bounded too.
                self.assertEqual(len(job_calls), 2)
                process.send("\x07")
                service.send_signal(signal.SIGCONT)
                process.send("r")
                process.expect_absent("Stale · refresh failed/timed out")
                self.assertEqual(len(job_calls), 2)
                process.send("\x03")
                self.assertEqual(process.child.wait(timeout=10), 0)
            finally:
                release.set()
                second_release.set()
                if process:
                    process.drain(0.1)
                    process.capture(evidence / "final-view")
                    process.close()
                if service and service.poll() is None:
                    service.send_signal(signal.SIGCONT)
                for child in [executor, service]:
                    if child:
                        child.send_signal(signal.SIGINT)
                        try:
                            child.wait(timeout=10)
                        except subprocess.TimeoutExpired:
                            child.kill()
                            child.wait(timeout=5)
                if service:
                    (evidence / "service-stderr.log").write_bytes(service.stderr.read())
                    service.stdout.close()
                    service.stderr.close()
                log.close()
                for state in [home / "client-state", home / "executor-state"]:
                    if state.exists():
                        for state_log in state.glob("*.log*"):
                            (evidence / (state.name + "-" + state_log.name)).write_bytes(state_log.read_bytes())
                (evidence / "requests.json").write_text(json.dumps(calls, indent=2))
                if thread.ident:
                    http.shutdown()
                    thread.join(timeout=5)
                http.server_close()
                print(f"Job monitor PTY evidence: {evidence}")


if __name__ == "__main__":
    unittest.main(verbosity=2)
