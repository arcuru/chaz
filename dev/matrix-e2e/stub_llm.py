#!/usr/bin/env python3
"""A minimal OpenAI-compatible endpoint that explicitly requests Matrix sends.

The Matrix end-to-end test exercises transport: a message enters over Matrix,
crosses into a session database, syncs to the agent peer, and a model-called Matrix send travels back the same way. A real model would add an API key, a network dependency, a
per-run cost, and non-determinism to a test that asserts on none of that.

The tool call is canned, and the assertion is that only this explicit send
reaches the room; normal finals remain local.

To cover the ReAct loop, the stub branches on the request body:

- The request carries a ``role: tool`` result → returns the fixed reply text.
- A user message asks for a tool call → returns a ``tool_calls`` response,
  simulating the model deciding to call ``compact``.
- Most initial turns → call ``matrix__send`` with the fixed marker.
- A local-final test → return text only, with no Matrix post.

Every request logs the user messages it was given, and the tool-result branch
logs "detected tool result". Between them the harness can assert that the ReAct
cycle completed, and that a message it expected to be dropped never became a
turn.

Usage: stub_llm.py <port> <reply-text>
"""

import json
import re
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

# The phrase that asks for a tool call. Branching on the request content rather
# than on a request counter keeps the tool call attached to the turn that asked
# for it: a counter hands it to whichever turn arrives first, which is the
# cold-boot turn, and leaves the ReAct case asserting on a log line written
# minutes earlier.
#
# Only the latest actual user message triggers a tool call; otherwise an
# earlier ReAct prompt in the same room's context would trigger every turn.
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
        """Only a tool result after the latest real user input belongs to this turn."""
        latest = max((index for index, msg in enumerate(messages)
                      if isinstance(msg, dict) and msg.get("role") == "user"
                      and isinstance(msg.get("content"), str)
                      and not msg["content"].startswith("## Relevant Memories")), default=-1)
        return any(isinstance(msg, dict) and msg.get("role") == "tool"
                   for msg in messages[latest + 1:])

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

    @staticmethod
    def _react_requested(messages):
        for msg in reversed(messages):
            if not isinstance(msg, dict) or msg.get("role") != "user":
                continue
            content = msg.get("content")
            if not isinstance(content, str) or content.startswith("## Relevant Memories"):
                continue
            return TOOL_CALL_TRIGGER in content
        return False

    def _fixture_reply(self, *, content=None, name=None, arguments=None, call_id=None):
        message = {"role": "assistant", "content": content}
        if name:
            message["tool_calls"] = [{
                "id": call_id, "type": "function", "function": {
                    "name": name, "arguments": json.dumps(arguments),
                },
            }]
        self._send({
            "id": "chatcmpl-e2e", "object": "chat.completion", "created": 0,
            "model": "stub", "choices": [{"index": 0, "message": message,
                "finish_reason": "tool_calls" if name else "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        })

    def _schedule_fixture(self, messages, tools):
        # The scheduler prepends private System input. Historical user prompts
        # and tool results are not a new schedule invocation or ReAct result.
        task = messages[0].get("content") if messages and isinstance(messages[0], dict) \
            and messages[0].get("role") == "system" else None
        for mode in ("send", "silent"):
            if task != f"{REPLY}-scheduled-{mode}":
                continue
            call_id = f"scheduled-{mode}-send"
            current_result = messages[-1].get("role") == "tool" \
                and messages[-1].get("tool_call_id") == call_id
            if mode == "send" and not current_result:
                if "matrix__send" not in tools:
                    self._send({"error": "scheduled matrix__send unavailable"}, status=400)
                    return True
                sys.stderr.write("stub_llm: scheduled explicit matrix__send\n")
                self._fixture_reply(name="matrix__send", call_id=call_id,
                                    arguments={"body": f"{REPLY}-scheduled-post"})
            else:
                sys.stderr.write(f"stub_llm: scheduled {mode} local final\n")
                self._fixture_reply(content=f"{REPLY}-scheduled-{mode}-final")
            return True

        latest_user = next((msg.get("content", "") for msg in reversed(messages)
                            if isinstance(msg, dict) and msg.get("role") == "user"
                            and isinstance(msg.get("content"), str)
                            and not msg["content"].startswith("## Relevant Memories")), "")
        for mode in ("send", "silent"):
            if f"create pinned schedule {mode}" not in latest_user:
                continue
            call_id = f"create-schedule-{mode}"
            if messages[-1].get("role") == "tool" \
                    and messages[-1].get("tool_call_id") == call_id:
                if "Added schedule" not in str(messages[-1].get("content")):
                    self._send({"error": "schedule fixture creation failed"}, status=400)
                    return True
                self._fixture_reply(content=f"{REPLY}-schedule-{mode}-created")
            else:
                if "schedule_add" not in tools:
                    self._send({"error": "schedule_add unavailable"}, status=400)
                    return True
                self._fixture_reply(name="schedule_add", call_id=call_id, arguments={
                    "id": f"e2ee-{mode}", "interval_seconds": 5, "target": "pinned",
                    "max_fires": 1, "task": f"{REPLY}-scheduled-{mode}",
                })
            return True
        return False

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

        # Only chat completions are served. Answering every path would make any
        # other POST the daemon learns to send — an embedding request, say —
        # arrive as a chat completion and land in the request log, where the
        # harness counts model turns.
        if not self.path.rstrip("/").endswith("/chat/completions"):
            self._send({"error": "not found"}, status=404)
            return

        request = {}
        try:
            request = json.loads(body)
            messages = request.get("messages", [])
        except (json.JSONDecodeError, TypeError, AttributeError):
            request = {}
            messages = []

        # Match strict OpenAI-compatible providers, including DeepSeek: dotted
        # internal tool names must not appear as wire function names.
        tools = [tool.get("function", {}).get("name") for tool in request.get("tools", [])]
        if any(not isinstance(name, str) or not re.fullmatch(r"[a-zA-Z0-9_-]+", name)
               for name in tools):
            self._send({"error": "invalid wire tool name"}, status=400)
            return

        has_tool_result = self._has_tool_result(messages)
        user_text = self._user_text(messages)

        # One line per request, carrying what the turn was given. Every reply
        # this stub sends is the same string, so the room cannot show which
        # turn produced which reply — a case that needs to know asserts here.
        sys.stderr.write("stub_llm: request: " + user_text + "\n")
        if not has_tool_result:
            sys.stderr.write("stub_llm: turn: " + user_text + "\n")

        latest_user = next((msg.get("content") for msg in reversed(messages)
                            if isinstance(msg, dict) and msg.get("role") == "user"
                            and isinstance(msg.get("content"), str)
                            and not msg["content"].startswith("## Relevant Memories")), "")
        if self._schedule_fixture(messages, tools):
            return
        if "participation after restart" in latest_user:
            if not any('"kind":"matrix_reply"' in str(msg.get("content", ""))
                       for msg in messages if isinstance(msg, dict)):
                self._send({"error": "auto-posted final was not framed as a Matrix reply"}, status=400)
                return
        if "participation local-only test" in latest_user:
            system_text = " ".join(msg.get("content", "") for msg in messages
                                   if isinstance(msg, dict) and msg.get("role") == "system")
            if ("call no_reply({}) as the sole terminal action" not in system_text
                    or "a normal final is posted to that room" not in system_text):
                self._send({"error": "Matrix reply/silence guidance missing"}, status=400)
                return
            if "no_reply" not in tools or "matrix__send" not in tools:
                self._send({"error": "Matrix send or silence action unavailable"}, status=400)
                return
            sys.stderr.write("stub_llm: participation terminal no_reply\n")
            self._send({
                "id": "chatcmpl-e2e", "object": "chat.completion", "created": 0,
                "model": "stub", "choices": [{"index": 0, "message": {
                    "role": "assistant", "content": None, "tool_calls": [{
                        "id": "no-reply-e2e", "type": "function", "function": {
                            "name": "no_reply", "arguments": "{}",
                        },
                    }]}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            })
        elif "participation auto-final test" in latest_user:
            if "no_reply" not in tools:
                self._send({"error": "no_reply unavailable on Matrix-origin turn"}, status=400)
                return
            sys.stderr.write("stub_llm: participation ordinary final reply\n")
            self._send({
                "id": "chatcmpl-e2e", "object": "chat.completion", "created": 0,
                "model": "stub", "choices": [{"index": 0, "message": {
                    "role": "assistant", "content": self.server.reply}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            })
        elif has_tool_result:
            sys.stderr.write(
                "stub_llm: detected tool result in request, returning local final\n"
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
                            "message": {"role": "assistant", "content": self.server.reply},
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
        elif self._react_requested(messages):
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
        elif "local final test" in latest_user:
            self._send({
                "id": "chatcmpl-e2e", "object": "chat.completion", "created": 0,
                "model": "stub", "choices": [{"index": 0, "message": {
                    "role": "assistant", "content": self.server.reply}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            })
        else:
            if "matrix__send" not in tools:
                self._send({"error": "matrix__send unavailable"}, status=400)
                return
            sys.stderr.write("stub_llm: explicit matrix__send call\n")
            self._send({
                "id": "chatcmpl-e2e", "object": "chat.completion", "created": 0,
                "model": "stub", "choices": [{"index": 0, "message": {
                    "role": "assistant", "content": None, "tool_calls": [{
                        "id": "matrix-e2e", "type": "function", "function": {
                            "name": "matrix__send", "arguments": json.dumps({"body": self.server.reply}),
                        },
                    }]}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            })

    def log_message(self, fmt, *args):
        # Server logs go to the harness's log file, not the test's stderr.
        sys.stderr.write("stub_llm: " + (fmt % args) + "\n")


if __name__ == "__main__":
    server = HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
    server.reply = sys.argv[2]
    server.serve_forever()
