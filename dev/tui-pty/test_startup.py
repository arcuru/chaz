#!/usr/bin/env python3
# /// script
# dependencies = ["pyte==0.8.2"]
# ///
"""First-frame and usable-session latency are separate, observable contracts."""
import json
from pathlib import Path
import select
import socket
import struct
import subprocess
import tempfile
import threading
import time
import unittest
import sys

sys.dont_write_bytecode = True

from test_lifecycle import BINARY, TARGET, PtyProcess
from test_interactions import environment, evidence


class Gate:
    """Pass through real IPC, holding exactly one matching request before dispatch."""
    def __init__(self, path, upstream, predicate):
        self.hit = threading.Event()
        self.release = threading.Event()
        self.predicate = predicate
        self.held = None
        self.failure = None
        self.connections = []
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(path))
        self.listener.listen(1)
        self.thread = threading.Thread(target=self.run, args=(str(upstream),))
        self.thread.start()

    @staticmethod
    def read(sock, size):
        result = bytearray()
        while len(result) < size:
            part = sock.recv(size - len(result))
            if not part:
                return None
            result.extend(part)
        return bytes(result)

    def run(self, upstream):
        reverse = None
        try:
            client, _ = self.listener.accept()
            server = socket.socket(socket.AF_UNIX)
            self.connections.extend([client, server])
            server.connect(upstream)

            def replies():
                try:
                    while data := server.recv(65536):
                        client.sendall(data)
                except OSError:
                    pass  # Peer teardown is expected on quit while held.
            reverse = threading.Thread(target=replies)
            reverse.start()
            while header := self.read(client, 4):
                payload = self.read(client, struct.unpack(">I", header)[0])
                if payload is None:
                    break
                request = json.loads(payload)
                if self.held is None and self.predicate(request):
                    self.held = request
                    self.hit.set()
                    if not self.release.wait(timeout=30):
                        raise AssertionError("IPC gate was never released")
                server.sendall(header + payload)
        except OSError:
            pass  # Tests also exercise cancelled connection reads.
        except BaseException as error:
            self.failure = error
        finally:
            for connection in self.connections:
                try:
                    connection.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                connection.close()
            if reverse:
                reverse.join(timeout=5)

    def close(self):
        self.release.set()
        for connection in self.connections:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        try:
            self.listener.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.listener.close()
        self.thread.join(timeout=5)
        assert not self.thread.is_alive(), "IPC fixture thread survived"
        if self.failure:
            raise self.failure


class Startup(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="chaz-startup-pty-")
        self.home = Path(self.temp.name)
        self.env = environment(self.home)
        self.env["RUST_LOG"] = "info"
        self.owner = subprocess.Popen(
            [TARGET / "debug/examples/tui_service_fixture", self.home / "owner.sock"],
            env=self.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True)
        self.reply("READY")
        self.process = self.gate = None
        self.out = evidence(self._testMethodName)

    def reply(self, prefix):
        self.assertTrue(select.select([self.owner.stdout], [], [], 15)[0], "fixture deadline")
        line = self.owner.stdout.readline().strip()
        self.assertTrue(line.startswith(prefix), (prefix, line))
        return line

    def control(self, command, prefix):
        self.owner.stdin.write(command + "\n")
        self.owner.stdin.flush()
        return self.reply(prefix)

    def launch(self, predicate, *args, username="pty-service"):
        self.gate = Gate(self.home / "proxy.sock", self.home / "owner.sock", predicate)
        config = self.home / "config.yaml"
        config.write_text(f'''execution: client
state_dir: "{self.home}/state"
eidetica:
  connection: "unix://{self.home}/proxy.sock"
  login:
    username: {username}
    passwordless: true
''')
        self.started = time.monotonic()
        self.process = PtyProcess([BINARY, "--config", config, *args], self.env, self.home)
        self.process.resize(120, 40)

    def held(self):
        deadline = time.monotonic() + 10
        while not self.gate.hit.is_set():
            self.assertLess(time.monotonic(), deadline, "the positive IPC gate was not reached")
            self.process.drain(0.05)
        self.assertIsNone(self.process.child.poll())
        self.assertFalse(self.gate.release.is_set())
        self.out.mkdir(parents=True, exist_ok=True)
        (self.out / "held-request.json").write_text(json.dumps(self.gate.held, indent=2))

    def restored(self):
        import termios
        self.process.drain(0.1)
        self.assertEqual(bytes(self.process.raw).count(b"\x1b[?1049l"), 1)
        flags = termios.tcgetattr(self.process.slave)[3]
        self.assertTrue(flags & termios.ICANON and flags & termios.ECHO)

    def wait_restored(self):
        deadline = time.monotonic() + 1
        while b"\x1b[?1049l" not in self.process.raw:
            self.assertLess(time.monotonic(), deadline, "terminal not promptly restored")
            self.process.drain(0.05)

    def quit(self, code=0):
        self.process.send("\x03")
        self.assertEqual(self.process.child.wait(timeout=10), code)
        self.restored()

    def quit_while_held(self):
        self.process.send("\x03")
        self.wait_restored()
        # Give an incorrectly cancelled/detached worker time to exit while
        # the request is still held; polling once at restoration can race it.
        deadline = time.monotonic() + 0.2
        while time.monotonic() < deadline:
            self.process.drain(0.05)
            self.assertIsNone(self.process.child.poll(), "startup work was detached/cancelled")
            self.assertFalse(self.gate.release.is_set())
        self.gate.release.set()
        self.assertEqual(self.process.child.wait(timeout=10), 0)
        self.restored()

    def capture(self):
        if self.process:
            self.process.drain(0.1)
            self.process.capture(self.out)
        for log in (self.home / "state").glob("chaz-tui*.log*"):
            (self.out / log.name).write_bytes(log.read_bytes())

    def tearDown(self):
        try:
            self.capture()
        finally:
            try:
                if self.gate:
                    self.gate.close()
            finally:
                try:
                    if self.process:
                        self.process.close()
                finally:
                    try:
                        self.owner.stdin.write("quit\n")
                        self.owner.stdin.flush()
                        self.owner.wait(timeout=10)
                    finally:
                        if self.owner.poll() is None:
                            self.owner.kill()
                            self.owner.wait(timeout=5)
                        self.owner.stdin.close()
                        self.owner.stdout.close()
                        self.owner.stderr.close()
                        self.temp.cleanup()

    def test_first_frame_and_quit_while_connection_is_blocked(self):
        self.launch(lambda r: "protocol_version" in r)
        self.process.expect_screen("Connecting", timeout=1)
        first = (time.monotonic() - self.started) * 1000
        self.held()
        self.process.expect_screen("New session", timeout=1)
        self.process.send("\x1bOP")  # F1
        self.process.expect_screen("Help", timeout=1)
        self.process.send("\x1b")
        deadline = time.monotonic() + 1
        # The hub is also visible behind Help. Wait for the overlay to actually
        # close before the next byte, or Esc+n is a single Alt+n key.
        while "Help —" in "\n".join(self.process.screen.display):
            self.assertLess(time.monotonic(), deadline, "Help did not close")
            self.process.drain(0.05)
        self.process.expect_screen("New session", timeout=1)
        self.process.send("n")
        self.process.expect_screen("action not accepted", timeout=1)
        self.assertEqual(self.control("sessions", "SESSIONS"), "SESSIONS 1")
        self.quit()
        self.assertFalse(self.gate.release.is_set(), "quit needed the connector to answer")
        print(f"STARTUP CONNECTION CONTRACT: PASS; first-frame={first:.3f}ms, gate unreleased at process exit")

    def test_runtime_build_quit_restores_before_joining_owned_work(self):
        self.launch(lambda r: "AuthenticatedDb" in r and '"peer_defaults"' in json.dumps(r))
        self.held()
        self.process.expect_screen("Initializing runtime", timeout=1)
        self.process.send("\x1bOP")
        self.process.expect_screen("Help", timeout=1)
        self.quit_while_held()
        self.assertEqual(self.control("sessions", "SESSIONS"), "SESSIONS 1")
        print("STARTUP RUNTIME CONTRACT: PASS; input live, restore precedes joined construction, no new session")

    def test_named_loading_retains_edits_and_readiness_is_measured_separately(self):
        self.launch(lambda r: "AuthenticatedDb" in r and '"entries"' in json.dumps(r),
                    "--session", "service-fixture", "startup-draft")
        self.process.expect_screen("Connecting", timeout=1)
        first = (time.monotonic() - self.started) * 1000
        self.held()
        self.process.expect_screen("Loading session", timeout=1)
        self.process.expect_screen("service-fixture · loading", timeout=1)
        self.process.send("-界EDIT\x7f!\r")
        self.process.expect_screen("startup-draft-界EDI!", timeout=1)
        self.assertEqual(self.control("entries", "ENTRIES"), "ENTRIES 1", "loading Enter submitted a draft")
        # Deliberately separate first-frame latency from backend usability.
        until = time.monotonic() + 0.3
        while time.monotonic() < until:
            self.process.drain(0.05)
        self.assertFalse(self.gate.release.is_set())
        self.gate.release.set()
        self.process.expect_screen("SERVICE-CACHED-TRANSCRIPT", timeout=15)
        self.process.expect_screen("Ctrl+S settings", timeout=1)
        self.process.expect_screen("startup-draft-界EDI!", timeout=1)
        ready = (time.monotonic() - self.started) * 1000
        self.assertGreater(ready, first + 250)
        self.assertEqual(self.control("entries", "ENTRIES"), "ENTRIES 1")
        self.assertEqual(self.control("sessions", "SESSIONS"), "SESSIONS 1")
        self.quit()
        logs = "\n".join(log.read_text() for log in sorted((self.home / "state").glob("chaz-tui*.log*")))
        milestones = ["TUI first frame drawn", "Session registry callback installed",
                      "TUI launch ready", "TUI ready frame drawn"]
        for event in milestones:
            self.assertEqual(logs.count(event), 1, f"ambiguous readiness milestone: {event}")
        positions = [logs.index(event) for event in milestones]
        self.assertEqual(positions, sorted(positions), "first frame and frontend readiness are distinct")
        (self.out / "timings.json").write_text(json.dumps({"first_frame_ms": first, "usable_session_ms": ready}))
        print(f"STARTUP NAMED CONTRACT: PASS; first-frame={first:.3f}ms, usable-session={ready:.3f}ms; draft unsent and retained")

    def test_quit_during_named_loading_joins_without_duplicate_creation(self):
        self.launch(lambda r: "AuthenticatedDb" in r and '"entries"' in json.dumps(r),
                    "--session", "service-fixture")
        self.held()
        self.process.expect_screen("Loading session", timeout=1)
        self.quit_while_held()
        self.assertEqual(self.control("sessions", "SESSIONS"), "SESSIONS 1")
        self.assertEqual(self.control("entries", "ENTRIES"), "ENTRIES 1")
        print("STARTUP NAMED QUIT CONTRACT: PASS; joined loading, unchanged catalog/transcript")

    def test_named_load_error_does_not_replace_the_session_and_relaunch_recovers(self):
        self.control("corrupt-meta", "META corrupt-meta")
        self.launch(lambda r: False, "--session", "service-fixture", "keep-this")
        self.process.expect_screen("Startup failed", timeout=15)
        self.process.send("-界EDIT")
        self.process.expect_screen("keep-this-界EDIT", timeout=1)
        self.assertNotIn(b"\x1b[?1049l", self.process.raw)
        self.quit(code=1)
        self.process.capture(self.out / "failed-load")
        self.gate.close()
        self.process.close()
        self.gate = self.process = None
        (self.home / "proxy.sock").unlink()
        self.assertEqual(self.control("sessions", "SESSIONS"), "SESSIONS 1")
        self.control("repair-meta", "META repair-meta")
        self.launch(lambda r: False, "--session", "service-fixture", "keep-this-界EDIT")
        self.process.expect_screen("SERVICE-CACHED-TRANSCRIPT", timeout=15)
        self.process.expect_screen("keep-this-界EDIT", timeout=1)
        self.assertEqual(self.control("sessions", "SESSIONS"), "SESSIONS 1")
        self.assertEqual(self.control("entries", "ENTRIES"), "ENTRIES 1")
        self.quit()
        print("STARTUP LOAD ERROR CONTRACT: PASS; live error, same session after repair/relaunch, no submitted draft")

    def test_login_error_is_visible_and_does_not_restore_a_live_terminal(self):
        self.launch(lambda r: False, "--session", "must-not-create", "retained", username="absent-user")
        self.process.expect_screen("Startup failed", timeout=10)
        self.assertNotIn(b"\x1b[?1049l", self.process.raw)
        self.process.send("-界ERROR")
        self.process.expect_screen("retained-界ERROR", timeout=1)
        self.quit(code=1)
        self.assertEqual(self.control("sessions", "SESSIONS"), "SESSIONS 1")
        print("STARTUP ERROR CONTRACT: PASS; live error/draft, no fallback session, restored once on explicit exit")


if __name__ == "__main__":
    unittest.main(verbosity=2)
