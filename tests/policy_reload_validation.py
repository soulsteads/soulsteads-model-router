"""Isolated HTTP proof for the restrictive local policy reload gate."""

from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from tempfile import TemporaryDirectory
from threading import Event, Thread
import json
import os
import subprocess
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
BIN = Path(os.environ.get("SLOTLINE_TEST_BINARY", str(ROOT / "target/release/slotline")))
HOST = os.environ.get("SLOTLINE_TEST_HOST", "100.127.255.254")
PORT = 4014
BACKEND_PORT = 18121
KEY = "reload-test-operator-key-1234567890"
NEW_KEY = "changed-operator-key-1234567890"


class Backend(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    release = Event()
    streaming = Event()
    embedding_started = Event()
    embedding_release = Event()

    def log_message(self, *_args):
        pass

    def send_json(self, value):
        data = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/v1/models":
            self.send_json({"object": "list", "data": [{"id": "qwen38"}, {"id": "qwen3-embedding:0.6b"}]})
        elif self.path == "/health":
            self.send_json({"ok": True})
        else:
            self.send_error(404)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path == "/v1/embeddings":
            assert body["model"] == "qwen3-embedding:0.6b"
            if body["input"] == "hold":
                self.embedding_started.set()
                self.embedding_release.wait(20)
            self.send_json({"object": "list", "model": body["model"], "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2]}]})
            return
        if self.path == "/v1/responses":
            assert body["model"] == "qwen38" and body["stream"]
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            self.close_connection = True
            try:
                self.wfile.write(b'event: response.created\ndata: {"type":"response.created","response":{"object":"response","model":"qwen38"}}\n\n')
                self.wfile.flush()
                self.streaming.set()
                self.release.wait(20)
                self.wfile.write(b'event: response.completed\ndata: {"type":"response.completed","response":{"object":"response","model":"qwen38","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"OK"}]}]}}\n\n')
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass
            return
        assert self.path == "/v1/chat/completions" and body["model"] == "qwen38"
        if body.get("stream"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Connection", "close")
            self.end_headers()
            self.close_connection = True
            try:
                self.wfile.write(b'data: {"id":"old","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"old"},"finish_reason":null}]}\n\n')
                self.wfile.flush()
                self.streaming.set()
                self.release.wait(20)
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass
        else:
            self.send_json({"id": "mock", "object": "chat.completion", "model": "qwen38", "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}]})


def request(path, body=None, key=KEY, timeout=5):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(
        f"http://{HOST}:{PORT}{path}", data=data,
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def reload(path, good=True):
    result = subprocess.run([str(BIN), "reload", str(path)], capture_output=True, text=True, timeout=35)
    assert (result.returncode == 0) == good, (result.returncode, result.stderr)


def chat(stream=False):
    return {"model": "qwen38", "messages": [{"role": "user", "content": "test"}], "max_tokens": 8, "stream": stream}


def catalog_limit():
    status, data = request("/v1/models")
    assert status == 200
    return next(item["context_length"] for item in json.loads(data)["data"] if item["id"] == "qwen38")


def main():
    backend = ThreadingHTTPServer(("127.0.0.1", BACKEND_PORT), Backend)
    thread = Thread(target=backend.serve_forever, daemon=True)
    thread.start()
    with TemporaryDirectory() as temp:
        root = Path(temp)
        key = root / "operator.key"
        key.write_text(KEY)
        key.chmod(0o600)
        path = root / "policy.yaml"
        source = f"""
bind: {HOST}
port: {PORT}
refresh_seconds: 30
credentials:
  - {{name: operator, key_file: {key}, aliases: [qwen38, qwen3-embedding], class: test, requests_per_minute: 10000, burst: 100, max_in_flight: 10, operator: true}}
routes:
  - {{name: qwen-4, kind: local, base_url: 'http://127.0.0.1:{BACKEND_PORT}/v1', model: qwen38, safe_concurrency: 1, context_limit: 200000, adapter_version: mock-v1, capabilities: [text, responses], enabled: true}}
  - {{name: embedding-local, kind: embedding, base_url: 'http://127.0.0.1:{BACKEND_PORT}/v1', model: 'qwen3-embedding:0.6b', safe_concurrency: 1, context_limit: 32768, adapter_version: mock-v1, capabilities: [text], streaming: false, enabled: true}}
aliases:
  qwen38: {{context_limit: 200000, capabilities: [text, responses], routes: [{{route: qwen-4, priority: 10}}]}}
  qwen3-embedding: {{endpoint: embedding, context_limit: 32768, capabilities: [text], streaming: false, routes: [{{route: embedding-local, priority: 10}}]}}
"""
        path.write_text(source)
        process = subprocess.Popen([str(BIN), str(path)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            for _ in range(100):
                try:
                    status, data = request("/healthz")
                    if status == 200 and all(item["state"] == "serving" for item in json.loads(data)["routes"].values()):
                        break
                except urllib.error.URLError:
                    pass
                time.sleep(.1)
            else:
                raise AssertionError("reload fixture route did not serve")
            assert catalog_limit() == 200000
            stream_request = urllib.request.Request(
                f"http://{HOST}:{PORT}/v1/chat/completions", data=json.dumps(chat(True)).encode(),
                headers={"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"},
            )
            stream = urllib.request.urlopen(stream_request, timeout=5)
            assert stream.status == 200 and b"old" in stream.readline()
            assert Backend.streaming.wait(2)
            path.write_text(source.replace("context_limit: 200000, capabilities", "context_limit: 100000, capabilities"))
            reload(path)
            assert catalog_limit() == 100000
            assert request("/v1/chat/completions", chat())[0] == 503
            Backend.release.set()
            assert b"[DONE]" in stream.read()
            stream.close()
            for _ in range(30):
                if request("/v1/chat/completions", chat())[0] == 200:
                    break
                time.sleep(.1)
            else:
                raise AssertionError("stream permit did not release")

            Backend.release.clear()
            Backend.streaming.clear()
            cancelled = urllib.request.urlopen(stream_request, timeout=5)
            assert cancelled.status == 200 and b"old" in cancelled.readline()
            assert Backend.streaming.wait(2)
            assert request("/v1/chat/completions", chat())[0] == 503
            cancelled.close()
            for _ in range(30):
                if request("/v1/chat/completions", chat())[0] == 200:
                    break
                time.sleep(.1)
            else:
                raise AssertionError("cancelled stream permit did not release")
            Backend.release.set()

            Backend.release.clear()
            Backend.streaming.clear()
            response_request = urllib.request.Request(
                f"http://{HOST}:{PORT}/v1/responses",
                data=json.dumps({"model": "qwen38", "input": "test", "max_output_tokens": 8, "stream": True}).encode(),
                headers={"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"},
            )
            response_stream = urllib.request.urlopen(response_request, timeout=5)
            assert response_stream.status == 200 and b"response.created" in response_stream.readline()
            assert Backend.streaming.wait(2)
            path.write_text(source.replace("context_limit: 200000, capabilities", "context_limit: 80000, capabilities"))
            reload(path)
            assert catalog_limit() == 80000
            assert request("/v1/chat/completions", chat())[0] == 503
            Backend.release.set()
            assert b"response.completed" in response_stream.read()
            response_stream.close()
            assert request("/v1/chat/completions", chat())[0] == 200

            with ThreadPoolExecutor(max_workers=1) as pool:
                held_embedding = pool.submit(request, "/v1/embeddings", {"model": "qwen3-embedding", "input": "hold"})
                assert Backend.embedding_started.wait(2)
                path.write_text(source.replace("context_limit: 200000, capabilities", "context_limit: 70000, capabilities"))
                reload(path)
                assert catalog_limit() == 70000
                assert request("/v1/embeddings", {"model": "qwen3-embedding", "input": "test"})[0] == 503
                Backend.embedding_release.set()
                assert held_embedding.result(timeout=5)[0] == 200
                assert request("/v1/embeddings", {"model": "qwen3-embedding", "input": "test"})[0] == 200

            health_before = json.loads(request("/healthz")[1])["routes"]["qwen-4"]["state"]
            path.write_text(source.replace("safe_concurrency: 1", "safe_concurrency: 2"))
            reload(path, good=False)
            assert catalog_limit() == 70000
            assert json.loads(request("/healthz")[1])["routes"]["qwen-4"]["state"] == health_before
            path.write_text(source.replace("model: qwen38", "model: gpt-6-luna"))
            reload(path, good=False)
            assert catalog_limit() == 70000

            path.write_text(source.replace("context_limit: 200000, capabilities", "context_limit: 90000, capabilities"))
            with ThreadPoolExecutor(max_workers=6) as pool:
                assert all(pool.map(lambda _: subprocess.run([str(BIN), "reload", str(path)], capture_output=True).returncode == 0, range(6)))
            assert catalog_limit() == 90000
            key.write_text(NEW_KEY)
            reload(path, good=False)
            assert request("/v1/models")[0] == 200
            assert request("/v1/models", key=NEW_KEY)[0] == 401
            key.write_text(KEY)
            assert request("/v1/chat/completions", chat())[0] == 200
            print("policy reload validation passed")
        finally:
            Backend.release.set()
            Backend.embedding_release.set()
            process.terminate()
            process.wait(timeout=5)
    backend.shutdown()
    backend.server_close()


if __name__ == "__main__":
    main()
