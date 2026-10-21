import importlib.util
import pathlib
import unittest
from unittest.mock import patch
from types import SimpleNamespace

MODULE = pathlib.Path(__file__).parents[1] / "scripts" / "shadow_qwen_acceptance.py"
spec = importlib.util.spec_from_file_location("shadow_qwen_acceptance", MODULE)
cli = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cli)


class ParserNormalizerTests(unittest.TestCase):
    def test_extracts_normal_chat_text(self):
        data = {"choices": [{"message": {"content": "hello"}, "finish_reason": "stop"}]}
        self.assertEqual(cli.normalize_response(data).text, "hello")

    def test_normalizes_tool_call_and_continuation(self):
        first = {"choices": [{"message": {"content": None, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "weather", "arguments": "{\"city\":\"Oslo\"}"}}]}, "finish_reason": "tool_calls"}]}
        second = {"choices": [{"message": {"content": "sunny"}, "finish_reason": "stop"}]}
        normalized = cli.normalize_response(first)
        self.assertEqual(normalized.tool_calls[0]["function"]["name"], "weather")
        self.assertEqual(cli.normalize_response(second).text, "sunny")

    def test_parses_stream_sse_done_and_content(self):
        stream = 'data: {"choices":[{"delta":{"content":"ok"}}]}\n\ndata: [DONE]\n\n'
        result = cli.parse_sse(stream)
        self.assertEqual(result.text, "ok")
        self.assertTrue(result.done)

    def test_rejects_missing_choices(self):
        with self.assertRaises(cli.AcceptanceError):
            cli.normalize_response({"error": "bad"})

    def test_rejects_stream_without_done(self):
        with self.assertRaises(cli.AcceptanceError):
            cli.parse_sse('data: {"choices":[]}\n\n')

    def test_malformed_request_cannot_pass_on_http_200(self):
        class Response:
            def __enter__(self): return self
            def __exit__(self, *_): pass
            def read(self, *_): return b'{"error":{"message":"not actually rejected"}}'
        with patch.object(cli.urllib.request, "urlopen", return_value=Response()):
            with self.assertRaises(cli.AcceptanceError):
                cli._malformed_probe("http://127.0.0.1/v1/chat/completions", "dummy-key", SimpleNamespace(timeout=1))


if __name__ == "__main__":
    unittest.main()
