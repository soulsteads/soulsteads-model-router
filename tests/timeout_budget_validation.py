"""Exercise class-scoped request deadlines against an isolated Slotline and slow backend."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from tempfile import TemporaryDirectory
from threading import Thread
import json
import os
import subprocess
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
HOST = os.environ.get("SLOTLINE_TEST_HOST", "100.127.255.254")
PORT = 4013


class Backend(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format, *args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "qwen38flash"}]} if self.path == "/v1/models" else {"ok": True}).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        self.rfile.read(int(self.headers["Content-Length"]))
        time.sleep(1.5)
        body = json.dumps({"model": "qwen38flash", "choices": [{"message": {"content": "ok"}}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except BrokenPipeError:
            pass


def request(key):
    body = json.dumps({"model": "test", "messages": [{"role": "user", "content": "ok"}], "max_tokens": 2}).encode()
    req = urllib.request.Request(f"http://{HOST}:{PORT}/v1/chat/completions", data=body,
                                 headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=8) as response:
            return response.status, json.loads(response.read())
    except urllib.error.HTTPError as error:
        return error.code, json.loads(error.read())


def main():
    backend = ThreadingHTTPServer(("127.0.0.1", 18121), Backend)
    thread = Thread(target=backend.serve_forever, daemon=True)
    thread.start()
    with TemporaryDirectory() as tmp:
        root = Path(tmp)
        for name in ("slow", "regular"):
            key = root / f"{name}.key"
            key.write_text(f"slotline-test-{name}-12345678901234567890")
            key.chmod(0o600)
        config = f"""bind: {HOST}
port: {PORT}
refresh_seconds: 30
class_timeout_budgets:
  long-job: {{attempt_seconds: 3, total_seconds: 4}}
credentials:
  - {{name: slow, key_file: {root}/slow.key, aliases: [test], class: long-job, requests_per_minute: 100, burst: 20, max_in_flight: 2}}
  - {{name: regular, key_file: {root}/regular.key, aliases: [test], class: interactive, requests_per_minute: 100, burst: 20, max_in_flight: 2}}
routes:
  - {{name: local, kind: local, base_url: 'http://127.0.0.1:18121/v1', model: qwen38flash, safe_concurrency: 2, context_limit: 2000, adapter_version: fixture, capabilities: [text], timeout_seconds: 1, enabled: true}}
aliases:
  test: {{context_limit: 2000, capabilities: [text], routes: [{{route: local, priority: 10}}]}}
"""
        path = root / "config.yaml"
        path.write_text(config)
        binary = os.environ.get("SLOTLINE_TEST_BINARY", str(ROOT / "target/release/slotline"))
        check = subprocess.run([binary, "--check-config", str(path)], capture_output=True, text=True)
        assert check.returncode == 0, check.stderr
        for old, bad in (("attempt_seconds: 3", "attempt_seconds: 0"), ("total_seconds: 4", "total_seconds: 2")):
            invalid = root / "invalid.yaml"
            invalid.write_text(config.replace(old, bad))
            result = subprocess.run([binary, "--check-config", str(invalid)], capture_output=True, text=True)
            assert result.returncode != 0, (bad, result.stdout)
        process = subprocess.Popen([binary, str(path)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            for _ in range(100):
                status = subprocess.run([binary, "status", str(path), "--json"], capture_output=True, text=True)
                if status.returncode == 0 and json.loads(status.stdout)["routes"]["local"]["state"] == "serving":
                    break
                time.sleep(0.1)
            else:
                raise AssertionError("fixture did not serve")
            code, data = request((root / "slow.key").read_text())
            assert code == 200 and data["choices"][0]["message"]["content"] == "ok", (code, data)
            before = time.monotonic()
            code, data = request((root / "regular.key").read_text())
            elapsed = time.monotonic() - before
            assert code == 504 and data["error"]["code"] == "upstream_error" and 0.9 < elapsed < 1.4, (code, data, elapsed)
            print("class timeout budget: long-job completed after 1.5s; regular timed out after 1s")
        finally:
            process.terminate()
            process.wait(timeout=10)
    backend.shutdown()


if __name__ == "__main__":
    main()
