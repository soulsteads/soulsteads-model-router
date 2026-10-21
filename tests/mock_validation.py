"""Run an isolated, real HTTP compatibility and admission check against the Rust binary."""

from concurrent.futures import ThreadPoolExecutor
import base64
from http.client import IncompleteRead
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from tempfile import TemporaryDirectory
from threading import Event, Lock
import json
import os
import subprocess
import struct
import time
import urllib.error
import urllib.request
import zlib

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("SLOTLINE_TEST_BINARY", str(ROOT / "target/release/slotline")))
HOST = os.environ.get("SLOTLINE_TEST_HOST", "100.64.0.1")
PORT = 4012
KEY = "slotline-mock-operator-key-1234567890"
LIMITED_KEY = "slotline-mock-limited-key-1234567890"
CANARY_KEY = "slotline-mock-canary-key-1234567890"


class Backend(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    lock = Lock()
    active = {}
    peak = {}
    fail_retry = True
    seen_thinking = []
    seen_extras = []
    seen_embedding_extra = []
    seen_embedding_formats = []
    seen_tools = []
    seen_images = []
    seen_formats = []
    seen_responses_extras = []
    seen_responses_tools = []
    seen_responses_formats = []
    hold_release = Event()

    def log_message(self, *_args):
        pass

    def send_json(self, value, status=200):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        model = "qwen3-embedding:0.6b" if self.server.server_port == 18114 else "gpt-6-luna" if self.server.server_port == 18118 else "qwen38" if self.server.server_port in (18104, 18109) else "qwen38flash"
        if self.path == "/v1/models":
            self.send_json({"object": "list", "data": [{"id": model}]})
        elif self.path in ("/health", "/healthz", "/api/tags"):
            self.send_json({"ok": True})
        else:
            self.send_json({"error": "not found"}, 404)

    def do_POST(self):
        if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
            chunks = []
            while True:
                size = int(self.rfile.readline().split(b";", 1)[0].strip(), 16)
                if size == 0:
                    while self.rfile.readline() not in (b"\r\n", b"\n"):
                        pass
                    break
                chunks.append(self.rfile.read(size))
                assert self.rfile.read(2) == b"\r\n"
            raw_body = b"".join(chunks)
        else:
            raw_body = self.rfile.read(int(self.headers["Content-Length"]))
        body = json.loads(raw_body)
        assert "Authorization" not in self.headers
        port = self.server.server_port
        if port == 18114:
            assert self.path == "/v1/embeddings" and body["model"] == "qwen3-embedding:0.6b"
            assert "metadata" not in body, "client metadata must not reach the model server"
            with self.lock:
                self.seen_embedding_extra.append(body.get("truncate"))
                self.seen_embedding_formats.append(body.get("encoding_format"))
            vector = base64.b64encode(struct.pack("<ff", 0.1, 0.2)).decode() if body.get("encoding_format") == "base64" else [0.1, 0.2]
            self.send_json({"model": body["model"], "object": "list", "data": [{"index": 0, "object": "embedding", "embedding": vector}]})
            return
        if self.path == "/v1/responses":
            model = "gpt-6-luna" if port == 18118 else "qwen38" if port in (18104, 18109) else "qwen38flash"
            assert body["model"] == model
            assert isinstance(body.get("input"), (str, list))
            with self.lock:
                self.seen_responses_extras.append((port, body.get("top_k")))
                if body.get("text", {}).get("format", {}).get("type") in ("json_schema", "json_object"):
                    self.seen_responses_formats.append((port, body["text"]["format"]))
                if body.get("tools") or any(isinstance(item, dict) and item.get("type") == "function_call_output" for item in body["input"] if isinstance(body["input"], list)):
                    self.seen_responses_tools.append((port, body.get("tools"), body.get("tool_choice"), body.get("parallel_tool_calls"), body["input"]))
                self.active[port] = self.active.get(port, 0) + 1
                self.peak[port] = max(self.peak.get(port, 0), self.active[port])
            try:
                if body["input"] == "hold":
                    self.hold_release.wait(timeout=10)
                if body.get("stream"):
                    created = json.dumps({"type": "response.created", "response": {"object": "response", "model": model}})
                    tool_item = json.dumps({"type": "response.output_item.added", "item": {"type": "function_call", "call_id": "call_mock_1", "name": "echo", "arguments": "{}"}})
                    output = [{"type": "function_call", "call_id": "call_mock_1", "name": "echo", "arguments": "{}"}] if body.get("tools") else [{"type": "message", "content": [{"type": "output_text", "text": "OK"}]}]
                    completed = json.dumps({"type": "response.completed", "response": {"object": "response", "model": model, "status": "completed", "output": output}})
                    first = f"event: response.created\ndata: {created}\n\n".encode()
                    middle = f"event: response.output_item.added\ndata: {tool_item}\n\n".encode() if body.get("tools") else b""
                    last = middle + f"event: response.completed\ndata: {completed}\n\n".encode()
                    tail = b""
                    if port == 18118:
                        done = json.dumps({"type": "response.done", "response": {"object": "response", "model": model}})
                        tail = f"event: response.done\ndata: {done}\n\n".encode()
                    truncated = body["input"] == "truncated"
                    premature_done = port == 18118 and body["input"] == "premature_done"
                    if premature_done:
                        tail = b"data: [DONE]\n\n"
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    if truncated:
                        self.send_header("Connection", "close")
                    else:
                        self.send_header("Content-Length", str(len(first) + len(last) + len(tail)))
                    self.end_headers()
                    self.wfile.write(first)
                    if not truncated:
                        self.wfile.write(last)
                        if premature_done:
                            self.wfile.flush()
                            time.sleep(0.05)
                        self.wfile.write(tail)
                    else:
                        self.close_connection = True
                else:
                    text = '{"ok":true}' if body.get("text", {}).get("format", {}).get("type") in ("json_schema", "json_object") else "OK"
                    output = [{"type": "function_call", "call_id": "call_mock_1", "name": "echo", "arguments": "{}"}] if body.get("tools") else [{"type": "message", "content": [{"type": "output_text", "text": text}]}]
                    self.send_json({"object": "response", "model": model, "status": "completed", "output": output})
            finally:
                with self.lock:
                    self.active[port] -= 1
            return
        assert self.path == "/v1/chat/completions"
        model = "gpt-6-luna" if port == 18118 else "qwen38" if port in (18104, 18109) else "qwen38flash"
        assert body["model"] == model
        fixture = body["messages"][0]["content"] == "ping"
        retry_case = body["messages"][0]["content"] == "retry"
        slow_stream = body["messages"][0]["content"] == "slow"
        truncated_stream = body["messages"][0]["content"] == "truncated"
        timeout_stream = body["messages"][0]["content"] == "timeout_stream"
        adapter_fail = body["messages"][0]["content"] == "adapter_fail" and port == 18108
        fast_route_check = body["messages"][0]["content"] == "rotate"
        hold_route_check = body["messages"][0]["content"] == "hold"
        tool_request = bool(body.get("tools"))
        tool_result = any(message.get("role") == "tool" for message in body["messages"])
        format_request = bool(body.get("response_format"))
        if not fixture:
            with self.lock:
                self.active[port] = self.active.get(port, 0) + 1
                self.peak[port] = max(self.peak.get(port, 0), self.active[port])
                if "chat_template_kwargs" in body:
                    self.seen_thinking.append((port, body["chat_template_kwargs"].get("enable_thinking")))
                self.seen_extras.append((port, {k: body[k] for k in ("top_k", "min_p", "repetition_penalty") if k in body}))
                if tool_request:
                    self.seen_tools.append((port, body["tools"], body.get("tool_choice"), body.get("parallel_tool_calls")))
                for message in body["messages"]:
                    if isinstance(message.get("content"), list) and any(part.get("type") == "image_url" for part in message["content"]):
                        self.seen_images.append((port, message["content"]))
                if format_request:
                    self.seen_formats.append((port, body["response_format"]))
        try:
            if not fixture:
                if retry_case:
                    with self.lock:
                        fail = self.fail_retry
                        self.__class__.fail_retry = False
                    if fail:
                        self.send_json({"error": "transient"}, 503)
                        return
                if adapter_fail:
                    self.send_json({"error": "adapter unavailable"}, 503)
                    return
                if hold_route_check:
                    self.hold_release.wait(timeout=10)
                elif not slow_stream and not fast_route_check and not tool_request and not tool_result and not format_request:
                    time.sleep(2)
            if body.get("stream"):
                delta = {"tool_calls": [{"index": 0, "id": "call_mock_1", "type": "function", "function": {"name": "get_status", "arguments": "{}"}}]} if tool_request else {"content": "OK"}
                chunk = f'data: {json.dumps({"model": model, "choices": [{"delta": delta}]})}\n\ndata: [DONE]\n\n'.encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                if slow_stream or truncated_stream or timeout_stream:
                    self.send_header("Connection", "close")
                else:
                    self.send_header("Content-Length", str(len(chunk)))
                self.end_headers()
                if truncated_stream:
                    first, _ = chunk.split(b"\n\n", 1)
                    self.wfile.write(first + b"\n\n")
                    self.wfile.flush()
                    self.close_connection = True
                elif timeout_stream:
                    first, rest = chunk.split(b"\n\n", 1)
                    self.wfile.write(first + b"\n\n")
                    self.wfile.flush()
                    time.sleep(6)
                    try:
                        self.wfile.write(rest)
                    except BrokenPipeError:
                        pass
                    self.close_connection = True
                elif slow_stream:
                    first, rest = chunk.split(b"\n\n", 1)
                    self.wfile.write(first + b"\n\n")
                    self.wfile.flush()
                    time.sleep(4)
                    try:
                        self.wfile.write(rest)
                    except BrokenPipeError:
                        pass
                    self.close_connection = True
                else:
                    self.wfile.write(chunk)
            else:
                message = {"content": "OK"}
                if tool_request:
                    message = {"content": None, "tool_calls": [{"id": "call_mock_1", "type": "function", "function": {"name": "get_status", "arguments": "{}"}}]}
                elif format_request:
                    message = {"content": json.dumps({"ok": True} if body["response_format"]["type"] == "json_object" else {"title": "Mock session title"})}
                self.send_json({"model": model, "choices": [{"message": message}]})
        finally:
            if not fixture:
                with self.lock:
                    self.active[port] -= 1


def request(path, body=None, key=KEY, extra_headers=None):
    headers = {"Authorization": "Bearer " + key}
    if extra_headers:
        headers.update(extra_headers)
    if body is not None:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(
        f"http://{HOST}:{PORT}{path}",
        data=json.dumps(body).encode() if body is not None else None,
        headers=headers,
    )
    try:
        with urllib.request.urlopen(req, timeout=15) as response:
            return response.status, response.read().decode()
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode()


def chat(model, key=KEY, session=None, **extra):
    body = {"model": model, "messages": [{"role": "user", "content": "test"}], "max_tokens": 8}
    body.update(extra)
    headers = {"X-Slotline-Session": session} if session is not None else None
    return request("/v1/chat/completions", body, key=key, extra_headers=headers)


def main():
    servers = [ThreadingHTTPServer(("127.0.0.1", port), Backend) for port in (18102, 18106, 18104, 18109, 18108, 18114, 18116, 18118)]
    from threading import Thread
    threads = [Thread(target=server.serve_forever, daemon=True) for server in servers]
    for thread in threads:
        thread.start()
    with TemporaryDirectory() as temp:
        root = Path(temp)
        (root / "operator.key").write_text(KEY)
        (root / "limited.key").write_text(LIMITED_KEY)
        (root / "canary.key").write_text(CANARY_KEY)
        (root / "operator.key").chmod(0o600)
        (root / "limited.key").chmod(0o600)
        (root / "canary.key").chmod(0o600)
        config = f"""
bind: {HOST}
port: {PORT}
refresh_seconds: 30
affinity_lease_seconds: 5
credentials:
  - {{name: operator, key_file: {root}/operator.key, aliases: [qwen38auto, qwen38flash, qwen38, qwen38mixed, qwen3-embedding, direct-2, direct-9, direct-16, codex-mock, headroom-2], class: test, requests_per_minute: 10000, burst: 100, max_in_flight: 100, operator: true}}
  - {{name: limited, key_file: {root}/limited.key, aliases: [qwen38flash], class: limited, requests_per_minute: 10000, burst: 100, max_in_flight: 100, agent_affinity: true}}
  - {{name: headroom-canary, key_file: {root}/canary.key, aliases: [qwen38flash], allowed_routes: [headroom-2], class: canary, requests_per_minute: 10000, burst: 100, max_in_flight: 100}}
routes:
  - {{name: flash-2, kind: local, base_url: 'http://127.0.0.1:18102/v1', model: qwen38flash, safe_concurrency: 4, context_limit: 262144, adapter_version: mock-v1, capabilities: [text, image, tools, json_schema, json_object, responses, responses_json_schema, responses_json_object], enabled: true}}
  - {{name: flash-6, kind: local, base_url: 'http://127.0.0.1:18106/v1', model: qwen38flash, safe_concurrency: 2, context_limit: 262144, adapter_version: mock-v1, capabilities: [text, tools, json_schema, responses], enabled: true}}
  - {{name: qwen-4, kind: local, base_url: 'http://127.0.0.1:18104/v1', model: qwen38, safe_concurrency: 2, context_limit: 200000, adapter_version: mock-v1, capabilities: [text, image, tools, json_schema, json_object, responses, responses_json_schema, responses_json_object], disable_thinking: true, enabled: true}}
  - {{name: partial-9, kind: local, base_url: 'http://127.0.0.1:18109/v1', model: qwen38, safe_concurrency: 1, context_limit: 200000, adapter_version: mock-v1, capabilities: [text, tools], disable_thinking: true, timeout_seconds: 4, enabled: true}}
  - {{name: responses-16, kind: local, base_url: 'http://127.0.0.1:18116/v1', model: qwen38flash, safe_concurrency: 1, context_limit: 262144, adapter_version: mock-v1, capabilities: [text, responses], enabled: true}}
  - {{name: codex-18, kind: local, base_url: 'http://127.0.0.1:18118/v1', model: gpt-6-luna, safe_concurrency: 1, context_limit: 258000, adapter_version: mock-v1, capabilities: [text, responses], responses_done_tail: true, enabled: true}}
  - {{name: headroom-2, kind: headroom, physical_route: flash-2, base_url: 'http://127.0.0.1:18108/v1', model: qwen38flash, health_path: /healthz, safe_concurrency: 4, context_limit: 262144, adapter_version: mock-headroom-v1, capabilities: [text, tools, json_schema], enabled: true}}
  - {{name: embedding-14, kind: embedding, base_url: 'http://127.0.0.1:18114/v1', model: 'qwen3-embedding:0.6b', health_path: /api/tags, safe_concurrency: 1, context_limit: 32768, adapter_version: mock-v1, capabilities: [text], streaming: false, enabled: true}}
aliases:
  direct-2: {{context_limit: 262144, capabilities: [text, responses], routes: [{{route: flash-2, priority: 10}}]}}
  direct-16: {{context_limit: 262144, capabilities: [text, responses], routes: [{{route: responses-16, priority: 10}}]}}
  codex-mock: {{context_limit: 258000, capabilities: [text, responses], routes: [{{route: codex-18, priority: 10}}]}}
  direct-9: {{context_limit: 200000, capabilities: [text], routes: [{{route: partial-9, priority: 10}}]}}
  headroom-2: {{context_limit: 262144, capabilities: [text], routes: [{{route: headroom-2, priority: 10}}]}}
  qwen38auto: {{context_limit: 200000, capabilities: [text, image, tools, json_schema, json_object, responses, responses_json_schema, responses_json_object], routes: [{{route: flash-2, priority: 10}}, {{route: flash-6, priority: 10}}, {{route: qwen-4, priority: 20}}]}}
  qwen38flash: {{context_limit: 262144, capabilities: [text, image, tools, json_schema, json_object, responses, responses_json_schema, responses_json_object], routes: [{{route: flash-2, priority: 10}}, {{route: flash-6, priority: 10}}, {{route: headroom-2, priority: 100, fallback_only: true}}]}}
  qwen38: {{context_limit: 200000, capabilities: [text, image, tools, json_schema, json_object, responses, responses_json_schema, responses_json_object], routes: [{{route: qwen-4, priority: 10}}]}}
  qwen38mixed: {{context_limit: 200000, capabilities: [text, image, tools, json_schema, json_object, responses, responses_json_schema, responses_json_object], routes: [{{route: partial-9, priority: 10}}, {{route: qwen-4, priority: 20, fallback_only: true}}]}}
  qwen3-embedding: {{endpoint: embedding, context_limit: 32768, capabilities: [text], streaming: false, routes: [{{route: embedding-14, priority: 10}}]}}
"""
        (root / "config.yaml").write_text(config)
        for old, new in (
            ("physical_route: flash-2", "physical_route: missing-route"),
            ("physical_route: flash-2", "physical_route: qwen-4"),
            ("physical_route: flash-2", "physical_route: flash-6"),
            ("http://127.0.0.1:18108/v1", "http://100.64.0.1:18108/v1"),
            ("health_path: /healthz, safe_concurrency: 4", "health_path: /health, safe_concurrency: 4"),
            ("allowed_routes: [headroom-2]", "allowed_routes: [embedding-14]"),
            ("allowed_routes: [headroom-2]", "allowed_routes: [unknown-route]"),
            ("capabilities: [text, responses], responses_done_tail: true", "capabilities: [text], responses_done_tail: true"),
        ):
            invalid = root / "invalid-headroom.yaml"
            invalid.write_text(config.replace(old, new, 1))
            checked = subprocess.run(
                [str(BINARY), "--check-config", str(invalid)],
                capture_output=True, text=True,
            )
            assert checked.returncode != 0, (old, new, checked.stdout)
        process = subprocess.Popen([str(BINARY), str(root / "config.yaml")], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            for _ in range(50):
                try:
                    status, data = request("/healthz")
                    if status == 200 and all(item["state"] == "serving" for item in json.loads(data)["routes"].values()):
                        break
                except urllib.error.URLError:
                    pass
                time.sleep(.1)
            else:
                raise AssertionError("mock routes never became serving")
            assert request("/status", key=LIMITED_KEY)[0] == 403
            assert request("/status", key="wrong")[0] == 401
            status_code, snapshot = request("/status")
            assert status_code == 200
            snapshot = json.loads(snapshot)
            assert snapshot["routes"]["flash-2"]["state"] == "serving"
            assert [entry["route"] for entry in snapshot["credentials"]["headroom-canary"]["aliases"]["qwen38flash"]] == ["headroom-2"]
            assert KEY not in json.dumps(snapshot) and LIMITED_KEY not in json.dumps(snapshot)
            cli = subprocess.run([str(BINARY), "status", str(root / "config.yaml")], capture_output=True, text=True, timeout=15)
            assert cli.returncode == 0 and "API-key routes" in cli.stdout and "Advisory leases" in cli.stdout, cli.stderr
            assert KEY not in cli.stdout and KEY not in cli.stderr
            json_cli = subprocess.run([str(BINARY), "status", str(root / "config.yaml"), "--json"], capture_output=True, text=True, timeout=15)
            assert json_cli.returncode == 0 and "routes" in json.loads(json_cli.stdout), json_cli.stderr
            env = dict(os.environ, SLOTLINE_CONFIG=str(root / "config.yaml"))
            bare_cli = subprocess.run([str(BINARY), "status"], env=env, capture_output=True, text=True, timeout=15)
            assert bare_cli.returncode == 0 and bare_cli.stdout == cli.stdout, bare_cli.stderr
            bare_json = subprocess.run([str(BINARY), "status", "--json"], env=env, capture_output=True, text=True, timeout=15)
            assert bare_json.returncode == 0 and "routes" in json.loads(bare_json.stdout), bare_json.stderr
            assert request("/v1/models", key="wrong")[0] == 401
            status, data = request("/v1/models", key=LIMITED_KEY)
            assert status == 200 and [m["id"] for m in json.loads(data)["data"]] == ["qwen38flash"]
            assert chat("qwen3-embedding")[0] == 400
            embed_status, embed_data = request("/v1/embeddings", {"model": "qwen3-embedding", "input": "hello"})
            assert embed_status == 200 and json.loads(embed_data)["model"] == "qwen3-embedding"
            assert request("/v1/embeddings", {"model": "qwen3-embedding", "input": "x" * 32736})[0] == 200
            assert request("/v1/embeddings", {"model": "qwen3-embedding", "input": "x" * 32737})[0] == 400
            assert request("/v1/embeddings", {"model": "qwen3-embedding", "input": "界" * 10913})[0] == 400
            assert request("/v1/embeddings", {"model": "qwen3-embedding", "input": ["ok", "x" * 32737]})[0] == 400
            # Hindsight sends request metadata; accept it locally but do not forward it.
            embed_status, embed_data = request("/v1/embeddings", {"model": "qwen3-embedding", "input": ["hello"], "metadata": {"source": "fixture"}})
            assert embed_status == 200 and json.loads(embed_data)["model"] == "qwen3-embedding"
            assert request("/v1/embeddings", {"model": "qwen3-embedding", "input": "hello", "metadata": "not-an-object"})[0] == 400
            assert request("/v1/embeddings", {"model": "qwen3-embedding", "input": "hello", "metadata": {"api_key": "should-not-pass"}})[0] == 400
            embed_status, embed_data = request("/v1/embeddings", {"model": "qwen3-embedding", "input": ["hello"], "encoding_format": "base64"})
            encoded = json.loads(embed_data)["data"][0]["embedding"]
            assert embed_status == 200 and len(base64.b64decode(encoded)) == 8
            assert "base64" in Backend.seen_embedding_formats
            assert request("/v1/embeddings", {"model": "qwen3-embedding", "input": "hello", "encoding_format": "hex"})[0] == 400
            assert request("/v1/embeddings", {"model": "qwen3-embedding", "input": "hello", "extra_body": {"truncate": True}})[0] == 200
            assert True in Backend.seen_embedding_extra
            assert request("/v1/embeddings", {"model": "qwen3-embedding", "input": [1, 2]})[0] == 400
            assert chat("qwen38auto", max_tokens=200001)[0] == 400
            def png_chunk(tag, data):
                return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data))
            image = (b"\x89PNG\r\n\x1a\n" + png_chunk(b"IHDR", struct.pack(">IIBBBBB", 48, 48, 8, 2, 0, 0, 0))
                     + png_chunk(b"IDAT", zlib.compress((b"\x00" + b"\xff\x00\x00" * 48) * 48)) + png_chunk(b"IEND", b""))
            url = "data:image/png;base64," + base64.b64encode(image).decode()
            parts = [{"type": "text", "text": "red?"}, {"type": "image_url", "image_url": {"url": url}}]
            _, before_image = request("/healthz")
            before_image = json.loads(before_image)["routes"]
            status, data = chat("qwen38auto", messages=[{"role": "user", "content": parts}], max_tokens=32)
            assert status == 200 and json.loads(data)["model"] == "qwen38auto", (status, data)
            assert any(port in (18102, 18104) and seen == parts for port, seen in Backend.seen_images)
            _, after_image = request("/healthz")
            after_image = json.loads(after_image)["routes"]
            assert after_image["flash-6"]["attempts_total"] == before_image["flash-6"]["attempts_total"]
            assert after_image["headroom-2"]["attempts_total"] == before_image["headroom-2"]["attempts_total"]
            assert chat("qwen38flash", key=CANARY_KEY, messages=[{"role": "user", "content": parts}])[0] == 400
            stream_status, stream_data = chat("qwen38", messages=[{"role": "user", "content": parts}], max_tokens=32, stream=True)
            assert stream_status == 200 and "data: [DONE]" in stream_data
            assert any(port == 18104 and seen == parts for port, seen in Backend.seen_images)
            assert chat("qwen38", messages=[{"role": "user", "content": parts}], max_tokens=199000)[0] == 400
            assert chat("qwen38", messages=[{"role": "assistant", "content": parts}])[0] == 400
            assert chat("qwen38", messages=[{"role": "user", "content": parts * 5}])[0] == 400
            assert chat("qwen38", messages=[{"role": "user", "content": [{"type": "image_url", "image_url": {"url": "data:image/png;base64,bad!"}}]}])[0] == 400
            assert chat("qwen38", messages=[{"role": "user", "content": [{"type": "image_url", "image_url": {"url": url, "detail": "wrong"}}]}])[0] == 400
            assert chat("qwen38auto", messages=[{"role": "user", "content": [{"type": "image_url", "image_url": {"url": "http://127.0.0.1/private"}}]}])[0] == 400
            assert chat("qwen38auto", messages=[{"role": "user", "content": [{"type": "image_url", "image_url": {"url": "x"}}]}])[0] == 400
            assert chat("qwen38auto", messages=[{"role": "user", "content": [{"type": "input_audio", "input_audio": {"data": "abc"}}]}])[0] == 400
            assert chat("qwen38auto", route="flash-2")[0] == 400
            assert chat("qwen38auto", extra_body={"base_url": "http://127.0.0.1:9999/v1"})[0] == 400
            assert chat("qwen38auto", chat_template_kwargs={"enable_thinking": "false"})[0] == 400
            assert chat("qwen38flash", extra_body={"chat_template_kwargs": {"enable_thinking": False}})[0] == 200
            assert chat("qwen38", enable_thinking=False)[0] == 200
            assert chat("qwen38auto", chat_template_kwargs={"enable_thinking": False})[0] == 200
            assert chat("qwen38auto", extra_body={"top_k": 20, "min_p": 0.05, "repetition_penalty": 1.1})[0] == 200
            assert any(extras == {"top_k": 20, "min_p": 0.05, "repetition_penalty": 1.1} for _, extras in Backend.seen_extras)
            assert any(port == 18104 and value is False for port, value in Backend.seen_thinking)
            assert any(port in (18102, 18106) and value is False for port, value in Backend.seen_thinking)
            tool = {"type": "function", "function": {"name": "get_status", "description": "Return current status", "parameters": {"type": "object", "properties": {}}}}
            status, data = chat("qwen38flash", tools=[tool], tool_choice="required", parallel_tool_calls=False)
            assert status == 200 and json.loads(data)["choices"][0]["message"]["tool_calls"][0]["function"]["name"] == "get_status"
            assert any(tools == [tool] and choice == "required" and parallel is False for _, tools, choice, parallel in Backend.seen_tools)
            status, data = chat("qwen38", tools=[tool], tool_choice="auto", stream=True)
            assert status == 200 and '"tool_calls"' in data and '"model":"qwen38"' in data and "data: [DONE]" in data
            continuation = [{"role": "user", "content": "Check status"}, {"role": "assistant", "content": None, "tool_calls": [{"id": "call_mock_1", "type": "function", "function": {"name": "get_status", "arguments": "{}"}}]}, {"role": "tool", "tool_call_id": "call_mock_1", "content": "online"}]
            assert chat("qwen38auto", messages=continuation)[0] == 200
            assert chat("qwen38flash", tools=[{"type": "retrieval", "function": tool["function"]}])[0] == 400
            assert chat("qwen38flash", tools=[tool], tool_choice={"type": "function", "function": {"name": "other"}})[0] == 400
            title_format = {"type": "json_schema", "json_schema": {"name": "session_title", "strict": True, "schema": {"type": "object", "properties": {"title": {"type": "string"}}, "required": ["title"], "additionalProperties": False}}}
            status, data = chat("qwen38auto", response_format=title_format)
            assert status == 200 and json.loads(json.loads(data)["choices"][0]["message"]["content"])["title"] == "Mock session title"
            assert chat("qwen38", extra_body={"response_format": title_format})[0] == 200
            assert any(format_value == title_format for _, format_value in Backend.seen_formats)
            _, health_before = request("/healthz")
            before_routes = json.loads(health_before)["routes"]
            assert chat("qwen38mixed", messages=[{"role": "user", "content": "rotate"}], tools=[tool], tool_choice="required")[0] == 200
            _, health_after_tool = request("/healthz")
            tool_routes = json.loads(health_after_tool)["routes"]
            assert tool_routes["partial-9"]["attempts_total"] == before_routes["partial-9"]["attempts_total"] + 1
            assert chat("qwen38mixed", response_format=title_format)[0] == 200
            _, health_after_schema = request("/healthz")
            schema_routes = json.loads(health_after_schema)["routes"]
            assert schema_routes["partial-9"]["attempts_total"] == tool_routes["partial-9"]["attempts_total"]
            assert schema_routes["qwen-4"]["attempts_total"] == tool_routes["qwen-4"]["attempts_total"] + 1
            status, data = chat("qwen38flash", key=CANARY_KEY, tools=[tool], tool_choice="required")
            assert status == 200 and json.loads(data)["choices"][0]["message"]["tool_calls"][0]["function"]["name"] == "get_status"
            status, data = chat("qwen38flash", key=CANARY_KEY, tools=[tool], tool_choice="required", stream=True)
            assert status == 200 and '"tool_calls"' in data and "data: [DONE]" in data
            status, data = chat("qwen38flash", key=CANARY_KEY, response_format=title_format)
            assert status == 200 and json.loads(json.loads(data)["choices"][0]["message"]["content"])["title"] == "Mock session title"
            assert chat("qwen38", key=CANARY_KEY)[0] == 403
            _, health_before_object = request("/healthz")
            before_object = json.loads(health_before_object)["routes"]
            status, data = chat("qwen38mixed", response_format={"type": "json_object"})
            assert status == 200 and json.loads(json.loads(data)["choices"][0]["message"]["content"])["ok"] is True
            _, health_after_object = request("/healthz")
            after_object = json.loads(health_after_object)["routes"]
            assert after_object["partial-9"]["attempts_total"] == before_object["partial-9"]["attempts_total"]
            assert after_object["qwen-4"]["attempts_total"] == before_object["qwen-4"]["attempts_total"] + 1
            assert chat("qwen38auto", response_format={"type": "json_object"})[0] == 200
            assert chat("qwen38auto", response_format={"type": "json_object", "json_schema": {}})[0] == 400
            remote_ref = {"type": "json_schema", "json_schema": {"name": "unsafe", "schema": {"$ref": "https://example.invalid/schema.json"}}}
            assert chat("qwen38auto", response_format=remote_ref)[0] == 400
            status, data = chat("qwen38auto")
            assert status == 200 and json.loads(data)["model"] == "qwen38auto"
            status, data = chat("qwen38auto", stream=True)
            assert status == 200 and '"model":"qwen38auto"' in data and "data: [DONE]" in data
            status, data = request("/v1/responses", {"model": "qwen38auto", "input": "Reply OK.", "max_output_tokens": 8})
            assert status == 200 and json.loads(data)["object"] == "response" and json.loads(data)["model"] == "qwen38auto"
            assert request("/v1/responses", {"model": "qwen38auto", "input": "Reply OK.", "max_output_tokens": 8, "extra_body": {"top_k": 7}})[0] == 200
            assert any(value == 7 for _, value in Backend.seen_responses_extras)
            status, data = request("/v1/responses", {"model": "qwen38flash", "input": [{"role": "user", "content": [{"type": "input_text", "text": "Reply OK."}]}], "max_output_tokens": 8, "stream": True})
            assert status == 200 and "event: response.completed" in data and '"model":"qwen38flash"' in data and '"model":"qwen38"' not in data
            status, data = request("/v1/responses", {"model": "codex-mock", "input": "Reply OK.", "max_output_tokens": 8})
            assert status == 200 and json.loads(data)["model"] == "codex-mock", (status, data[:300])
            status, data = request("/v1/responses", {"model": "codex-mock", "input": "Reply OK.", "max_output_tokens": 8, "stream": True})
            assert status == 200 and "event: response.completed" in data and "event: response.done" in data and "data: [DONE]" in data
            assert '"model":"codex-mock"' in data and '"model":"gpt-6-luna"' not in data
            assert request("/v1/responses", {"model": "qwen38auto", "input": "test", "previous_response_id": "resp_123"})[0] == 400
            assert request("/v1/responses", {"model": "qwen38auto", "input": "test", "max_output_tokens": 200001})[0] == 400
            assert request("/v1/responses", {"model": "qwen38auto", "input": "test", "max_output_tokens": 200000})[0] == 400
            assert request("/v1/responses", {"model": "qwen38auto", "input": [{"role": "user", "content": [{"type": "input_image", "image_url": "https://example.invalid/a.png"}]}]})[0] == 400
            response_tool = {"type": "function", "name": "echo", "description": "Echo text", "parameters": {"type": "object", "properties": {"text": {"type": "string"}}}}
            status, data = request("/v1/responses", {"model": "qwen38auto", "input": "Call echo.", "tools": [response_tool], "tool_choice": {"type": "function", "name": "echo"}, "parallel_tool_calls": False})
            assert status == 200 and json.loads(data)["output"][0]["type"] == "function_call", (status, data[:500])
            assert any(tools == [response_tool] and choice == {"type": "function", "name": "echo"} and parallel is False for _, tools, choice, parallel, _ in Backend.seen_responses_tools)
            status, data = request("/v1/responses", {"model": "qwen38flash", "input": "Call echo.", "tools": [response_tool], "stream": True})
            assert status == 200 and "event: response.output_item.added" in data and "event: response.completed" in data
            continuation = [{"type": "message", "role": "user", "content": "Call echo."}, {"type": "function_call", "call_id": "call_mock_1", "name": "echo", "arguments": "{}"}, {"type": "function_call_output", "call_id": "call_mock_1", "output": "done"}]
            status, data = request("/v1/responses", {"model": "qwen38auto", "input": continuation})
            assert status == 200 and json.loads(data)["output"][0]["type"] == "message"
            assert any(isinstance(items, list) and any(isinstance(item, dict) and item.get("type") == "function_call_output" for item in items) for _, _, _, _, items in Backend.seen_responses_tools)
            assert request("/v1/responses", {"model": "qwen38auto", "input": [{"type": "function_call_output", "call_id": "unknown", "output": "done"}]})[0] == 400
            assert request("/v1/responses", {"model": "qwen38auto", "input": "test", "tools": [response_tool], "tool_choice": {"type": "function", "name": "unknown"}})[0] == 400
            assert request("/v1/responses", {"model": "direct-16", "input": "test", "tools": [response_tool]})[0] == 400
            response_format = {"type": "json_schema", "name": "answer", "strict": True, "schema": {"type": "object", "properties": {"ok": {"type": "boolean"}}, "required": ["ok"], "additionalProperties": False}}
            status, data = request("/v1/responses", {"model": "qwen38flash", "input": "Return ok true.", "text": {"format": response_format}})
            assert status == 200 and json.loads(json.loads(data)["output"][0]["content"][0]["text"])["ok"] is True
            assert any(port == 18102 and fmt == response_format for port, fmt in Backend.seen_responses_formats)
            assert all(port != 18106 for port, _ in Backend.seen_responses_formats)
            object_format = {"type": "json_object"}
            status, data = request("/v1/responses", {"model": "qwen38flash", "input": "Return ok true.", "text": {"format": object_format}})
            assert status == 200 and json.loads(json.loads(data)["output"][0]["content"][0]["text"])["ok"] is True
            assert any(port == 18102 and fmt == object_format for port, fmt in Backend.seen_responses_formats)
            assert all(port != 18106 for port, fmt in Backend.seen_responses_formats if fmt == object_format)
            status, data = request("/v1/responses", {"model": "qwen38", "input": "Return ok true.", "text": {"format": object_format}})
            assert status == 200 and json.loads(json.loads(data)["output"][0]["content"][0]["text"])["ok"] is True
            assert any(port == 18104 and fmt == object_format for port, fmt in Backend.seen_responses_formats)
            assert request("/v1/responses", {"model": "direct-16", "input": "test", "text": {"format": object_format}})[0] == 400
            assert request("/v1/responses", {"model": "qwen38flash", "input": "test", "text": {"format": {"type": "json_object", "schema": {}}}})[0] == 400
            assert request("/v1/responses", {"model": "direct-16", "input": "test", "text": {"format": response_format}})[0] == 400
            remote_response_format = {"type": "json_schema", "name": "answer", "schema": {"$ref": "https://example.invalid/schema.json"}}
            assert request("/v1/responses", {"model": "qwen38auto", "input": "test", "text": {"format": remote_response_format}})[0] == 400
            assert request("/v1/responses", {"model": "qwen38auto", "input": "test", "text": {"format": {"type": "json_schema"}}})[0] == 400
            assert request("/v1/responses", {"model": "qwen3-embedding", "input": "test"})[0] == 400
            def attempts():
                _, payload = request("/healthz")
                return {name: route["attempts_total"] for name, route in json.loads(payload)["routes"].items()}

            before = attempts()
            assert chat("qwen38flash", key=CANARY_KEY, messages=[{"role": "user", "content": "rotate"}])[0] == 200
            after = attempts()
            assert after["headroom-2"] == before["headroom-2"] + 1
            assert after["flash-2"] == before["flash-2"] and after["flash-6"] == before["flash-6"]

            # When idle, the machine with the most free physical slots wins.
            previous = attempts()
            headroom_before_rotation = previous["headroom-2"]
            for _ in range(6):
                assert chat("qwen38flash", messages=[{"role": "user", "content": "rotate"}])[0] == 200
                current = attempts()
                assert current["flash-2"] == previous["flash-2"] + 1, (previous, current)
                assert current["flash-6"] == previous["flash-6"]
                previous = current
            assert attempts()["headroom-2"] == headroom_before_rotation

            # A dedicated agent credential identifies the agent without a caller header.
            before = attempts()
            assert chat("qwen38flash", key=LIMITED_KEY, messages=[{"role": "user", "content": "rotate"}])[0] == 200
            after = attempts()
            agent_route = next(name for name in ("flash-2", "flash-6") if after[name] > before[name])
            assert chat("qwen38flash", messages=[{"role": "user", "content": "rotate"}])[0] == 200
            before = attempts()
            assert chat("qwen38flash", key=LIMITED_KEY, messages=[{"role": "user", "content": "rotate"}])[0] == 200
            after = attempts()
            assert after[agent_route] == before[agent_route] + 1, (before, after)

            # A caller session header cannot split a dedicated profile lease.
            before = attempts()
            assert chat("qwen38flash", key=LIMITED_KEY, session="thread-a", messages=[{"role": "user", "content": "rotate"}])[0] == 200
            after = attempts()
            assert after[agent_route] == before[agent_route] + 1
            before = after
            assert chat("qwen38flash", key=LIMITED_KEY, session="thread-b", messages=[{"role": "user", "content": "rotate"}])[0] == 200
            after = attempts()
            assert after[agent_route] == before[agent_route] + 1
            before = after
            assert chat("qwen38flash", key=LIMITED_KEY, session="thread-a", messages=[{"role": "user", "content": "rotate"}])[0] == 200
            after = attempts()
            assert after[agent_route] == before[agent_route] + 1

            # A shared credential can supply a bounded session hint. Idle
            # expiration is observable, but with equal idle occupancy the
            # larger physical pool can still win on fresh placement.
            session = "agent-thread-1"
            before = attempts()
            assert chat("qwen38flash", session=session, messages=[{"role": "user", "content": "rotate"}])[0] == 200
            after = attempts()
            session_route = next(name for name in ("flash-2", "flash-6") if after[name] > before[name])
            before = after
            assert chat("qwen38flash", session=session, messages=[{"role": "user", "content": "rotate"}])[0] == 200
            after = attempts()
            assert after[session_route] == before[session_route] + 1, (before, after)
            metrics_before = request("/metrics")[1]
            expired_before = int(next(line.split()[1] for line in metrics_before.splitlines() if line.startswith("slotline_affinity_expired_total ")))
            time.sleep(5.1)
            assert chat("qwen38flash", session=session, messages=[{"role": "user", "content": "rotate"}])[0] == 200
            metrics_after = request("/metrics")[1]
            expired_after = int(next(line.split()[1] for line in metrics_after.splitlines() if line.startswith("slotline_affinity_expired_total ")))
            assert expired_after > expired_before, (expired_before, expired_after)

            # Direct and adapter routes to the same physical Qwen server
            # must consume one shared capacity pool.
            Backend.hold_release.clear()
            with ThreadPoolExecutor(max_workers=4) as held:
                try:
                    holds = [held.submit(chat, "direct-2", messages=[{"role": "user", "content": "hold"}]) for _ in range(4)]
                    for _ in range(50):
                        _, payload = request("/healthz")
                        routes = json.loads(payload)["routes"]
                        if routes["flash-2"]["active"] == 4:
                            break
                        time.sleep(.02)
                    else:
                        raise AssertionError("shared physical route never filled")
                    assert routes["headroom-2"]["active"] == 4, routes
                    before = attempts()
                    assert chat("headroom-2", messages=[{"role": "user", "content": "rotate"}])[0] == 503
                    assert attempts() == before, "adapter bypassed physical admission"
                finally:
                    Backend.hold_release.set()
                assert all(future.result()[0] == 200 for future in holds)
            before = attempts()
            status, data = chat("headroom-2", messages=[{"role": "user", "content": "rotate"}])
            assert status == 200 and json.loads(data)["model"] == "headroom-2"
            assert attempts()["headroom-2"] == before["headroom-2"] + 1

            # A profile's occupied route yields to an empty eligible machine;
            # the lease remains advisory and physical permits are not reclaimed.
            Backend.hold_release.clear()
            with ThreadPoolExecutor(max_workers=2) as held:
                try:
                    hold = held.submit(chat, "qwen38flash", key=LIMITED_KEY, messages=[{"role": "user", "content": "hold"}])
                    for _ in range(50):
                        _, payload = request("/healthz")
                        routes = json.loads(payload)["routes"]
                        if routes["flash-2"]["active"] == 1:
                            break
                        time.sleep(.02)
                    else:
                        raise AssertionError("profile hold never became active")
                    before = attempts()
                    assert chat("qwen38flash", key=LIMITED_KEY, messages=[{"role": "user", "content": "rotate"}])[0] == 200
                    after = attempts()
                    assert after["flash-6"] == before["flash-6"] + 1
                    assert after["flash-2"] == before["flash-2"]
                finally:
                    Backend.hold_release.set()
                assert hold.result()[0] == 200

            # With 2/4 versus 1/2 occupied, choose fewer occupied slots.
            def wait_for_active(expected):
                for _ in range(50):
                    _, payload = request("/healthz")
                    routes = json.loads(payload)["routes"]
                    if sum(routes[name]["active"] for name in ("flash-2", "flash-6")) == expected:
                        return routes
                    time.sleep(.02)
                raise AssertionError(f"Flash active count never reached {expected}")

            Backend.hold_release.clear()
            with ThreadPoolExecutor(max_workers=3) as held:
                try:
                    holds = []
                    for count in range(1, 4):
                        holds.append(held.submit(chat, "qwen38flash", messages=[{"role": "user", "content": "hold"}]))
                        routes = wait_for_active(count)
                    assert (routes["flash-2"]["active"], routes["flash-6"]["active"]) == (2, 1), routes
                    before = attempts()
                    assert chat("qwen38flash", messages=[{"role": "user", "content": "rotate"}])[0] == 200
                    after = attempts()
                    assert after["flash-6"] == before["flash-6"] + 1 and after["flash-2"] == before["flash-2"], (before, after)
                    before = after
                    assert chat("qwen38auto", messages=[{"role": "user", "content": "rotate"}])[0] == 200
                    after = attempts()
                    assert after["qwen-4"] == before["qwen-4"] + 1, (before, after)
                finally:
                    Backend.hold_release.set()
                assert all(future.result()[0] == 200 for future in holds)
            with ThreadPoolExecutor(max_workers=28) as executor:
                futures = [executor.submit(chat, "qwen38auto" if i % 2 else "qwen38flash") for i in range(28)]
                statuses = [future.result()[0] for future in futures]
            assert 200 in statuses and 503 in statuses, statuses
            assert all(Backend.peak.get(port, 0) <= limit for port, limit in ((18102, 4), (18106, 2), (18104, 2))), Backend.peak
            assert Backend.peak.get(18104, 0) > 0
            _, health_data=request("/healthz")
            route_data=json.loads(health_data)["routes"]
            assert abs(sum(item["traffic_percent_total"] for item in route_data.values())-100)<0.001
            assert abs(sum(item["traffic_percent_5m"] for item in route_data.values())-100)<0.001
            _, metrics_data=request("/metrics")
            assert "slotline_route_skips_total" in metrics_data and "slotline_route_traffic_percent" in metrics_data
            status, data = chat("qwen38flash", messages=[{"role": "user", "content": "retry"}])
            assert status == 200 and json.loads(data)["model"] == "qwen38flash"
            slow_body = {"model": "qwen38", "messages": [{"role": "user", "content": "slow"}], "max_tokens": 8, "stream": True}
            slow_req = urllib.request.Request(f"http://{HOST}:{PORT}/v1/chat/completions", data=json.dumps(slow_body).encode(), headers={"Authorization": "Bearer " + KEY, "Content-Type": "application/json"})
            with urllib.request.urlopen(slow_req, timeout=15) as slow_response:
                assert slow_response.readline().startswith(b"data: ")
            for _ in range(20):
                _, state = request("/healthz")
                if json.loads(state)["routes"]["qwen-4"]["active"] == 0:
                    break
                time.sleep(.1)
            else:
                raise AssertionError("client cancellation did not release the admission slot")
            # The preceding saturation burst can trip the physical route's
            # cooldown. Wait for its health probe before testing cancellation.
            for _ in range(80):
                _, state = request("/healthz")
                if json.loads(state)["routes"]["headroom-2"]["state"] == "serving":
                    break
                time.sleep(.5)
            else:
                raise AssertionError("Headroom route did not recover after burst")
            slow_body = {"model": "qwen38flash", "messages": [{"role": "user", "content": "slow"}], "max_tokens": 8, "stream": True}
            slow_req = urllib.request.Request(f"http://{HOST}:{PORT}/v1/chat/completions", data=json.dumps(slow_body).encode(), headers={"Authorization": "Bearer " + CANARY_KEY, "Content-Type": "application/json"})
            try:
                slow_response = urllib.request.urlopen(slow_req, timeout=15)
            except urllib.error.HTTPError as error:
                raise AssertionError((error.code, error.read().decode(), request("/healthz")[1])) from error
            with slow_response:
                assert slow_response.readline().startswith(b"data: ")
            for _ in range(20):
                _, state = request("/healthz")
                routes = json.loads(state)["routes"]
                if routes["headroom-2"]["active"] == routes["flash-2"]["active"] == 0:
                    break
                time.sleep(.1)
            else:
                raise AssertionError("Headroom cancellation did not release the shared admission slot")
            before = attempts()
            assert chat("qwen38flash", key=CANARY_KEY, messages=[{"role": "user", "content": "adapter_fail"}])[0] == 502
            after = attempts()
            assert after["headroom-2"] == before["headroom-2"] + 1
            _, state = request("/healthz")
            routes = json.loads(state)["routes"]
            assert routes["headroom-2"]["state"] == "cooling_down"
            assert routes["flash-2"]["state"] == "serving"
            assert chat("qwen38flash", messages=[{"role": "user", "content": "rotate"}])[0] == 200
            Backend.hold_release.clear()
            try:
                with ThreadPoolExecutor(max_workers=4) as held:
                    futures = [held.submit(chat, "direct-2", messages=[{"role": "user", "content": "hold"}]) for _ in range(2)]
                    futures += [held.submit(request, "/v1/responses", {"model": "direct-2", "input": "hold", "max_output_tokens": 8}) for _ in range(2)]
                    for _ in range(50):
                        _, state = request("/healthz")
                        if json.loads(state)["routes"]["flash-2"]["active"] == 4:
                            break
                        time.sleep(.1)
                    else:
                        raise AssertionError("mixed chat and Responses requests did not fill four shared slots")
                    assert request("/v1/responses", {"model": "direct-2", "input": "hold", "max_output_tokens": 8})[0] == 503
                    Backend.hold_release.set()
                    assert all(future.result()[0] == 200 for future in futures)
            finally:
                Backend.hold_release.set()
            _, state = request("/healthz")
            timeout_completions = json.loads(state)["routes"]["partial-9"]["completed_total"]
            _, metrics_before = request("/metrics")
            errors_before = int(next(line.split()[-1] for line in metrics_before.splitlines() if line.startswith("slotline_upstream_errors_total ")))
            timeout_body = {"model": "direct-9", "messages": [{"role": "user", "content": "timeout_stream"}], "max_tokens": 8, "stream": True}
            timeout_req = urllib.request.Request(f"http://{HOST}:{PORT}/v1/chat/completions", data=json.dumps(timeout_body).encode(), headers={"Authorization": "Bearer " + KEY, "Content-Type": "application/json"})
            stream_failed = False
            with urllib.request.urlopen(timeout_req, timeout=15) as timeout_response:
                assert timeout_response.readline().startswith(b"data: ")
                try:
                    timeout_response.read()
                except (ConnectionError, OSError, IncompleteRead):
                    stream_failed = True
            assert stream_failed, "timed-out SSE was presented as a complete response"
            _, state = request("/healthz")
            route = json.loads(state)["routes"]["partial-9"]
            assert route["state"] == "cooling_down" and route["active"] == 0, route
            assert route["completed_total"] == timeout_completions, route
            _, metrics_after = request("/metrics")
            errors_after = int(next(line.split()[-1] for line in metrics_after.splitlines() if line.startswith("slotline_upstream_errors_total ")))
            assert errors_after == errors_before + 1, (errors_before, errors_after)
            _, state = request("/healthz")
            completions_before = json.loads(state)["routes"]["flash-2"]["completed_total"]
            _, metrics_before = request("/metrics")
            errors_before = int(next(line.split()[-1] for line in metrics_before.splitlines() if line.startswith("slotline_upstream_errors_total ")))
            truncated_body = {"model": "direct-2", "messages": [{"role": "user", "content": "truncated"}], "max_tokens": 8, "stream": True}
            truncated_req = urllib.request.Request(f"http://{HOST}:{PORT}/v1/chat/completions", data=json.dumps(truncated_body).encode(), headers={"Authorization": "Bearer " + KEY, "Content-Type": "application/json"})
            stream_failed = False
            with urllib.request.urlopen(truncated_req, timeout=15) as truncated_response:
                assert truncated_response.readline().startswith(b"data: ")
                try:
                    truncated_response.read()
                except (ConnectionError, OSError, IncompleteRead):
                    stream_failed = True
            assert stream_failed, "truncated SSE was presented as a complete response"
            _, state = request("/healthz")
            route = json.loads(state)["routes"]["flash-2"]
            assert route["state"] == "cooling_down" and route["active"] == 0, route
            assert route["completed_total"] == completions_before, route
            _, metrics_after = request("/metrics")
            errors_after = int(next(line.split()[-1] for line in metrics_after.splitlines() if line.startswith("slotline_upstream_errors_total ")))
            assert errors_after == errors_before + 1, (errors_before, errors_after)
            _, state = request("/healthz")
            completions_before = json.loads(state)["routes"]["responses-16"]["completed_total"]
            _, metrics_before = request("/metrics")
            errors_before = int(next(line.split()[-1] for line in metrics_before.splitlines() if line.startswith("slotline_upstream_errors_total ")))
            truncated_responses_body = {"model": "direct-16", "input": "truncated", "max_output_tokens": 8, "stream": True}
            truncated_responses_req = urllib.request.Request(f"http://{HOST}:{PORT}/v1/responses", data=json.dumps(truncated_responses_body).encode(), headers={"Authorization": "Bearer " + KEY, "Content-Type": "application/json"})
            stream_failed = False
            with urllib.request.urlopen(truncated_responses_req, timeout=15) as truncated_response:
                assert truncated_response.readline().startswith(b"event: response.created")
                try:
                    truncated_response.read()
                except (ConnectionError, OSError, IncompleteRead):
                    stream_failed = True
            assert stream_failed, "truncated Responses SSE was presented as a complete response"
            _, state = request("/healthz")
            route = json.loads(state)["routes"]["responses-16"]
            assert route["state"] == "cooling_down" and route["active"] == 0, route
            assert route["completed_total"] == completions_before, route
            _, metrics_after = request("/metrics")
            errors_after = int(next(line.split()[-1] for line in metrics_after.splitlines() if line.startswith("slotline_upstream_errors_total ")))
            assert errors_after == errors_before + 1, (errors_before, errors_after)
            _, state = request("/healthz")
            completions_before = json.loads(state)["routes"]["codex-18"]["completed_total"]
            _, metrics_before = request("/metrics")
            errors_before = int(next(line.split()[-1] for line in metrics_before.splitlines() if line.startswith("slotline_upstream_errors_total ")))
            premature_done_body = {"model": "codex-mock", "input": "premature_done", "max_output_tokens": 8, "stream": True}
            premature_done_req = urllib.request.Request(f"http://{HOST}:{PORT}/v1/responses", data=json.dumps(premature_done_body).encode(), headers={"Authorization": "Bearer " + KEY, "Content-Type": "application/json"})
            stream_failed = False
            with urllib.request.urlopen(premature_done_req, timeout=15) as premature_response:
                data = bytearray()
                try:
                    while b"response.completed" not in data:
                        line = premature_response.readline()
                        assert line, ("response.completed was not forwarded", bytes(data[-300:]))
                        data.extend(line)
                    data.extend(premature_response.read())
                except (ConnectionError, OSError, IncompleteRead):
                    stream_failed = True
            assert stream_failed and b"response.completed" in data, "premature [DONE] was presented as a Codex response.done"
            _, state = request("/healthz")
            route = json.loads(state)["routes"]["codex-18"]
            assert route["state"] == "cooling_down" and route["active"] == 0, route
            assert route["completed_total"] == completions_before, route
            _, metrics_after = request("/metrics")
            errors_after = int(next(line.split()[-1] for line in metrics_after.splitlines() if line.startswith("slotline_upstream_errors_total ")))
            assert errors_after == errors_before + 1, (errors_before, errors_after)
            print(json.dumps({"result": "pass", "statuses": {str(code): statuses.count(code) for code in set(statuses)}, "peak": Backend.peak}, sort_keys=True))
        finally:
            process.terminate()
            process.wait(timeout=5)
    for server in servers:
        server.shutdown()


if __name__ == "__main__":
    main()
