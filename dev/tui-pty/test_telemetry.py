#!/usr/bin/env python3
# /// script
# dependencies = ["pyte==0.8.2"]
# ///
"""Per-call status redraws on a real TUI with a blocked final model call."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import HTTPServer

sys.dont_write_bytecode = True

from test_interactions import environment, evidence
from test_lifecycle import BINARY, FIXTURE, PtyProcess, stub


class Telemetry(unittest.TestCase):
    def test_usage_redraws_without_keys_before_final_and_survives_reopen(self):
        release = [threading.Event() for _ in range(3)]
        arrived = [threading.Event() for _ in range(3)]
        calls = []

        class Handler(stub.Handler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                index = len(calls)
                calls.append(body)
                if index >= 3:
                    self._send({"error": "unexpected fourth call"}, status=400)
                    return
                arrived[index].set()
                if not release[index].wait(30):
                    self._send({"error": "fixture gate timed out"}, status=400)
                    return
                message = {"role": "assistant", "content": "TELEMETRY-FINAL" if index == 2 else None}
                if index < 2:
                    message["tool_calls"] = [{
                        "id": f"call-{index}", "type": "function",
                        "function": {"name": "calculate", "arguments": '{"expression":"1+1"}'},
                    }]
                prompt = (index + 1) * 1000
                self._send({
                    "id": f"telemetry-{index}", "object": "chat.completion", "created": 0,
                    "model": "gpt-6.1-sol",
                    "choices": [{"index": 0, "message": message,
                                 "finish_reason": "stop" if index == 2 else "tool_calls"}],
                    "usage": {"prompt_tokens": prompt, "completion_tokens": 100,
                              "total_tokens": prompt + 100,
                              "prompt_tokens_details": {"cached_tokens": prompt // 2}, "cost": 0.01},
                })

        http = HTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=http.serve_forever, daemon=True)
        thread.start()
        process = None
        out = evidence("telemetry")
        try:
            with tempfile.TemporaryDirectory(prefix="chaz-telemetry-pty-") as directory:
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
      - name: gpt-sol-latest
        context_window: 10000
agents:
  - name: chaz
    model: gpt-sol-latest
    tools: [calculate]
    system_prompt: You are a disposable test fixture.
default_agents: [chaz]
''')
                subprocess.run([FIXTURE, home / "fixture.db"], env=env, cwd=home, check=True,
                               timeout=20, capture_output=True)
                command = [BINARY, "--config", config, "--session", "telemetry"]
                process = PtyProcess(command, env, home)
                process.resize(180, 35)
                process.expect_screen("ctx unknown/10k tok")
                process.send("telemetry fixture\r")
                self.assertTrue(arrived[0].wait(10), "first call not started")
                # No send(), resize(), or signal between these gates and redraws.
                for index, context, totals in [
                    (0, "ctx ~1k/10k (10.0%) tok", "1k/100 tok • 50% cached • $0.0100"),
                    (1, "ctx ~2k/10k (20.0%) tok", "3k/200 tok • 50% cached • $0.0200"),
                ]:
                    release[index].set()
                    self.assertTrue(arrived[index + 1].wait(10), "next call not blocked")
                    process.expect_screen(context)
                    process.expect_screen(totals)
                    self.assertNotIn("TELEMETRY-FINAL", "\n".join(process.screen.display))
                    process.capture(out / f"intermediate-{index + 1}")
                release[2].set()
                process.expect_screen("TELEMETRY-FINAL")
                expected = "6k/300 tok • 50% cached • $0.0300"
                process.expect_screen("ctx ~3k/10k (30.0%) tok")
                process.expect_screen(expected)
                process.capture(out / "finalized")
                self.assertEqual([call["model"] for call in calls], ["gpt-sol-latest"] * 3)
                # A reopened frontend sees the same durable calls, not a live counter.
                process.send("\x1b")
                process.child.wait(timeout=10)
                process.close()
                process = PtyProcess(command, env, home)
                process.resize(180, 35)
                process.expect_screen(expected)
                process.expect_screen("ctx ~3k/10k (30.0%) tok")
                process.capture(out / "reopened")
                process.send("\x1b")
                process.child.wait(timeout=10)
                self.assertEqual(len(calls), 3, "reopen caused another model call")
        finally:
            for gate in release:
                gate.set()
            if process is not None:
                process.capture(out / "exit")
                process.close()
            http.shutdown()
            http.server_close()
            thread.join(timeout=5)
            out.mkdir(parents=True, exist_ok=True)
            (out / "requests.json").write_text(json.dumps(calls, indent=2) + "\n")


if __name__ == "__main__":
    unittest.main()
