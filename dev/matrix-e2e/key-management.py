"""Actual CLI + loopback Synapse acceptance. All accounts/state are disposable."""
import http.client
import http.server
import json
import os
from pathlib import Path
import secrets
import signal
import sqlite3
import subprocess
import threading
import time
import urllib.request

WORK = Path(os.environ["KEY_TEST_WORKSPACE"])
HS = os.environ["KEY_TEST_HOMESERVER"]
BIN = os.environ["CHAZ_MATRIX_BIN"]
PROBE = os.environ["KEY_PROBE_BIN"]
assert HS.startswith("http://127.0.0.1:")
PASSWORD = secrets.token_hex(24)
os.environ["KEY_TEST_PASSWORD"] = PASSWORD
os.environ["RUST_LOG"] = "trace"  # Maintenance must suppress SDK secret-bearing tracing.
USER = "@manager:keys.test"
CHECKS = []
LOGS = []


def check(condition, name):
    assert condition, name
    CHECKS.append(name)
    print("PASS", name, flush=True)


def request(path, body=None, token=None):
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = "Bearer " + token
    req = urllib.request.Request(HS + path, data=None if body is None else json.dumps(body).encode(), headers=headers)
    with urllib.request.urlopen(req, timeout=15) as response:
        return json.load(response)


request("/_matrix/client/v3/register", {"username": "manager", "password": PASSWORD, "auth": {"type": "m.login.dummy"}})


# Forward to the actual server, but lose the post-commit key-query response.
# This is transport uncertainty, not a production flag or fake reset outcome.
class Proxy(http.server.BaseHTTPRequestHandler):
    fail_after_commit = False
    committed = False
    lose_recovery_response = False
    recovery_committed = False
    block_generation = False
    generation_guard = None
    generation_blocked = threading.Event()

    def log_message(self, *_):
        pass

    def forward(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        if self.path.startswith("/_matrix/client/v3/keys/query") and Proxy.block_generation and Proxy.generation_guard is None and json.loads(body).get("device_keys", {}).get(USER) == []:
            active = Path(session()["client_session"]["db_path"])
            staged = max((p for p in ROOT.glob("store-*") if p != active), key=lambda p: p.stat().st_ctime_ns)
            guard = sqlite3.connect(staged / "matrix-sdk-crypto.sqlite3", timeout=2, check_same_thread=False, isolation_level=None)
            guard.execute("BEGIN IMMEDIATE")  # Real write lock blocks SDK persistence before upload.
            Proxy.generation_guard = guard
            Proxy.generation_blocked.set()
        if self.path.startswith("/_matrix/client/v3/keys/query") and Proxy.committed:
            self.send_response(503)
            self.end_headers()
            return
        connection = http.client.HTTPConnection(HS.removeprefix("http://"), timeout=15)
        headers = {k: v for k, v in self.headers.items() if k.lower() not in ("host", "connection", "content-length")}
        connection.request(self.command, self.path, body, headers)
        response = connection.getresponse()
        data = response.read()
        if self.path.endswith("/account_data/m.secret_storage.default_key") and self.command == "PUT" and response.status == 200 and Proxy.lose_recovery_response:
            Proxy.recovery_committed = True
            self.send_response(503)
            self.end_headers()
            connection.close()
            return
        if "/keys/device_signing/upload" in self.path and response.status == 200 and body and "auth" in json.loads(body) and Proxy.fail_after_commit:
            Proxy.committed = True
        self.send_response(response.status)
        self.send_header("Content-Type", response.getheader("Content-Type", "application/json"))
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        try:
            self.wfile.write(data)
        except BrokenPipeError:
            pass  # A killed SDK client is the intentional crash control.
        connection.close()

    do_GET = do_POST = do_PUT = do_DELETE = forward


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
threading.Thread(target=server.serve_forever, daemon=True).start()
PROXIED = "http://127.0.0.1:" + str(server.server_port)


def config(name, hs=PROXIED):
    base = WORK / name
    base.mkdir(mode=0o700)
    cfg = base / "config.yaml"
    cfg.write_text(json.dumps({"state_dir": str(base / "state"), "unlock_password": "unused-maintenance-no-eidetica", "logins": [{"agent": "fixture", "type": "matrix", "homeserver_url": hs, "username": USER, "password": "${KEY_TEST_PASSWORD}"}]}))
    cfg.chmod(0o600)
    return cfg, base / "state/matrix/_manager_keys_test"


CFG, ROOT = config("original")


def cli(*args, cfg=CFG, text="", ok=True, env=None):
    result = subprocess.run([BIN, "--config", str(cfg), "keys", *args], input=text, text=True, capture_output=True, timeout=100, env=env)
    output = result.stdout + result.stderr
    log = WORK / ("cli-%03d.log" % len(LOGS))
    log.write_text(output)
    LOGS.append(log)
    assert (result.returncode == 0) == ok, f"CLI {args} unexpected exit {result.returncode}: {output}"
    return output


def probe(root, op, *args):
    result = subprocess.run([PROBE, str(root), op, *map(str, args)], text=True, capture_output=True, timeout=100)
    log = WORK / ("probe-%03d.log" % len(LOGS))
    log.write_text(result.stdout + result.stderr)
    LOGS.append(log)
    assert result.returncode == 0, result.stderr
    return result.stdout.strip()


def inspect(root=ROOT):
    return json.loads(probe(root, "inspect"))


def session(root=ROOT):
    return json.loads((root / "session").read_text())


def fingerprint():
    s = session()
    return s["client_session"]["device_ed25519_key"]


def remote():
    s = session()
    response = request("/_matrix/client/v3/keys/query", {"device_keys": {USER: []}}, s["user_session"]["access_token"])
    return next(iter(response["master_keys"][USER]["keys"].values()))


def unchanged(master, device):
    return inspect()["master"] == master and remote() == master and fingerprint() == device


cli("status")  # First call saves a genuinely new login; no identity yet.
check(inspect()["master"] is None, "fresh persistent login has no private identity")
cli("init", "--account", USER, text=f"INIT {USER}\nAUTHORIZE {USER}\n")
master = inspect()["master"]
device = fingerprint()
check(master is not None and unchanged(master, device), "init publishes matching persistent identity and device")
cli("status")
check(unchanged(master, device), "restart preserves identity and device fingerprints")
cli("init", "--account", USER, text=f"INIT {USER}\n", ok=False)
check(unchanged(master, device), "init refuses an existing published identity")
cli("reset", "--account", "@wrong:keys.test", text="RESET @wrong:keys.test\n", ok=False)
check(unchanged(master, device), "account-specific reset rejects a different account")
cli("reset", "--account", USER, text="cancel\n", ok=False)
check(unchanged(master, device) and not (ROOT / "pending-reset.json").exists(), "initial reset confirmation cancellation does not stage or change keys")
cli("reset", "--account", USER, text=f"RESET {USER}\ncancel\n", ok=False)
check(unchanged(master, device) and (ROOT / "pending-reset.json").exists(), "real UIAA cancellation preserves active persisted identity")
cli("abort")
check(unchanged(master, device), "cancelled transaction safely archives pending state, retains both stores")
wrong_env = dict(os.environ, KEY_TEST_PASSWORD="deliberately-wrong-disposable-password")
cli("reset", "--account", USER, text=f"RESET {USER}\nAUTHORIZE {USER}\n", env=wrong_env, ok=False)
check(unchanged(master, device) and json.loads((ROOT / "pending-reset.json").read_text())["phase"] == "Rejected", "real bad-password UIAA rejection preserves active keys")
cli("abort")

# Interrupt the OTHER crash window: a real SQLite write lock prevents SDK
# private-key persistence, rather than changing an application phase flag.
Proxy.block_generation = True
process = subprocess.Popen([BIN, "--config", str(CFG), "keys", "reset", "--account", USER], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
process.stdin.write(f"RESET {USER}\n")
process.stdin.flush()
assert Proxy.generation_blocked.wait(timeout=15), "staged SQLite writer not reached"
until = time.monotonic() + 10
while not (ROOT / "pending-reset.json").exists() or json.loads((ROOT / "pending-reset.json").read_text())["phase"] != "Uncertain":
    assert process.poll() is None and time.monotonic() < until, "pending pre-generation journal not reached"
    time.sleep(0.01)
process.kill()
output, _ = process.communicate(timeout=10)
(WORK / "killed-before-generation.log").write_text(output)
LOGS.append(WORK / "killed-before-generation.log")
Proxy.generation_guard.rollback()
Proxy.generation_guard.close()
Proxy.generation_guard = None
Proxy.block_generation = False
check(unchanged(master, device), "death with real staged SQLite writer blocked preserves active identity")
cli("resume", ok=False)
cli("abort")
check(unchanged(master, device) and not (ROOT / "pending-reset.json").exists(), "recorded unchanged private baseline safely repairs interruption before generation")

# Kill an actual reset after replacement keys are saved and server asks UIAA.
process = subprocess.Popen([BIN, "--config", str(CFG), "keys", "reset", "--account", USER], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, bufsize=1)
process.stdin.write(f"RESET {USER}\n")
process.stdin.flush()
lines = []
for line in process.stdout:
    lines.append(line)
    if "Type AUTHORIZE" in line:
        break
else:
    raise AssertionError("did not reach real reset UIAA")
cli("status", ok=False)  # The resetting process owns the SAME store.
check(unchanged(master, device), "exclusive CLI ownership rejects second maintenance writer")
process.kill()
process.wait(timeout=10)
(WORK / "killed-reset.log").write_text("".join(lines))
LOGS.append(WORK / "killed-reset.log")
pending = json.loads((ROOT / "pending-reset.json").read_text())
# Probe reads full-session metadata, so publish a TEST fixture pointing at the
# pending staged generation, without changing the application's active pointer.
staged_view = WORK / "staged-inspection"
staged_view.mkdir(mode=0o700)
(staged_view / "session").write_text(json.dumps(pending["staged"]))
(staged_view / "session").chmod(0o600)
candidate = inspect(staged_view)["master"]
check(candidate != master and unchanged(master, device), "process death retains real replacement secrets without changing active keys")
cli("resume", ok=False)
cli("abort", ok=False)
check(unchanged(master, device), "ambiguous outcome cannot be silently discarded or treated as rollback")
cli("resume", "--authorize", text=f"AUTHORIZE {USER}\n")
check(inspect()["master"] == candidate and remote() == candidate and fingerprint() == device, "authorized resume commits SAME staged identity after crash")
master = candidate

Proxy.fail_after_commit = True
cli("reset", "--account", USER, text=f"RESET {USER}\nAUTHORIZE {USER}\n", ok=False)
check(Proxy.committed and remote() != master and inspect()["master"] == master and fingerprint() == device, "server-committed but lost query response leaves old active keys and pending replacement")
Proxy.fail_after_commit = Proxy.committed = False
cli("resume")
master = inspect()["master"]
check(remote() == master and fingerprint() == device and not (ROOT / "pending-reset.json").exists(), "fresh process resolves committed server fingerprint and atomically commits staged store")
cli("status")
check(unchanged(master, device), "committed replacement survives another process restart")

identity_keys = inspect()["identity_keys"]
check(all(identity_keys), "replacement has all three real private cross-signing fingerprints")
fixture = json.loads(probe(ROOT, "fixture"))
control = WORK / "fixture.json"
control.write_text(json.dumps(fixture))
control.chmod(0o600)
interrupted_key = WORK / "interrupted-recovery.key"
Proxy.lose_recovery_response = True
cli("recovery-setup", "--output", str(interrupted_key), ok=False)
check(Proxy.recovery_committed and interrupted_key.stat().st_mode & 0o777 == 0o600, "lost recovery-setup response retains a durable standard recovery secret")
Proxy.lose_recovery_response = False
cli("recovery-restore", "--key-file", str(interrupted_key))
check(inspect()["master"] == master and inspect()["keys"] >= 1, "interrupted recovery setup repaired using saved passphrase without reset")
key = WORK / "recovery.key"
previous_backup = request("/_matrix/client/v3/room_keys/version", token=session()["user_session"]["access_token"])
cli("recovery-setup", "--output", str(key), "--replace-existing", text=f"REPLACE RECOVERY {USER}\n")
retained_backup = request("/_matrix/client/v3/room_keys/version", token=session()["user_session"]["access_token"])
check(retained_backup["version"] == previous_backup["version"] and retained_backup["count"] >= 1, "recovery reconfiguration retains the SAME backup version and its post-cutover keys")
check(key.stat().st_mode & 0o777 == 0o600 and len(key.read_text()) == 64, "recovery secret durably saved only to explicit private file")
secret = key.read_text()
cli("recovery-setup", "--output", str(key), "--replace-existing", text=f"REPLACE RECOVERY {USER}\n", ok=False)
check(key.read_text() == secret, "recovery output cannot overwrite existing keys")
link = WORK / "recovery-link"
link.symlink_to(key)
cli("recovery-setup", "--output", str(link), "--replace-existing", text=f"REPLACE RECOVERY {USER}\n", ok=False)
check(key.read_text() == secret and link.is_symlink(), "recovery output rejects symlinks before destructive replacement")
public_dir = WORK / "public"
public_dir.mkdir(mode=0o755)
public_dir.chmod(0o755)
cli("recovery-setup", "--output", str(public_dir / "key"), "--replace-existing", text=f"REPLACE RECOVERY {USER}\n", ok=False)
check(not (public_dir / "key").exists(), "public recovery destination rejected")

FRESH_CFG, FRESH = config("fresh")
check(not FRESH.exists(), "recovery starts from nonexistent isolated store")
cli("status", cfg=FRESH_CFG)
check(inspect(FRESH)["keys"] == 0 and inspect(FRESH)["master"] is None, "fresh device has neither cached identity nor inbound room keys")
bad_key = WORK / "wrong.key"
bad_key.write_text(secrets.token_hex(32))
bad_key.chmod(0o600)
cli("recovery-restore", "--key-file", str(bad_key), cfg=FRESH_CFG, ok=False)
check(inspect(FRESH)["master"] is None and inspect(FRESH)["keys"] == 0, "wrong recovery secret restores no identity or keys")
cli("recovery-restore", "--key-file", str(key), cfg=FRESH_CFG)
fresh = inspect(FRESH)
check(fresh["master"] == master and fresh["identity_keys"] == identity_keys and fresh["keys"] >= 1 and fresh["imported"] >= 1, "fresh-store CLI restore recovers SAME NEW identity and real standard backup keys")
probe(FRESH, "diagnose", control)
check(True, "test-only Untrusted diagnostic decrypts exact fixture; no authenticated history acceptance")
fresh_device = session(FRESH)["client_session"]["device_ed25519_key"]
cli("status", cfg=FRESH_CFG)
check(inspect(FRESH)["master"] == master and session(FRESH)["client_session"]["device_ed25519_key"] == fresh_device, "fresh restored store survives restart with stable fingerprints")
cli("verify", "--user", "@other:keys.test", "--device", "NOPE", ok=False)
check(True, "other-user verification rejected before SAS")


def sas_case(name, reply, peer_mode="success", expect=True, no_peer=False):
    cfg, root = config(name)
    cli("status", cfg=cfg)
    target = session(root)["user_session"]["device_id"]
    peer_file = WORK / (name + "-peer.log")
    peer = None
    with peer_file.open("w") as out:
        if not no_peer:
            peer = subprocess.Popen([PROBE, str(root), "sas", peer_mode], stdout=out, stderr=subprocess.STDOUT, text=True)
            until = time.monotonic() + 15
            while "PEER READY" not in peer_file.read_text():
                assert peer.poll() is None and time.monotonic() < until, peer_file.read_text()
                time.sleep(0.1)
        process = subprocess.Popen([BIN, "--config", str(CFG), "keys", "verify", "--user", USER, "--device", target, "--timeout", "3" if no_peer else ("5" if reply is None else "30")], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, bufsize=1)
        lines, flow, decimals = [], None, None
        for line in process.stdout:
            lines.append(line)
            if line.startswith("SAS transaction: "):
                flow = line.strip().split(": ", 1)[1]
            if "Compare SAS decimals" in line:
                decimals = line.strip().split(": ", 1)[1]
            if line.startswith("Type MATCH "):
                if reply == "EOF":
                    process.stdin.close()
                elif reply is not None:
                    answer = f"MATCH {USER} {target} {flow}" if reply == "match" else reply
                    process.stdin.write(answer + "\n")
                    process.stdin.flush()
        process.wait(timeout=40)
        logfile = WORK / (name + "-cli.log")
        logfile.write_text("".join(lines))
        if peer:
            peer.wait(timeout=40)
        LOGS.extend([logfile, peer_file])
        assert (process.returncode == 0) == expect, logfile.read_text()
        if expect:
            assert "PEER SAS " + decimals in peer_file.read_text(), "SAS values differ across actual devices"
            assert "PASS peer completed real SAS" in peer_file.read_text()
    if not expect:
        status = cli("status")
        assert f"Device {target} cross-signed: false" in status, "negative SAS control signed the selected device"
    check(True, name + ": real SAS " + ("success with equal values and explicit bound match" if expect else "negative control does not cross-sign device"))


sas_case("sas-success", "match")
sas_case("sas-operator-mismatch", "mismatch", expect=False)
sas_case("sas-operator-cancel", "cancel", expect=False)
sas_case("sas-unbound-match", "MATCH wrong-account wrong-device wrong-transaction", expect=False)
sas_case("sas-peer-reject", "cancel", peer_mode="reject", expect=False)
sas_case("sas-peer-cancel", "cancel", peer_mode="cancel", expect=False)
sas_case("sas-timeout", "cancel", no_peer=True, expect=False)
sas_case("sas-dialog-timeout", None, expect=False)
sas_case("sas-eof", "EOF", expect=False)

MISSING_CFG, MISSING_ROOT = config("missing-store")
cli("status", cfg=MISSING_CFG)
missing_saved = session(MISSING_ROOT)
missing_crypto = Path(missing_saved["client_session"]["db_path"]) / "matrix-sdk-crypto.sqlite3"
retained_crypto = WORK / "retained-missing-store.sqlite3"
missing_crypto.rename(retained_crypto)
cli("status", cfg=MISSING_CFG, ok=False)
check(not missing_crypto.exists() and session(MISSING_ROOT) == missing_saved, "missing recorded crypto database is refused before creating replacement keys")
retained_crypto.rename(missing_crypto)
check("Account:" in cli("status", cfg=MISSING_CFG), "restoring the original crypto database repairs status without device replacement")

TILDE_CFG, _ = config("tilde-state")
fake_home = WORK / "tilde-state/home"
fake_home.mkdir(mode=0o700)
tilde_config = json.loads(TILDE_CFG.read_text())
tilde_config["state_dir"] = "~/state"
TILDE_CFG.write_text(json.dumps(tilde_config))
tilde_env = dict(os.environ, HOME=str(fake_home))
cli("status", cfg=TILDE_CFG, env=tilde_env)
TILDE_ROOT = fake_home / "state/matrix/_manager_keys_test"
tilde_device = session(TILDE_ROOT)["user_session"]["device_id"]
tilde_status = cli("status", cfg=TILDE_CFG, env=tilde_env)
check("Account:" in tilde_status and "New Matrix device" not in tilde_status and session(TILDE_ROOT)["user_session"]["device_id"] == tilde_device, "maintenance resolves tilde state paths exactly like the bridge, without provisioning another login")

KEYLESS_CFG, KEYLESS = config("keyless-backup")
cli("status", cfg=KEYLESS_CFG)
cli("reset", "--account", USER, cfg=KEYLESS_CFG, text=f"RESET {USER}\nAUTHORIZE {USER}\n")
keyless_output = WORK / "keyless-recovery.key"
cli("recovery-setup", "--output", str(keyless_output), "--replace-existing", cfg=KEYLESS_CFG, text=f"REPLACE RECOVERY {USER}\n", ok=False)
protected_backup = request("/_matrix/client/v3/room_keys/version", token=session(KEYLESS)["user_session"]["access_token"])
check(not keyless_output.exists() and protected_backup["version"] == retained_backup["version"] and protected_backup["count"] >= 1, "unavailable backup key refuses reconfiguration without deleting server-only keys")

# All secret-bearing fixture files are private and all process logs are scanned
# against actual passwords, recovery secrets, tokens and store passphrases.
secret_values = [PASSWORD, secret, interrupted_key.read_text(), bad_key.read_text(), wrong_env["KEY_TEST_PASSWORD"]]
for file in WORK.glob("**/session"):
    s = json.loads(file.read_text())
    secret_values += [s["client_session"]["passphrase"], s["user_session"]["access_token"]]
    assert file.stat().st_mode & 0o777 == 0o600
for file in LOGS + [WORK / "synapse.log", WORK / "homeserver.log"]:
    text = file.read_text()
    assert all(value not in text for value in secret_values if value), "secret in process log: " + str(file)
check(True, "all actual fixture secrets absent from CLI/probe/server logs; session files mode0600")
server.shutdown()
print(f"Key management acceptance summary: {len(CHECKS)} passed; 0 failed", flush=True)
