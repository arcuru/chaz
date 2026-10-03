#!/usr/bin/env python3
"""Linux PTY smoke test of the built TUI; stdlib only, no live services."""

import errno
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import pty
import queue
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import unittest
from http.server import HTTPServer

ROOT = Path(__file__).resolve().parents[2]
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")).resolve()
BINARY = TARGET / "debug/chaz"
FIXTURE = TARGET / "debug/examples/tui_fixture"
spec = importlib.util.spec_from_file_location(
    "stub_llm", ROOT / "dev/matrix-e2e/stub_llm.py"
)
stub = importlib.util.module_from_spec(spec)
sys.dont_write_bytecode = True
spec.loader.exec_module(stub)
CSI = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")


class PtyProcess:
    def __init__(self, command, env, cwd):
        self.raw = bytearray()
        self.checkpoints = []
        self.command = [str(arg) for arg in command]
        self.master, self.slave = pty.openpty()
        self.resize(40, 16)
        # A fresh session must acquire the slave as its controlling terminal:
        # crossterm reads /dev/tty, not merely the redirected stdin.
        wrapper = (
            "import fcntl,termios,os,sys; "
            "fcntl.ioctl(0,termios.TIOCSCTTY,0); "
            "os.execv(sys.argv[1],sys.argv[1:])"
        )
        try:
            self.child = subprocess.Popen(
                [sys.executable, "-c", wrapper, *self.command],
                stdin=self.slave, stdout=self.slave, stderr=self.slave,
                env=env, cwd=cwd, start_new_session=True,
            )
        except BaseException:
            os.close(self.master)
            os.close(self.slave)
            raise

    def resize(self, columns, rows):
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))
        self.checkpoints.append({"resize": [columns, rows], "time": time.monotonic()})
        if hasattr(self, "child"):
            os.kill(self.child.pid, signal.SIGWINCH)

    def send(self, text):
        os.write(self.master, text.encode("utf-8"))

    def drain(self, timeout):
        if select.select([self.master], [], [], timeout)[0]:
            try:
                self.raw.extend(os.read(self.master, 65536))
            except OSError as error:
                if error.errno != errno.EIO:
                    raise

    def expect(self, text, timeout=20, offset=0):
        deadline = time.monotonic() + timeout
        while True:
            self.drain(min(0.05, max(0, deadline - time.monotonic())))
            if text.encode() in CSI.sub(b"", bytes(self.raw[offset:])):
                self.checkpoints.append({"seen": text, "time": time.monotonic()})
                return
            if self.child.poll() is not None:
                raise AssertionError(f"child exited {self.child.returncode} before {text!r}")
            if time.monotonic() >= deadline:
                raise AssertionError(f"deadline waiting for {text!r}")

    def close(self):
        try:
            if self.child.poll() is None:
                try:
                    os.killpg(self.child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass  # It exited between poll() and killpg(); still reap below.
            self.child.wait(timeout=5)
        finally:
            os.close(self.master)
            os.close(self.slave)
        # wait() must have reaped, not just sent a signal.
        try:
            os.waitpid(self.child.pid, os.WNOHANG)
        except ChildProcessError:
            return
        raise AssertionError("child was not reaped")

    def capture(self, directory):
        # Evidence failure must not replace the original assertion/traceback.
        try:
            directory.mkdir(parents=True, exist_ok=True)
            (directory / "pty.raw").write_bytes(self.raw)
            (directory / "checkpoints.json").write_text(json.dumps({
                "command": self.command,
                "binary_sha256": hashlib.sha256(Path(self.command[0]).read_bytes()).hexdigest(),
                "pid": self.child.pid,
                "returncode": self.child.poll(),
                "checkpoints": self.checkpoints,
            }, indent=2))
            print(f"PTY artifacts: {directory}", file=sys.stderr)
        except Exception as error:
            print(f"artifact capture failed: {error}", file=sys.stderr)


class Lifecycle(unittest.TestCase):
    def test_negative_controls(self):
        for code, reason in [("pass", "child exited"), ("import time; time.sleep(60)", "deadline")]:
            with self.subTest(reason=reason), tempfile.TemporaryDirectory() as directory:
                process = PtyProcess([sys.executable, "-c", code], {}, directory)
                try:
                    with self.assertRaisesRegex(AssertionError, reason):
                        try:
                            process.expect("absent-marker", timeout=0.3)
                        except AssertionError:
                            evidence = Path(directory) / "evidence"
                            process.capture(evidence)
                            self.assertTrue((evidence / "pty.raw").exists())
                            # A file cannot be an artifact directory. Even this
                            # write failure must preserve the missing-text error.
                            process.capture(evidence / "pty.raw")
                            raise
                finally:
                    process.close()
                self.assertIsNotNone(process.child.returncode)

    def test_tui_lifecycle(self):
        received = queue.Queue()
        requests = []

        class Handler(stub.Handler):
            def setup(self):
                super().setup()
                self.connection.settimeout(5)

            @staticmethod
            def _user_text(messages):
                text = stub.Handler._user_text(messages)
                requests.append(messages)
                received.put(messages)
                return text

            def log_message(self, *_args):
                pass

        server = HTTPServer(("127.0.0.1", 0), Handler)
        server.timeout = 2
        server.reply = "pty-first-ok"
        port = server.server_port
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        process = None
        with tempfile.TemporaryDirectory(prefix="chaz-pty-") as directory:
            home = Path(directory)
            # Deliberately do not inherit provider keys, proxy variables or user paths.
            env = {"PATH": str(home / "bin"), "HOME": directory, "TERM": "xterm-256color",
                   "LANG": "C.UTF-8", "RUST_LOG": "warn"}
            for name in ["CONFIG", "DATA", "STATE", "CACHE", "RUNTIME"]:
                path = home / name.lower()
                path.mkdir(mode=0o700)
                env[f"XDG_{name}_HOME" if name != "RUNTIME" else "XDG_RUNTIME_DIR"] = str(path)
            (home / "bin").mkdir()
            for cli in ["claude", "pi", "codex", "chaz", "gemini"]:
                path = home / "bin" / cli
                path.write_text("#!/bin/sh\necho 'unexpected model CLI' >&2\nexit 97\n")
                path.chmod(0o700)
            # Preserve only the build shell's loader search path on NixOS.
            if "LD_LIBRARY_PATH" in os.environ:
                env["LD_LIBRARY_PATH"] = os.environ["LD_LIBRARY_PATH"]
            config = home / "config.yaml"
            config.write_text(f'''execution: executor
state_dir: "{home}/state"
unlock_password: pty-fixture-only
eidetica:
  connection: "sqlite://{home}/fixture.db"
  login:
    username: pty-test
    passwordless: true
backends:
  - name: stub
    type: openaicompatible
    api_base: "http://127.0.0.1:{port}/v1"
    api_key: not-a-real-key
    models:
      - name: stub
agents:
  - name: chaz
    model: stub
    system_prompt: You are a test fixture.
default_agents: [chaz]
''')
            try:
                subprocess.run([FIXTURE, home / "fixture.db"], env=env, cwd=home,
                               check=True, timeout=20, capture_output=True)
                process = PtyProcess([BINARY, "--config", config], env, home)
                thread.start()
                process.expect("Chaz")
                # Fresh-store picker loading settles automatically into chat.
                process.expect(" > ")
                prompt = "pty-café-界-e\u0301"
                process.send(prompt + "\r")
                self.assertIn(prompt, [m.get("content") for m in received.get(timeout=20)
                                       if m.get("role") == "user"])
                process.expect(server.reply)
                process.resize(72, 24)
                server.reply = "pty-resized-ok"
                offset = len(process.raw)
                process.send("pty-after-resize\r")
                self.assertIn("pty-after-resize", [m.get("content") for m in received.get(timeout=20)
                                                   if m.get("role") == "user"])
                process.expect(server.reply, offset=offset)
                process.send("\x03")
                self.assertEqual(process.child.wait(timeout=10), 0)
                process.drain(0.1)
                self.assertIn(b"\x1b[?1049l", process.raw, "alternate screen was not restored")
                self.assertEqual(termios.tcgetattr(process.slave)[3] & (termios.ICANON | termios.ECHO),
                                 termios.ICANON | termios.ECHO, "raw mode was not restored")
            except BaseException:
                if process:
                    artifact = ROOT / "target/tui-pty-failures" / str(time.time_ns())
                    process.capture(artifact)
                    try:
                        (artifact / "requests.json").write_text(json.dumps(requests, indent=2))
                    except Exception as error:
                        print(f"request artifact failed: {error}", file=sys.stderr)
                raise
            finally:
                try:
                    if process:
                        process.close()
                finally:
                    if thread.ident is not None:
                        server.shutdown()
                        thread.join(timeout=5)
                        self.assertFalse(thread.is_alive(), "stub thread survived shutdown")
                    server.server_close()
            with self.assertRaises(OSError):
                # The listener itself is closed, not just an idle serve loop.
                server.socket.getsockname()


if __name__ == "__main__":
    unittest.main(verbosity=2)
