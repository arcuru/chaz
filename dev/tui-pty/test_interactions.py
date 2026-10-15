#!/usr/bin/env python3
# /// script
# dependencies = ["pyte==0.8.2"]
# ///
"""Approval ownership and visible pending-exit decisions on disposable peers."""
import base64
import json
import re
import shutil
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
import unicodedata
from http.server import HTTPServer

sys.dont_write_bytecode = True

from test_lifecycle import BINARY, FIXTURE, ROOT, TARGET, PtyProcess, stub


def environment(home):
    (home / "bin").mkdir()
    env = {"HOME": str(home), "PATH": str(home / "bin"), "TERM": "xterm-256color",
           "LANG": "C.UTF-8", "RUST_LOG": "warn"}
    if "LD_LIBRARY_PATH" in os.environ:
        env["LD_LIBRARY_PATH"] = os.environ["LD_LIBRARY_PATH"]
    for cli in ["claude", "pi", "codex", "chaz", "gemini"]:
        path = home / "bin" / cli
        path.write_text("#!/bin/sh\necho unexpected-model-CLI >&2\nexit 97\n")
        path.chmod(0o700)
    for name in ["CONFIG", "DATA", "STATE", "CACHE", "RUNTIME"]:
        path = home / name.lower()
        path.mkdir(mode=0o700)
        env[f"XDG_{name}_HOME" if name != "RUNTIME" else "XDG_RUNTIME_DIR"] = str(path)
    return env


def evidence(name):
    return Path(os.environ.get("TUI_EVIDENCE", ROOT / "target/tui-interaction-pty")) / name / str(time.time_ns())


class Interactions(unittest.TestCase):
    def test_conversation_selection_copies_source_text(self):
        self.selection_case(False)

    @unittest.skipUnless(shutil.which("tmux"), "tmux is not installed")
    def test_conversation_selection_reaches_tmux_clipboard(self):
        self.selection_case(True)

    def selection_case(self, multiplexed):
        ordinary = "Selection ordinary café 界 e\u0301."
        screen_ordinary = unicodedata.normalize("NFC", ordinary)
        code = 'fn main() {\n    println!("hello");\n}'
        reply = ordinary + "\n\n```rust\n" + code + "\n```\n" + "\n".join(
            f"history filler {i:02}" for i in range(35))

        class Handler(stub.Handler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                self.rfile.read(int(self.headers["Content-Length"]))
                self._fixture_reply(content=reply)

        http = HTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=http.serve_forever, daemon=True)
        thread.start()
        process = None
        socket = None
        out = evidence("selection-tmux" if multiplexed else "selection-direct")
        try:
            with tempfile.TemporaryDirectory(prefix="chaz-selection-pty-") as directory:
                home = Path(directory)
                env = environment(home)
                config = home / "config.yaml"
                config.write_text(f'''execution: executor
state_dir: "{home}/state"
unlock_password: fixture-only
eidetica:
  connection: "sqlite://{home}/fixture.db"
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
    tools: []
    system_prompt: You are a disposable test fixture.
default_agents: [chaz]
''')
                subprocess.run([FIXTURE, home / "fixture.db"], env=env, cwd=home,
                               check=True, timeout=20, capture_output=True)
                command = [BINARY, "--config", config, "--session", "selection"]
                if multiplexed:
                    socket_dir = Path(os.environ.get("TUI_TMUX_SOCKET_DIR", "/tmp/chaz-tmux-sockets"))
                    socket_dir.mkdir(parents=True, exist_ok=True)
                    socket = socket_dir / ("chaz-copy-" + str(time.time_ns()) + ".sock")
                    tmux = shutil.which("tmux")
                    command = [tmux, "-S", socket, "-f", "/dev/null", "new-session",
                               "-s", "selection", *command]
                    # Set clipboard/mouse before the app starts; no shared server
                    # or user configuration is changed by this disposable check.
                    tmux_config = home / "tmux.conf"
                    tmux_config.write_text("set -s set-clipboard on\nset -g mouse on\n"
                                           "set -as terminal-features ',xterm-256color:clipboard'\n")
                    command[command.index("/dev/null")] = tmux_config
                    print(f"Monitor: tmux -S {socket} attach -t selection", file=sys.stderr)
                process = PtyProcess(command, env, home)
                if multiplexed:
                    # pyte 0.8.2 rejects DEC's private status-query keyword;
                    # tmux sends it during attach, before Chaz renders anything.
                    process.screen.report_device_status = lambda *_args, **_kwargs: None
                    process.stream.attach(process.screen)
                process.resize(80, 24)
                process.expect_screen("Ctrl+S settings")
                process.send("show fixture\r")
                process.expect_screen("history filler 34")
                # The reply's beginning and code really are off-screen first.
                self.assertNotIn(screen_ordinary, "\n".join(process.screen.display))
                for _ in range(20):
                    process.send("\x1b[<64;40;10M")
                    process.drain(0.05)
                    if screen_ordinary in "\n".join(process.screen.display):
                        break
                process.expect_screen(screen_ordinary)

                def position(fragment):
                    for row, line in enumerate(process.screen.display):
                        column = line.find(fragment)
                        if column >= 0:
                            return column + 1, row + 1
                    self.fail(f"missing selectable fragment {fragment!r}")

                results = []
                def drag(start, end, expected):
                    offset = len(process.raw)
                    process.send(f"\x1b[<0;{start[0]};{start[1]}M")
                    process.send(f"\x1b[<32;{end[0]};{end[1]}M")
                    process.send(f"\x1b[<0;{end[0]};{end[1]}m")
                    deadline = time.monotonic() + 5
                    clipboard = None
                    while time.monotonic() < deadline:
                        process.drain(0.05)
                        if multiplexed:
                            result = subprocess.run([tmux, "-S", socket, "save-buffer", "-"],
                                                    capture_output=True, timeout=2)
                            if result.returncode == 0:
                                clipboard = result.stdout.decode()
                        else:
                            matches = re.findall(rb"\x1b]52;c;([A-Za-z0-9+/=]*)\x07",
                                                 process.raw[offset:])
                            if matches:
                                clipboard = base64.b64decode(matches[-1]).decode()
                        highlighted = process.screen.buffer[start[1] - 1][start[0] - 1].bg in ("blue", "0000ee")
                        if clipboard == expected and highlighted:
                            break
                    self.assertEqual(clipboard, expected)
                    self.assertIn(process.screen.buffer[start[1] - 1][start[0] - 1].bg, ("blue", "0000ee"))
                    results.append({"expected": expected, "clipboard": clipboard,
                                    "clipboard_kind": "tmux paste buffer" if multiplexed else "OSC 52 decoded request"})

                start = position("Selection ordinary")
                end = (79, start[1])  # padding reaches the source line end
                drag(start, end, ordinary)
                # Code is earlier history too, and its source indentation must
                # survive without Chaz's margin, label, fences or borders.
                start = position("fn main() {")
                end = position("}")
                drag(start, end, code)
                out.mkdir(parents=True, exist_ok=True)
                (out / "clipboard.json").write_text(json.dumps(results, ensure_ascii=False, indent=2))
                process.capture(out)
                process.send("\x03")
                deadline = time.monotonic() + 5
                while process.child.poll() is None and time.monotonic() < deadline:
                    process.drain(0.05)
                self.assertEqual(process.child.poll(), 0)
        except BaseException:
            if process:
                process.capture(out)
            raise
        finally:
            if process:
                process.close()
            if socket:
                subprocess.run([shutil.which("tmux"), "-S", socket, "kill-server"],
                               capture_output=True, timeout=5)
            http.shutdown()
            http.server_close()
            thread.join(timeout=5)

    def test_join_keeps_real_approval_prompts_and_explicit_decisions(self):
        self.approval_reopen_case(False)

    def test_discarded_join_keeps_real_approval_prompts_and_explicit_decisions(self):
        self.approval_reopen_case(True)

    def approval_reopen_case(self, stale):
        calls = []

        class Handler(stub.Handler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                calls.append(body)
                if self._has_tool_result(body["messages"]):
                    self._fixture_reply(content="APPROVAL-FINAL-" + str(len(calls)))
                else:
                    self.assert_shell(body)
                    self._fixture_reply(name="shell", arguments={"command": "echo MUST-NOT-EXECUTE"},
                                        call_id="approval-" + str(len(calls)))

            @staticmethod
            def assert_shell(body):
                assert any(t["function"]["name"] == "shell" for t in body["tools"])

        http = HTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=http.serve_forever, daemon=True)
        thread.start()
        process = None
        out = evidence("stale-join-approval" if stale else "join-approval")
        try:
            with tempfile.TemporaryDirectory(prefix="chaz-approval-pty-") as directory:
                home = Path(directory)
                env = environment(home)
                config = home / "config.yaml"
                config.write_text(f'''execution: executor
state_dir: "{home}/state"
unlock_password: fixture-only
eidetica:
  connection: "sqlite://{home}/fixture.db"
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
    tools: [shell]
    system_prompt: You are a disposable test fixture.
default_agents: [chaz]
security:
  tool_policies:
    shell:
      approval: always
''')
                subprocess.run([FIXTURE, home / "fixture.db"], env=env, cwd=home, check=True,
                               timeout=20, capture_output=True)
                process = PtyProcess([BINARY, "--config", config, "--session", "approval"], env, home)
                try:
                    process.resize(120, 40)
                    process.expect_screen("Ctrl+S settings")
                    for turn in range(2):
                        if turn:
                            if stale:
                                os.kill(process.child.pid, signal.SIGSTOP)
                                process.send("/join approval\r\x13")
                                os.kill(process.child.pid, signal.SIGCONT)
                                process.expect_screen("Session Settings")
                            else:
                                process.send("/join approval\r")
                            deadline = time.monotonic() + 1
                            while time.monotonic() < deadline:
                                process.drain(0.05)
                            if stale:
                                process.send("\x1b")
                                process.expect_screen("Ctrl+S settings")
                        process.send(f"approval-turn-{turn}\r")
                        process.expect_screen("[y]", timeout=15)
                        self.assertEqual(len(calls), 2 * turn + 1,
                                         "tool was decided without a user decision")
                        process.capture(out / f"prompt-{turn}")
                        process.send("n")  # Every real gated request has an explicit denial.
                        process.expect_screen(f"APPROVAL-FINAL-{2 * turn + 2}")
                        result = next(m for m in reversed(calls[-1]["messages"]) if m.get("role") == "tool")
                        self.assertIn("denied", str(result).lower())
                        self.assertNotIn("MUST-NOT-EXECUTE\n", str(result))
                    self.assertEqual(len(calls), 4)
                    process.send("\x03")
                    self.assertEqual(process.child.wait(timeout=15), 0)
                    print(f"JOIN APPROVAL CONTRACT: PASS; stale={stale}, both real prompts precede explicit denials")
                finally:
                    process.drain(0.1)
                    process.capture(out / "final")
                    (out / "requests.json").write_text(json.dumps(calls, indent=2))
                    process.close()
        finally:
            http.shutdown()
            http.server_close()
            thread.join(timeout=5)

    def test_pending_exit_visible_over_settings_and_model_edits(self):
        for mode in ["inline", "picker", "model"]:
            with self.subTest(mode=mode), tempfile.TemporaryDirectory(prefix="chaz-quit-pty-") as directory:
                home = Path(directory)
                env = environment(home)
                config = home / "config.yaml"
                config.write_text(f'''execution: client
state_dir: {home}/state
eidetica:
  connection: unix://{home}/service.sock
  login:
    username: pty-service
    passwordless: true
''')
                owner = subprocess.Popen([TARGET / "debug/examples/tui_service_fixture", home / "service.sock"],
                                         env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                         stderr=subprocess.PIPE, text=True)
                process = None
                out = evidence("pending-exit-" + mode)
                try:
                    self.assertTrue(select.select([owner.stdout], [], [], 15)[0])
                    self.assertTrue(owner.stdout.readline().startswith("READY"))
                    process = PtyProcess([BINARY, "--config", config, "--session", "service-fixture"], env, home)
                    process.resize(120, 40)
                    process.expect_screen("SERVICE-CACHED-STATUS")
                    owner.send_signal(signal.SIGSTOP)
                    process.send("accepted-held-write\r")
                    process.expect_screen("action(s) pending")
                    if mode == "inline":
                        process.send("\x10")
                        process.expect_screen("Sessions")
                        process.send("s")
                        process.expect_screen("Peer Settings")
                        process.send("\x1b[B\x1b[Ba")
                        process.expect_screen("add default agent")
                    elif mode == "picker":
                        process.send("\x13")
                        process.expect_screen("Session Settings")
                        process.send("\x1b[Ba")
                        process.expect_screen("add agent")
                    else:
                        process.send("/models\r")
                        process.expect_screen("Session Settings")
                        process.send("\r")
                        process.expect_screen("filter")
                    process.send("retained-edit-draft")
                    process.expect_screen("retained-edit-draft")
                    process.send("\x03")
                    process.expect_screen("Quit again", timeout=1)
                    self.assertIsNone(process.child.poll(), "first quit abandoned accepted work")
                    self.assertIn("retained-edit-draft", "\n".join(process.screen.display))
                    self.assertNotIn(b"\x1b[?1049l", process.raw)
                    process.capture(out / "first-quit")
                    process.send("\x03")
                    process.expect("Exited with unfinished/unconfirmed actions", timeout=3)
                    process.drain(0.1)
                    self.assertIn(b"accepted-held-write", process.raw)
                    self.assertEqual(bytes(process.raw).count(b"\x1b[?1049l"), 1)
                    owner.send_signal(signal.SIGCONT)
                    self.assertEqual(process.child.wait(timeout=15), 0)
                    print(f"PENDING EDIT EXIT CONTRACT: PASS; {mode} warning/draft/explicit second quit")
                finally:
                    if process:
                        process.drain(0.1)
                        process.capture(out / "final")
                        process.close()
                    if owner.poll() is None:
                        owner.send_signal(signal.SIGCONT)
                        owner.stdin.write("quit\n")
                        owner.stdin.flush()
                        try:
                            owner.wait(timeout=10)
                        except subprocess.TimeoutExpired:
                            owner.kill()
                            owner.wait(timeout=5)
                    owner.stdin.close()
                    owner.stdout.close()
                    owner.stderr.close()


if __name__ == "__main__":
    unittest.main(verbosity=2)
