"""Contract checks for the deterministic multi-round stub (no model calls)."""

import copy
import io
import json
import sys
import unittest
from contextlib import redirect_stderr
from types import SimpleNamespace
from unittest.mock import patch

sys.dont_write_bytecode = True
with patch.object(sys, "argv", ["stub_llm.py", "0", "reply"]):
    import stub_llm


class MultiRoundTests(unittest.TestCase):
    def request(self):
        return {
            "messages": [
                {"role": "user", "content": "old turn"},
                {"role": "tool", "tool_call_id": "old", "content": "old result"},
                {"role": "user", "content": stub_llm.MULTI_ROUND_TRIGGER},
                {"role": "user", "content": "## Relevant Memories\nNo memories found."},
            ],
            "tools": [{"type": "function", "function": {"name": "calculate"}}],
        }

    def advance(self, request, round_number):
        call = stub_llm.multi_round_response(request, "reply")
        self.assertEqual(
            call["tool_calls"][0]["id"], f"call_stub_calculate_{round_number}"
        )
        request["messages"].extend(
            [
                call,
                {
                    "role": "tool",
                    "tool_call_id": f"call_stub_calculate_{round_number}",
                    "content": f'<tool_output tool="calculate">\n{round_number + 100}\n</tool_output>',
                },
            ]
        )

    def test_twelve_rounds_ignore_history_and_finish_only_after_real_results(self):
        request = self.request()
        for round_number in range(1, 13):
            self.advance(request, round_number)
        self.assertEqual(
            stub_llm.multi_round_response(request, "reply"),
            {
                "role": "assistant",
                "content": "reply" + stub_llm.MULTI_ROUND_SUFFIX,
            },
        )
        request["messages"].append({"role": "user", "content": "next unrelated turn"})
        self.assertIsNone(stub_llm.multi_round_response(request, "reply"))

    def test_missing_wrong_or_denied_results_do_not_advance(self):
        request = self.request()
        self.advance(request, 1)
        for field, value in [
            ("tool_call_id", "old"),
            ("content", "denied"),
            ("role", "assistant"),
        ]:
            with self.subTest(field=field):
                broken = copy.deepcopy(request)
                broken["messages"][-1][field] = value
                with self.assertRaises(ValueError):
                    stub_llm.multi_round_response(broken, "reply")
        request["messages"].pop()
        with self.assertRaises(ValueError):
            stub_llm.multi_round_response(request, "reply")

    def test_mismatched_call_does_not_advance(self):
        request = self.request()
        self.advance(request, 1)
        request["messages"][-2]["tool_calls"][0]["function"]["arguments"] = "{}"
        with self.assertRaises(ValueError):
            stub_llm.multi_round_response(request, "reply")

    def test_compact_barrier_rejects_history_and_denied_current_result(self):
        def handle(messages):
            handler = object.__new__(stub_llm.Handler)
            body = json.dumps({"messages": messages, "tools": []}).encode()
            handler.headers = {"Content-Length": str(len(body))}
            handler.rfile = io.BytesIO(body)
            handler.path = "/v1/chat/completions"
            handler.server = SimpleNamespace(reply="reply", matrix_sends=True)
            responses = []
            handler._send = lambda payload, status=200: responses.append((payload, status))
            log = io.StringIO()
            with redirect_stderr(log):
                handler.do_POST()
            return responses[0], log.getvalue()

        result = {"role": "tool", "tool_call_id": "call_stub_compact_1",
                  "content": '<tool_output tool="compact">\nContext compacted. Summary written'}
        messages = [result, {"role": "user", "content": "react test"}]
        (response, status), log = handle(messages)
        self.assertEqual(status, 200)
        self.assertEqual(response["choices"][0]["message"]["tool_calls"][0]["id"],
                         "call_stub_compact_1")
        self.assertNotIn("compact: matched current result", log)
        (response, status), log = handle(messages + [dict(result, content="denied")])
        self.assertEqual(status, 400)
        self.assertNotIn("compact: matched current result", log)
        (response, status), log = handle(messages + [result])
        self.assertEqual(status, 200)
        self.assertEqual(response["choices"][0]["message"]["content"], "reply")
        self.assertIn("compact: matched current result", log)

    def test_tools_withdrawn_after_ten_results_is_not_success(self):
        request = self.request()
        for round_number in range(1, 11):
            self.advance(request, round_number)
        request["tools"] = []
        with self.assertRaisesRegex(ValueError, "definition withdrawn"):
            stub_llm.multi_round_response(request, "reply")


if __name__ == "__main__":
    unittest.main()
