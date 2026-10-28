#!/usr/bin/env python3
"""Single-run, fail-closed completion relay for the opt-in smoke test.

Only a loopback, already metered endpoint is accepted. No vendor credentials.
Six requests, 64 KiB input, 512 output tokens, 120s/request, USD 0.10/run.
The supplied rates must be conservative USD/million-token ceilings.
"""

import argparse
import json
import math
import os
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from urllib.parse import urlparse

MAX_BYTES = 65536
MAX_OUTPUT = 512
MAX_CALLS = 6
MAX_USD = 0.10


class Budget:
    def __init__(self, trace, model, input_rate, output_rate):
        if not all(math.isfinite(x) and x > 0 for x in (input_rate, output_rate)):
            raise ValueError("positive finite rate ceilings required")
        self.reserve = (MAX_BYTES * input_rate + MAX_OUTPUT * output_rate) / 1e6
        if self.reserve * MAX_CALLS > MAX_USD:
            raise ValueError("worst-case six-call reservation exceeds USD 0.10")
        self.model = model
        self.trace = Path(trace)
        # Never erase or reset an uncertain request ledger.
        self.file = self.trace.open("x")
        self.count = 0
        self.halted = False

    def record(self, event):
        self.file.write(json.dumps(event) + "\n")
        self.file.flush()
        os.fsync(self.file.fileno())

    def prepare(self, raw):
        if self.halted or self.count >= MAX_CALLS or len(raw) > MAX_BYTES:
            raise ValueError("request ceiling reached or relay halted")
        obj = json.loads(raw)
        if obj.get("model") != self.model or obj.get("stream") is True:
            raise ValueError("wrong model or streaming")
        obj.pop("max_completion_tokens", None)
        obj.update(
            max_tokens=MAX_OUTPUT,
            stream=False,
            reasoning={"enabled": False},
            usage={"include": True},
        )
        payload = json.dumps(obj).encode()
        if len(payload) > MAX_BYTES:
            raise ValueError("forwarded request too large")
        self.count += 1
        self.record(
            {
                "type": "reserve",
                "sequence": self.count,
                "at": time.time(),
                "model": self.model,
                "bytes": len(payload),
                "worst_usd": self.reserve,
                "max_tokens": MAX_OUTPUT,
            }
        )
        return payload

    def result(self, obj):
        usage = obj["usage"]
        if not all(
            type(usage.get(k)) is int and usage[k] >= 0
            for k in ("prompt_tokens", "completion_tokens")
        ):
            raise ValueError("missing token usage")
        if usage["completion_tokens"] > MAX_OUTPUT:
            raise ValueError("output cap violated")
        cost = usage.get("cost")
        if (
            not isinstance(cost, (int, float))
            or not math.isfinite(cost)
            or cost < 0
            or cost > self.reserve
        ):
            raise ValueError("missing cost or rate ceiling violated")
        choice = obj["choices"][0]
        message = choice["message"]
        if not message.get("content") and not message.get("tool_calls"):
            raise ValueError("empty model response")
        self.record(
            {
                "type": "result",
                "response_id": obj["id"],
                "usage": usage,
                "finish_reason": choice["finish_reason"],
                "content": message.get("content"),
                "tool_calls": message.get("tool_calls", []),
            }
        )


class Handler(BaseHTTPRequestHandler):
    def reply(self, status, obj):
        wire = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(wire)))
        self.end_headers()
        self.wfile.write(wire)

    def do_GET(self):
        self.reply(200, {"data": [{"id": self.server.budget.model}]})

    def do_POST(self):
        budget = self.server.budget
        try:
            if self.path != "/v1/chat/completions":
                raise ValueError("unsupported path")
            length = int(self.headers.get("Content-Length", "-1"))
            if not 0 < length <= MAX_BYTES:
                raise ValueError("invalid input length")
            self.connection.settimeout(10)
            raw = self.rfile.read(length)
            if len(raw) != length:
                raise ValueError("incomplete input")
            payload = budget.prepare(raw)
        except (ValueError, TimeoutError) as exc:
            return self.reply(422, {"error": str(exc)})
        try:
            request = urllib.request.Request(
                self.server.upstream,
                payload,
                {"Content-Type": "application/json", "Authorization": "Bearer chaz"},
            )
            with urllib.request.urlopen(request, timeout=120) as response:
                wire = response.read(262145)
            if len(wire) > 262144:
                raise ValueError("oversized response")
            obj = json.loads(wire)
            budget.result(obj)
            self.reply(200, obj)
        except Exception as exc:
            budget.halted = True
            budget.record({"type": "halt", "reason": type(exc).__name__})
            self.reply(502, {"error": "upstream failed; relay halted"})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--trace", required=True)
    args = parser.parse_args()
    upstream = os.environ["MATRIX_E2E_MODEL_ENDPOINT"]
    url = urlparse(upstream)
    if (
        url.scheme != "http"
        or url.hostname not in ("127.0.0.1", "localhost")
        or url.username
        or url.password
    ):
        raise ValueError(
            "only a loopback metered endpoint without credentials is allowed"
        )
    budget = Budget(
        args.trace,
        os.environ["MATRIX_E2E_MODEL"],
        float(os.environ["MATRIX_E2E_INPUT_RATE"]),
        float(os.environ["MATRIX_E2E_OUTPUT_RATE"]),
    )
    server = HTTPServer(("127.0.0.1", args.port), Handler)
    server.budget, server.upstream = budget, upstream
    server.serve_forever()


if __name__ == "__main__":
    main()
