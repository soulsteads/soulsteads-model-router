"""Exercise verifier-backed ingress without touching the shadow service."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from tempfile import TemporaryDirectory
from threading import Thread
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen
import json
import os
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
BIN = Path(os.environ.get("SLOTLINE_TEST_BINARY", str(ROOT / "target/release/slotline")))
HOST = os.environ.get("SLOTLINE_TEST_HOST", "100.127.255.254")
PORT = 4013


class Backend(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    requests = 0

    def log_message(self, *_args):
        pass

    def reply(self, body, status=200):
        payload = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        if self.path == "/v1/models":
            self.reply({"data": [{"id": "qwen38flash"}]})
        elif self.path == "/health":
            self.reply({"ok": True})
        else:
            self.reply({"error": "not found"}, 404)

    def do_POST(self):
        assert self.path == "/v1/chat/completions"
        assert "Authorization" not in self.headers
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert body["model"] == "qwen38flash"
        self.__class__.requests += 1
        self.reply({"model": "qwen38flash", "choices": [{"message": {"content": "OK"}}]})


def call(path, key=None, body=None):
    headers = {}
    if key:
        headers["Authorization"] = "Bearer " + key
    data = None
    if body is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(body).encode()
    req = Request(f"http://{HOST}:{PORT}{path}", data=data, headers=headers)
    try:
        with urlopen(req, timeout=10) as response:
            return response.status, response.read().decode()
    except HTTPError as error:
        return error.code, error.read().decode()


def wait_status(path, key, expected, seconds=8):
    for _ in range(seconds * 10):
        try:
            result = call(path, key)[0]
            if result == expected:
                return
        except URLError:
            pass
        time.sleep(.1)
    raise AssertionError(f"{path} did not reach HTTP {expected}")


def key_command(*args):
    return subprocess.run(
        [str(BIN), "keys", *map(str, args)],
        check=True, capture_output=True, text=True,
    ).stdout.strip()


def policy(path, identity):
    path.write_text(json.dumps({
        "id": identity,
        "aliases": ["qwen38flash"],
        "class": identity,
        "requests_per_minute": 10000,
        "burst": 100,
        "max_in_flight": 8,
        "operator": True,
    }))


def start(config):
    process = subprocess.Popen([str(BIN), str(config)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return process


def main():
    backend = ThreadingHTTPServer(("127.0.0.1", 18118), Backend)
    thread = Thread(target=backend.serve_forever, daemon=True)
    thread.start()
    try:
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            root.chmod(0o700)
            private = root / "private"
            private.mkdir(mode=0o700)
            shared = root / "shared"
            shared.mkdir(mode=0o775)
            store = private / "keys.sqlite3"
            config = shared / "slotline.yaml"
            first_policy = root / "first-policy.json"
            second_policy = root / "second-policy.json"
            policy(first_policy, "first-client")
            policy(second_policy, "second-client")
            key_command("init", store)
            first = key_command("issue", store, first_policy)
            config.write_text(f"""bind: {HOST}
port: {PORT}
refresh_seconds: 30
key_store: {store}
routes:
  - {{name: flash, kind: local, base_url: 'http://127.0.0.1:18118/v1', model: qwen38flash, safe_concurrency: 2, context_limit: 8192, adapter_version: mock-v1, capabilities: [text], enabled: true}}
aliases:
  qwen38flash: {{context_limit: 8192, capabilities: [text], routes: [{{route: flash, priority: 10}}]}}
""")
            config.chmod(0o600)
            assert subprocess.run([str(BIN), "--check-config", str(config)], capture_output=True).returncode == 0
            process = start(config)
            try:
                wait_status("/v1/models", first, 200)
                socket = Path(str(store) + ".control.sock")
                assert socket.exists() and socket.stat().st_mode & 0o777 == 0o600
                policy_socket = Path(str(store) + ".policy.sock")
                assert policy_socket.exists() and policy_socket.stat().st_mode & 0o777 == 0o600
                assert not Path(str(config) + ".policy.sock").exists()
                assert json.loads(call("/v1/models", first)[1])["data"][0]["context_length"] == 8192
                source = config.read_text()
                updated = source.replace("qwen38flash: {context_limit: 8192", "qwen38flash: {context_limit: 4096")
                assert updated != source
                config.write_text(updated)
                assert subprocess.run([str(BIN), "reload", str(config)], capture_output=True).returncode == 0
                assert json.loads(call("/v1/models", first)[1])["data"][0]["context_length"] == 4096
                config.write_text(updated.replace("safe_concurrency: 2", "safe_concurrency: 3"))
                assert subprocess.run([str(BIN), "reload", str(config)], capture_output=True).returncode != 0
                assert json.loads(call("/v1/models", first)[1])["data"][0]["context_length"] == 4096
                config.write_text(updated)
                assert call("/v1/models", "wrong")[0] == 401
                assert call("/v1/models")[0] == 401
                before = Backend.requests
                assert call("/v1/models", "wrong")[0] == 401
                assert Backend.requests == before
                body = {"model": "qwen38flash", "messages": [{"role": "user", "content": "OK"}], "max_tokens": 8}
                status, response = call("/v1/chat/completions", first, body)
                assert status == 200 and json.loads(response)["model"] == "qwen38flash"

                replacement = key_command("rotate", store, "first-client", "0")
                assert call("/v1/models", first)[0] == 401
                assert call("/v1/models", replacement)[0] == 200
                key_command("revoke-key", store, replacement.split("_")[1])
                assert call("/v1/models", replacement)[0] == 401
                assert subprocess.run([str(BIN), "--check-config", str(config)], capture_output=True).returncode != 0

                second = key_command("issue", store, second_policy)
                # New policy and its independent class/rate state become
                # usable as soon as the CLI reload acknowledgment returns.
                assert call("/v1/models", second)[0] == 200
                assert call("/v1/chat/completions", second, body)[0] == 200

                invalid_policy = root / "invalid-policy.json"
                policy(invalid_policy, "invalid-client")
                bad = json.loads(invalid_policy.read_text())
                bad["aliases"] = ["unknown-model"]
                invalid_policy.write_text(json.dumps(bad))
                rejected = subprocess.run(
                    [str(BIN), "keys", "issue", str(store), str(invalid_policy)],
                    capture_output=True, text=True,
                )
                assert rejected.returncode != 0
                assert call("/v1/models", second)[0] == 401
                key_command("revoke-credential", store, "invalid-client")
                assert call("/v1/models", second)[0] == 200
            finally:
                process.terminate()
                process.wait(timeout=5)

            process = start(config)
            try:
                wait_status("/v1/models", second, 200)
                # SIGTERM leaves a stale socket; the next startup must safely
                # reclaim it before accepting administrator notifications.
                assert socket.exists()
                socket.unlink()
                rejected = subprocess.run(
                    [str(BIN), "keys", "rotate", str(store), "second-client", "600"],
                    capture_output=True, text=True,
                )
                assert rejected.returncode != 0 and "slk_" in rejected.stdout
                socket.write_text("not a socket")
                rejected = subprocess.run(
                    [str(BIN), "keys", "revoke-key", str(store), rejected.stdout.strip().split("_")[1]],
                    capture_output=True, text=True,
                )
                assert rejected.returncode != 0
                socket.unlink()
                store.write_bytes(b"not a SQLite store")
                wait_status("/v1/models", second, 401)
                assert process.poll() is None
            finally:
                process.terminate()
                process.wait(timeout=5)
            print(json.dumps({"result": "pass", "verifier_auth": True, "rotation_reload": True, "revocation_reload": True, "new_policy_reload": True, "invalid_policy_fail_closed": True, "corrupt_store_fail_closed": True}))
    finally:
        backend.shutdown()


if __name__ == "__main__":
    main()
