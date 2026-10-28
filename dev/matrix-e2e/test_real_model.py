#!/usr/bin/env python3
"""Offline budget and evidence checks: no model endpoint is ever contacted."""

import json
import sqlite3
import subprocess
import tempfile
import unittest
from pathlib import Path

import cbor2
from real_evidence import inspect
from real_model import MAX_BYTES, MAX_CALLS, MAX_OUTPUT, Budget


class RelayTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.trace = Path(self.temp.name) / "trace.jsonl"
        self.budget = Budget(self.trace, "test", 0.15, 0.60)
        self.addCleanup(self.budget.file.close)
        self.raw = json.dumps(
            {"model": "test", "messages": [], "max_tokens": 10000}
        ).encode()

    def test_cap_and_seventh_request(self):
        for _ in range(MAX_CALLS):
            payload = json.loads(self.budget.prepare(self.raw))
            self.assertEqual(payload["max_tokens"], MAX_OUTPUT)
        with self.assertRaises(ValueError):
            self.budget.prepare(self.raw)
        self.assertEqual(len(self.trace.read_text().splitlines()), MAX_CALLS)

    def test_wrong_model_streaming_and_size(self):
        for raw in (
            b'{"model":"wrong"}',
            b'{"model":"test","stream":true}',
            b"x" * (MAX_BYTES + 1),
        ):
            with self.assertRaises(ValueError):
                self.budget.prepare(raw)
        self.assertEqual(self.budget.count, 0)

    def test_bad_rates_and_spend(self):
        for rate in (0, -1, float("nan"), float("inf"), 100):
            with self.assertRaises(ValueError):
                Budget(Path(self.temp.name) / "unused", "test", rate, 0.60)

    def test_existing_ledger_refused(self):
        with self.assertRaises(FileExistsError):
            Budget(self.trace, "test", 0.15, 0.60)

    def test_result_requires_content_usage_cost_and_cap(self):
        valid = {
            "id": "response",
            "usage": {"prompt_tokens": 10, "completion_tokens": 10, "cost": 0.00001},
            "choices": [{"finish_reason": "stop", "message": {"content": "yes"}}],
        }
        self.budget.result(valid)
        for usage in (
            {},
            {"prompt_tokens": 1, "completion_tokens": 513, "cost": 0},
            {"prompt_tokens": 1, "completion_tokens": 1},
            {"prompt_tokens": 1, "completion_tokens": 1, "cost": 1},
        ):
            with self.assertRaises(ValueError):
                self.budget.result(dict(valid, usage=usage))
        with self.assertRaises(ValueError):
            self.budget.result(dict(valid, choices=[{"message": {"content": None}}]))

    def test_halt_refuses_requests(self):
        self.budget.halted = True
        with self.assertRaises(ValueError):
            self.budget.prepare(self.raw)


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "state-daemon").mkdir()
        self.db = sqlite3.connect(self.root / "state-daemon/eidetica.db")
        self.addCleanup(self.db.close)
        self.db.executescript(
            "create table store_state_namespaces(namespace_id,store_name);"
            "create table store_state_records(namespace_id,record_value);"
            "create table entries(entry_cbor);"
        )
        self.db.executemany(
            "insert into store_state_namespaces values(?,?)",
            [(1, "turn_attempts"), (2, "turn_transcript")],
        )
        self.row(1, {"attempt_id": "attempt", "status": "Completed"})
        self.row(
            2,
            {
                "attempt_id": "attempt",
                "message": {
                    "ModelResponse": {
                        "metadata": {"response_id": "send"},
                        "terminal": False,
                        "content": None,
                        "tool_calls": [
                            {"id": "call", "name": "matrix__send", "arguments": "{}"}
                        ],
                    }
                },
            },
        )
        self.row(
            2,
            {
                "attempt_id": "attempt",
                "message": {
                    "ModelResponse": {
                        "metadata": {"response_id": "final"},
                        "terminal": True,
                        "content": "marker-LOCAL",
                    }
                },
            },
        )
        self.row(
            2,
            {
                "attempt_id": "attempt",
                "message": {
                    "ToolResult": {
                        "call_id": "call",
                        "name": "matrix__send",
                        "outcome": "Success",
                    }
                },
            },
        )
        self.trace = [{"type": "reserve", "worst_usd": 0.01}] * 2 + [
            {
                "type": "result",
                "response_id": "send",
                "usage": {"cost": 0.001},
                "tool_calls": [
                    {
                        "id": "call",
                        "function": {"name": "matrix__send", "arguments": "{}"},
                    }
                ],
            },
            {
                "type": "result",
                "response_id": "final",
                "content": "marker-LOCAL",
                "usage": {"cost": 0.001},
                "tool_calls": [],
            },
        ]
        self.room = [
            {
                "type": "m.room.message",
                "sender": "@agent:e2e.test",
                "content": {"body": "marker-SEND"},
            }
        ]
        self.save()

    def row(self, namespace, obj):
        if namespace == 1:
            self.db.execute(
                "insert into store_state_records values(?,?)",
                (namespace, json.dumps(obj)),
            )
        else:
            data = json.dumps(
                {"children": {"record": {"Text": json.dumps(obj)}}}
            ).encode()
            self.db.execute(
                "insert into entries values(?)",
                (cbor2.dumps({"subtrees": [{"data": data}]}),),
            )
        self.db.commit()

    def save(self):
        (self.root / "model.jsonl").write_text(
            "\n".join(json.dumps(x) for x in self.trace) + "\n"
        )
        (self.root / "room.json").write_text(json.dumps({"chunk": self.room}))

    def test_positive_correlation(self):
        self.assertEqual(inspect(self.root, "marker")["completed_turns"], 1)

    def test_duplicate_or_missing_outbound_fails(self):
        for room in ([], self.room * 2):
            self.room = room
            self.save()
            with self.assertRaises(AssertionError):
                inspect(self.root, "marker")

    def test_provider_tool_arguments_mismatch_fails(self):
        self.trace[-2]["tool_calls"][0]["function"]["arguments"] = '{"body":"wrong"}'
        self.save()
        with self.assertRaises(AssertionError):
            inspect(self.root, "marker")

    def test_old_started_projection_does_not_hide_completion(self):
        self.row(1, {"attempt_id": "attempt", "status": "Started"})
        self.assertEqual(inspect(self.root, "marker")["completed_turns"], 1)

    def test_provider_id_mismatch_fails(self):
        self.trace[-1]["response_id"] = "other"
        self.save()
        with self.assertRaises(AssertionError):
            inspect(self.root, "marker")

    def test_incomplete_turn_fails(self):
        self.db.execute("delete from store_state_records where namespace_id=1")
        self.db.commit()
        with self.assertRaises(AssertionError):
            inspect(self.root, "marker")

    def test_halted_gate_fails(self):
        self.trace.append({"type": "halt"})
        self.save()
        with self.assertRaises(AssertionError):
            inspect(self.root, "marker")


class SmokeShellTests(unittest.TestCase):
    def test_barrier_requires_two_turn_evidence(self):
        with tempfile.TemporaryDirectory() as temp:
            script = Path(__file__).with_name("real-smoke.sh")
            shell = r"""set -euo pipefail
WORKSPACE="$1"; MATRIX_E2E_EVIDENCE="$1"; HERE=fixture
PUPPET_TOKEN=puppet; STRANGER_PASSWORD=password; STRANGER_MXID=@stranger:e2e.test
AGENT_MXID=@agent:e2e.test; ROOM_ID=room; MARKER=marker; REPLY_TIMEOUT=10
mx() { printf '%s\n' '{"access_token":"token","event_id":"event","chunk":[]}'; }
uv() { printf '%s\n' "$*" >>"$WORKSPACE/checks"; printf '{}\n'; }
wait_for() { shift 2; "$@"; }
fail() { exit 1; }
source "$2"
"""
            result = subprocess.run(
                ["bash", "-c", shell, "smoke-test", temp, str(script)],
                capture_output=True,
                text=True,
                timeout=10,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            checks = (Path(temp) / "checks").read_text().splitlines()
            self.assertEqual(
                ["--final" in check for check in checks], [False, True, True]
            )


if __name__ == "__main__":
    unittest.main()
