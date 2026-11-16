# /// script
# dependencies = ["pyte==0.8.2"]
# ///
"""Read literal new side records using the current and an optional older TUI."""
import sys
sys.dont_write_bytecode = True
import os
from pathlib import Path
import select
import subprocess
import tempfile

from test_lifecycle import BINARY, ROOT, TARGET, PtyProcess

out = Path(os.environ.get("TUI_EVIDENCE", ROOT / "target/tui-worker-evidence")) / "compatibility"
out.mkdir(parents=True, exist_ok=True)
fixture = Path(os.environ.get("TUI_SERVICE_FIXTURE", TARGET / "debug/examples/tui_service_fixture"))
readers = [BINARY]
if old := os.environ.get("TUI_OLD_BINARY"):
    readers.append(Path(old))
with tempfile.TemporaryDirectory(prefix="worker-compatibility-") as directory:
    home = Path(directory)
    (home / "bin").mkdir()
    for cli in ["claude", "pi", "codex", "gemini", "aichat"]:
        path = home / "bin" / cli
        path.write_text("#!/bin/sh\necho unexpected-model-cli >&2\nexit 97\n")
        path.chmod(0o700)
    env = {"HOME": directory, "PATH": str(home / "bin"), "TERM": "xterm-256color", "LANG": "C.UTF-8", "RUST_LOG": "info"}
    for name in ["CONFIG", "DATA", "STATE", "CACHE", "RUNTIME"]:
        path = home / name.lower()
        path.mkdir(mode=0o700)
        env[f"XDG_{name}_HOME" if name != "RUNTIME" else "XDG_RUNTIME_DIR"] = str(path)
    if "LD_LIBRARY_PATH" in os.environ:
        env["LD_LIBRARY_PATH"] = os.environ["LD_LIBRARY_PATH"]
    config = home / "client.yaml"
    # No backend and no executor: an unexpected model request cannot leave the fixture.
    config.write_text(f'''execution: client
state_dir: "{home}/state"
eidetica:
  connection: "unix://{home}/service.sock"
  login: {{ username: pty-service, password: test-password }}
''')
    owner_log = (out / "service.log").open("wb")
    owner = subprocess.Popen([fixture, home / "service.sock", "worker-links"], env=env, cwd=home, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=owner_log, text=True)
    try:
        assert select.select([owner.stdout], [], [], 20)[0], "fixture readiness deadline"
        assert owner.stdout.readline().startswith("READY "), "fixture failed"
        for index, reader in enumerate(readers):
            for name, expected in [("service-fixture", ["SERVICE-CACHED-TRANSCRIPT", "COMPAT-PARENT-AFTER-SIDE-RECORDS"]), ("worker-compatibility", ["COMPAT-CHILD-ASSIGNMENT", "COMPAT-CHILD-RESULT"])]:
                p = PtyProcess([reader, "--config", config, "--session", name], env, home)
                try:
                    p.resize(120, 38)
                    for text in expected:
                        p.expect_screen(text)
                    p.capture(out / f"reader-{index}-{name}")
                    p.send("\x03")
                    assert p.child.wait(timeout=10) == 0
                finally:
                    p.close()
            print(f"PASS: reader {reader} preserves parent and child transcripts with literal v1 Worker side records")
        owner.stdin.write("entries\n")
        owner.stdin.flush()
        assert select.select([owner.stdout], [], [], 10)[0]
        assert owner.stdout.readline().strip() == "ENTRIES 2", "inspection changed parent transcript"
        print(f"PASS: {len(readers)} reader(s); side records ignored by legacy readers; inspection added zero parent entries")
    finally:
        if owner.poll() is None:
            owner.stdin.write("quit\n")
            owner.stdin.flush()
        try:
            owner.wait(timeout=10)
        except subprocess.TimeoutExpired:
            owner.kill()
            owner.wait(timeout=5)
        owner_log.close()
