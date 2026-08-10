#!/usr/bin/env python3
"""A minimal OpenAI-compatible endpoint that answers with a fixed string.

The Matrix end-to-end test exercises transport: a message enters over Matrix,
crosses into a session database, syncs to the agent peer, and the reply travels
back the same way. A real model would add an API key, a network dependency, a
per-run cost, and non-determinism to a test that asserts on none of that.

So the reply is canned, and the assertion is that this exact string comes back
out of the room. Anything that reaches Matrix proves the whole path carried it.

To cover the ReAct loop, the stub branches on the request body:

- The request carries a ``role: tool`` result → returns the fixed reply text.
- A user message asks for a tool call → returns a ``tool_calls`` response,
  simulating the model deciding to call ``compact``.
- Everything else → returns the fixed reply.

Every request logs the user messages it was given, and the tool-result branch
logs "detected tool result". Between them the harness can assert that the ReAct
cycle completed, and that a message it expected to be dropped never became a
turn.

Usage: stub_llm.py <port> <reply-text>
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1])
REPLY = sys.argv[2]

# The phrase that asks for a tool call. Branching on the request content rather
# than on a request counter keeps the tool call attached to the turn that asked
# for it: a counter hands it to whichever turn arrives first, which is the
# cold-boot turn, and leaves the ReAct case asserting on a log line written
# minutes earlier.
#
# The phrase stays in the room's history once sent, so every later turn in that
# same room repeats the cycle. Send it from a room whose remaining turns are
# meant to exercise tool calls too — in the harness that means last.
TOOL_CALL_TRIGGER = "react test"


class Handler(BaseHTTPRequestHandler):
    def _send(self, payload, status=200):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    @staticmethod
    def _has_tool_result(messages):
        """Return True if *messages* includes a ``role: tool`` entry."""
        for msg in messages:
            if isinstance(msg, dict) and msg.get("role") == "tool":
                return True
        return False

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

        has_tool_result = self._has_tool_result(messages)
        user_text = self._user_text(messages)

        # One line per request, carrying what the turn was given. Every reply
        # this stub sends is the same string, so the room cannot show which
        # turn produced which reply — a case that needs to know asserts here.
        sys.stderr.write("stub_llm: request: " + user_text + "\n")

        if has_tool_result:
            sys.stderr.write(
                "stub_llm: detected tool result in request, returning final reply\n"
            )
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
        elif TOOL_CALL_TRIGGER in user_text:
            sys.stderr.write("stub_llm: returning a tool call\n")
            self._send(
                {
                    "id": "chatcmpl-e2e",
                    "object": "chat.completion",
                    "created": 0,
                    "model": "stub",
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": None,
                                "tool_calls": [
                                    {
                                        "id": "call_stub_compact_1",
                                        "type": "function",
                                        "function": {
                                            "name": "compact",
                                            "arguments": '{"summary":"stub tool call"}',
                                        },
                                    }
                                ],
                            },
                            "finish_reason": "tool_calls",
                        }
                    ],
                    "usage": {
                        "prompt_tokens": 1,
                        "completion_tokens": 1,
                        "total_tokens": 2,
                    },
                }
            )
        else:
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
