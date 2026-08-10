#!/usr/bin/env python3
"""A minimal OpenAI-compatible endpoint that answers with a fixed string.

The Matrix end-to-end test exercises transport: a message enters over Matrix,
crosses into a session database, syncs to the agent peer, and the reply travels
back the same way. A real model would add an API key, a network dependency, a
per-run cost, and non-determinism to a test that asserts on none of that.

So the reply is canned, and the assertion is that this exact string comes back
out of the room. Anything that reaches Matrix proves the whole path carried it.

Every request logs the user messages it was given, so a case can assert that a
message it expected to be dropped never became a turn.

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

    @staticmethod
    def _user_text(messages):
        """Return every ``role: user`` entry's text, joined on one line."""
        parts = []
        for msg in messages:
            if not isinstance(msg, dict) or msg.get("role") != "user":
                continue
            content = msg.get("content")
            if isinstance(content, str):
                parts.append(content)
            elif isinstance(content, list):
                parts.extend(
                    part.get("text", "") for part in content if isinstance(part, dict)
                )
        return " | ".join(parts).replace("\n", " ")

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
        body = self.rfile.read(length)

        messages = []
        try:
            messages = json.loads(body).get("messages", [])
        except (json.JSONDecodeError, TypeError, AttributeError):
            pass

        # One line per request, carrying what the turn was given. Every reply
        # this stub sends is the same string, so the room cannot show which
        # turn produced which reply — a case that needs to know asserts here.
        sys.stderr.write("stub_llm: request: " + self._user_text(messages) + "\n")

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
