#!/usr/bin/env python3
# /// script
# dependencies = ["pyte==0.8.2"]
# ///
"""First-frame and usable-session latency are separate, observable contracts."""
import json
from datetime import datetime
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
    """Pass through real IPC; gate a request or withhold replies after it."""
    def __init__(self, path, upstream, predicate, hold_replies=False):
        self.hit = threading.Event()
        self.closed = threading.Event()
        self.release = threading.Event()
        self.predicate = predicate
        self.hold_replies = hold_replies
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
                        if self.hold_replies and self.hit.is_set():
                            if not self.release.wait(timeout=75):
                                self.failure = AssertionError("IPC reply gate was never released")
                                return
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
                    if not self.hold_replies and not self.release.wait(timeout=30):
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
            self.closed.set()
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


class ServicePty(unittest.TestCase):
    fixture_args = ()

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="chaz-startup-pty-")
        self.home = Path(self.temp.name)
        self.env = environment(self.home)
        self.env["RUST_LOG"] = "info"
        self.owner = subprocess.Popen(
            [TARGET / "debug/examples/tui_service_fixture", self.home / "owner.sock", *self.fixture_args],
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

    def launch(self, predicate, *args, username="pty-service", hold_replies=False):
        self.gate = Gate(self.home / "proxy.sock", self.home / "owner.sock", predicate, hold_replies)
        config = self.home / "config.yaml"
        config.write_text(f'''execution: client
state_dir: "{self.home}/state"
eidetica:
  connection: "unix://{self.home}/proxy.sock"
  login:
    username: {username}
    password: test-password
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

    def logs(self):
        return "\n".join(log.read_text() for log in sorted((self.home / "state").glob("chaz-tui*.log*")))


class Outage(ServicePty):
    fixture_args = ("outage",)

    def open_both(self):
        self.process.expect_screen("SERVICE-CACHED-TRANSCRIPT")
        self.process.send("/join secondary\r")
        self.process.expect_screen("SECONDARY-CACHED")
        self.process.send("\x08")  # Ctrl+H: return to the original tab
        self.process.expect_screen("SERVICE-CACHED-TRANSCRIPT", timeout=2)

    def test_session_read_timeout_does_not_disconnect_healthy_service(self):
        armed = threading.Event()
        self.launch(lambda r: armed.is_set() and '"entries"' in json.dumps(r)
                    and isinstance(r.get("AuthenticatedDb", {}).get("op"), dict)
                    and "GetStoreEntries" in r["AuthenticatedDb"]["op"],
                    "--session", "service-fixture")
        self.open_both()
        armed.set()
        self.control("append", "APPENDED")
        self.held()
        self.process.send("\x0c")  # Ctrl+L: cached second tab is still usable
        self.process.expect_screen("SECONDARY-CACHED", timeout=2)
        deadline = time.monotonic() + 15
        while "Refresh failed: backend read timed out" not in self.logs():
            self.assertLess(time.monotonic(), deadline, "the gated session read did not time out")
            self.process.drain(0.05)
        self.assertEqual(self.control("sessions", "SESSIONS"), "SESSIONS 2")
        self.gate.release.set()
        self.process.expect_screen("SECONDARY-FRESH", timeout=15)
        self.process.send("\x08")
        self.process.expect_screen("RECOVERY-VISIBLE", timeout=15)
        self.assertNotIn("TUI service disconnected", self.logs())
        self.assertNotIn("Backend unresponsive", "\n".join(self.process.screen.display))
        self.quit()
        print("SLOW TAB: one real session read timed out; live owner and both cached tabs recovered without reconnecting")

    def test_outage_recovery_does_not_replay_a_committed_write_with_no_reply(self):
        armed = threading.Event()
        self.launch(lambda r: armed.is_set() and isinstance(r.get("AuthenticatedDb", {}).get("op"), dict)
                    and "SubmitSignedEntry" in r["AuthenticatedDb"]["op"],
                    "--session", "service-fixture", hold_replies=True)
        self.open_both()
        self.assertEqual(self.control("entries", "ENTRIES"), "ENTRIES 1")
        armed.set()
        self.process.send("outage-accepted-write\r")
        self.held()
        # The request crossed the real socket and committed, but the client's
        # response is still withheld. This is not a server-side rejection.
        deadline = time.monotonic() + 5
        while self.control("entries", "ENTRIES") != "ENTRIES 2":
            self.assertLess(time.monotonic(), deadline, "write did not commit while the reply was held")
            self.process.drain(0.05)
        self.process.expect_screen("action(s) pending", timeout=2)
        self.process.send("queued-before-outage\r")
        self.process.expect_screen("2 action(s) pending", timeout=2)
        (self.home / "proxy.sock").unlink()  # replacement sockets cannot reach the old gate
        self.process.expect_screen("Backend unresponsive", timeout=32)
        self.assertTrue(self.gate.closed.wait(timeout=2), "old IPC connection was not retired")
        self.process.expect_screen("Write outcome unknown", timeout=3)
        self.process.expect_screen("not started", timeout=3)
        self.process.send("\x0c")
        self.process.expect_screen("SECONDARY-CACHED", timeout=2)
        self.process.send("rejected-offline-draft\r")
        self.process.expect_screen("draft not submitted", timeout=2)
        self.process.expect_screen("rejected-offline-draft", timeout=2)
        self.process.send("\x10")
        self.process.expect_screen("cached data stale", timeout=2)
        self.process.send("\x10")
        self.process.expect_screen("SECONDARY-CACHED", timeout=2)
        # Positively observe attempts while the replacement endpoint is absent.
        # Lower bounds allow for jitter but fail for fixed rapid retries or a spin.
        deadline = time.monotonic() + 12
        while True:
            retries = [datetime.fromisoformat(line.split()[0]) for line in self.logs().splitlines()
                       if "TUI service reconnect failed" in line]
            if len(retries) >= 5:
                break
            self.assertLess(time.monotonic(), deadline, "reconnect loop did not retry")
            self.process.drain(0.05)
        gaps = [(second - first).total_seconds() for first, second in zip(retries, retries[1:])]
        for gap, minimum in zip(gaps, (0.15, 0.30, 0.60, 1.20)):
            self.assertGreaterEqual(gap, minimum, "reconnect attempts bunched together")
        print(f"RECONNECT BACKOFF: observed failed-dial gaps (seconds): {gaps}")
        self.control("append", "APPENDED")
        (self.home / "proxy.sock").symlink_to(self.home / "owner.sock")
        self.process.expect_screen("SECONDARY-FRESH", timeout=20)
        self.process.expect_screen("rejected-offline-draft", timeout=2)
        self.process.send("\x08")
        self.process.expect_screen("RECOVERY-VISIBLE", timeout=20)
        self.process.expect_screen("outage-accepted-write", timeout=2)
        self.assertEqual(self.control("entries", "ENTRIES"), "ENTRIES 3", "accepted or queued write replayed")
        self.assertFalse(self.gate.release.is_set(), "recovery depended on the old connection's reply")
        exit_offset = len(self.process.raw)
        self.quit()
        self.process.expect("queued-before-outage", offset=exit_offset, timeout=2)
        print("CONNECTED OUTAGE: cached tabs and drafts stayed usable; both tabs rebound; committed write not replayed")


class Startup(ServicePty):
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

    def test_background_first_tab_keeps_a_newer_hub_open_request(self):
        armed = threading.Event()
        self.launch(lambda r: armed.is_set() and "AuthenticatedDb" in r
                    and "SubmitSignedEntry" in r["AuthenticatedDb"]["op"])
        self.process.expect_screen("Sessions", timeout=15)
        self.process.expect_screen("service-fixture", timeout=15)
        armed.set()
        self.process.send("n")
        self.held()
        self.process.send("draft-from-loading")
        self.process.expect_screen("draft-from-loading", timeout=1)
        self.process.send("\x10")  # Ctrl+P: no conversation tab is open yet
        self.process.expect_screen("Sessions", timeout=1)
        self.process.send("\x1b[B\r")  # open the existing session behind creation
        self.process.expect_screen("2 action(s) pending", timeout=1)
        self.gate.release.set()
        self.process.expect_screen("SERVICE-CACHED-TRANSCRIPT", timeout=15)
        self.process.expect_screen("Ctrl+S settings", timeout=1)
        self.assertNotIn("draft-from-loading", "\n".join(self.process.screen.display))
        self.process.send("\x0c")  # Ctrl+L: created tab retains its own draft
        self.process.expect_screen("draft-from-loading", timeout=1)
        self.assertNotIn("SERVICE-CACHED-TRANSCRIPT", "\n".join(self.process.screen.display))
        self.assertEqual(self.control("sessions", "SESSIONS"), "SESSIONS 2")
        self.quit()
        print("BACKGROUND FIRST TAB CONTRACT: PASS; newer hub navigation honored, creation draft isolated")

    def test_new_session_composer_is_editable_before_creation_finishes(self):
        # Database creation submits a signed genesis entry; there is no
        # separate CreateDatabase RPC in the native storage protocol.
        self.new_session_loading_case(lambda r: "AuthenticatedDb" in r and "SubmitSignedEntry" in r["AuthenticatedDb"]["op"])

    def test_new_session_composer_is_editable_before_hydration_finishes(self):
        self.new_session_loading_case(lambda r: "AuthenticatedDb" in r and '"entries"' in json.dumps(r))

    def new_session_loading_case(self, predicate):
        armed = threading.Event()
        self.launch(lambda r: armed.is_set() and predicate(r), "--session", "service-fixture")
        self.process.expect_screen("SERVICE-CACHED-TRANSCRIPT", timeout=15)
        self.process.expect_screen("Ctrl+S settings", timeout=1)
        self.process.send("old-draft")
        self.process.expect_screen("old-draft", timeout=1)
        armed.set()
        self.process.send("\x14")  # Ctrl+T
        self.held()
        self.process.expect_screen("New session · loading", timeout=1)
        self.process.send("new-界EDIT\x7f!\r")
        self.process.expect_screen("new-界EDI!", timeout=1)
        self.assertNotIn("old-draft", "\n".join(self.process.screen.display))
        self.assertEqual(self.control("entries", "ENTRIES"), "ENTRIES 1")
        self.process.send("\x10")  # Ctrl+P: hide, not cancel
        self.process.expect_screen("Sessions", timeout=1)
        self.process.send("\x14")  # Ctrl+T: same pending composer, not a new write
        self.process.expect_screen("new-界EDI!", timeout=1)
        self.assertFalse(self.gate.release.is_set())
        self.process.capture(self.out / "loading-composer")
        self.gate.release.set()
        self.process.expect_screen("Ctrl+S settings", timeout=15)
        self.process.expect_screen("new-界EDI!", timeout=1)
        self.assertEqual(self.control("sessions", "SESSIONS"), "SESSIONS 2")
        self.process.send("\x08")  # Ctrl+H: return to original conversation
        self.process.expect_screen("SERVICE-CACHED-TRANSCRIPT", timeout=1)
        self.process.expect_screen("old-draft", timeout=1)
        self.assertNotIn("new-界EDI!", "\n".join(self.process.screen.display))
        self.quit()
        print("NEW SESSION COMPOSER CONTRACT: PASS; editable before held IPC, unsent draft retained, no duplicate or cross-session input")

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
