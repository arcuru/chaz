#!/usr/bin/env python3
"""A minimal OpenAI-compatible endpoint that answers with a fixed string.

The Matrix end-to-end test exercises transport: a message enters over Matrix,
crosses into a session database, syncs to the agent peer, and the reply travels
back the same way. A real model would add an API key, a network dependency, a
per-run cost, and non-determinism to a test that asserts on none of that.

So the reply is canned, and the assertion is that this exact string comes back
out of the room. Anything that reaches Matrix proves the whole path carried it.

Usage: stub_llm.py <port> <reply-text>
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1])
REPLY = sys.argv[2]


class Handler(BaseHTTPRequestHandler):
    def _send(self, payload, status=200):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        # Some clients probe the model list before their first completion.
        if self.path.rstrip("/").endswith("/models"):
            self._send(
                {
                    "object": "list",
                    "data": [{"id": "stub", "object": "model", "owned_by": "e2e"}],
                }
            )
        else:
            self._send({"error": "not found"}, status=404)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        self.rfile.read(length)
        self._send(
            {
                "id": "chatcmpl-e2e",
                "object": "chat.completion",
                "created": 0,
                "model": "stub",
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": REPLY},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {
                    "prompt_tokens": 1,
                    "completion_tokens": 1,
                    "total_tokens": 2,
                },
            }
        )

    def log_message(self, fmt, *args):
        # Server logs go to the harness's log file, not the test's stderr.
        sys.stderr.write("stub_llm: " + (fmt % args) + "\n")


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
