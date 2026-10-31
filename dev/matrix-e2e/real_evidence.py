#!/usr/bin/env python3
# /// script
# dependencies = ["cbor2"]
# ///
"""Correlate provider IDs with durable turns and actual disposable room events."""

import argparse
import json
import sqlite3
from pathlib import Path

import cbor2


def inspect(root, marker, final=False):
    trace = [
        json.loads(line) for line in (root / "model.jsonl").read_text().splitlines()
    ]
    assert not any(e["type"] == "halt" for e in trace), "relay halted"
    results = [e for e in trace if e["type"] == "result"]
    expected = 2 if final else 1
    assert len(results) == expected * 2, "expected send and terminal response per turn"
    assert len([e for e in trace if e["type"] == "reserve"]) == len(results)
    room = json.loads((root / "room.json").read_text())["chunk"]
    sent = [
        e["content"]["body"]
        for e in room
        if e.get("type") == "m.room.message" and e["sender"] == "@agent:e2e.test"
    ]
    assert sorted(sent) == sorted(
        [marker + "-SEND"] + ([marker + "-BARRIER"] if final else [])
    ), sent
    with sqlite3.connect(
        f"file:{root / 'state-daemon/eidetica.db'}?mode=ro", uri=True
    ) as db:
        rows = db.execute(
            "select r.record_value from store_state_namespaces n "
            "join store_state_records r on n.namespace_id=r.namespace_id "
            "where n.store_name='turn_attempts'"
        ).fetchall()
        # SQLite state projections may lag a completed turn. Its signed append-only
        # entries still contain the actual transcript transaction; inspect those.
        records = []
        for (blob,) in db.execute("select entry_cbor from entries"):
            if b"ModelResponse" not in blob and b"ToolResult" not in blob:
                continue
            for subtree in cbor2.loads(blob).get("subtrees", []):
                data = subtree.get("data")
                if not isinstance(data, bytes):
                    continue
                for child in json.loads(data).get("children", {}).values():
                    if "Text" in child:
                        record = json.loads(child["Text"])
                        if (
                            isinstance(record, dict)
                            and "message" in record
                            and "attempt_id" in record
                        ):
                            records.append(record)
    attempts = {}
    responses = {}
    tools = {}
    terminals = {}
    for (raw,) in rows:
        obj = json.loads(raw)
        attempts.setdefault(obj["attempt_id"], set()).add(obj["status"])
    for obj in records:
        message = obj["message"]
        if not isinstance(message, dict):
            continue
        response = message.get("ModelResponse")
        if response:
            response_id = (response.get("metadata") or {}).get("response_id")
            responses[response_id] = response
            if response["terminal"]:
                terminals[obj["attempt_id"]] = response
        tool = message.get("ToolResult")
        if tool:
            tools[tool["call_id"]] = tool
    assert len(attempts) == len(terminals) == expected
    assert all("Completed" in statuses for statuses in attempts.values())
    assert {r["response_id"] for r in results} == set(responses), (
        "provider/durable IDs differ"
    )
    for result in results:
        durable = responses[result["response_id"]]
        assert durable.get("content") == result.get("content")
        provider_calls = [
            (c["id"], c["function"]["name"], json.loads(c["function"]["arguments"]))
            for c in result["tool_calls"]
        ]
        durable_calls = [
            (c["id"], c["name"], json.loads(c["arguments"]))
            for c in durable.get("tool_calls", [])
        ]
        assert durable_calls == provider_calls, "provider/durable tool calls differ"
    expected_finals = [marker + "-LOCAL"] + ([marker + "-DONE"] if final else [])
    assert sorted(r["content"].strip() for r in terminals.values()) == sorted(
        expected_finals
    )
    calls = [c for r in results for c in r["tool_calls"]]
    assert len(calls) == len(tools) == expected
    for call in calls:
        assert call["function"]["name"] == "matrix__send"
        assert tools[call["id"]]["name"] == "matrix__send"
        assert tools[call["id"]]["outcome"] == "Success"
    return {
        "completed_turns": len(terminals),
        "matrix_messages": sent,
        "response_ids": sorted(responses),
        "local_finals": expected_finals,
        "actual_usd": sum(r["usage"]["cost"] for r in results),
        "reserved_usd": sum(r["worst_usd"] for r in trace if r["type"] == "reserve"),
        "calls": len(results),
        "results": results,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("workspace", type=Path)
    parser.add_argument("marker")
    parser.add_argument("--final", action="store_true")
    args = parser.parse_args()
    print(json.dumps(inspect(args.workspace, args.marker, args.final), indent=2))


if __name__ == "__main__":
    main()
