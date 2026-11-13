"""Compiled gateway acceptance with disposable HTTP services and synthetic keys.

Set SLOTLINE_TEST_BINARY, SLOTLINE_TEST_HOST (an assigned Tailnet IPv4), and a
private, short TMPDIR. Run sequentially. Fixtures/logs are retained below TMPDIR
for inspection; the runner prints their root. No live services are used.
"""

from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from copy import deepcopy
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from threading import Event, Lock, Thread
import json
import os
import socket
import subprocess
import tempfile
import time
import unittest


BINARY = str(Path(os.environ["SLOTLINE_TEST_BINARY"]).resolve())
HOST = os.environ["SLOTLINE_TEST_HOST"]
WORKLOAD = "synthetic-memory-workload-key-0123456789"
CALLBACK = "synthetic-memory-callback-key-0123456789"
UPSTREAM = "synthetic-memory-upstream-key-0123456789"
OPERATOR = "synthetic-memory-operator-key-0123456789"


def request(address, path, value=None, key=WORKLOAD, headers=None, raw=None):
    connection = HTTPConnection(*address, timeout=8)
    data = raw if raw is not None else None if value is None else json.dumps(value).encode()
    fields = {"Authorization": "Bearer " + key, "Content-Type": "application/json"}
    fields.update(headers or {})
    try:
        connection.request("GET" if data is None else "POST", path, data, fields)
        response = connection.getresponse()
        return response.status, response.read(), dict(response.getheaders())
    finally:
        connection.close()


def chat(model="target"):
    return {"model": model, "messages": [{"role": "user", "content": "synthetic task"}],
            "max_tokens": 16, "stream": False}


class Backend(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def reply(self, value, status=200, headers=None, chunked=False):
        data = value if isinstance(value, bytes) else json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("X-Upstream-Private", "synthetic-private-header")
        for key, val in (headers or {}).items():
            self.send_header(key, val)
        self.send_header("Transfer-Encoding" if chunked else "Content-Length",
                         "chunked" if chunked else str(len(data)))
        self.end_headers()
        try:
            if chunked:
                for offset in range(0, len(data), 65536):
                    chunk = data[offset:offset + 65536]
                    self.wfile.write(f"{len(chunk):x}\r\n".encode() + chunk + b"\r\n")
                self.wfile.write(b"0\r\n\r\n")
            else:
                self.wfile.write(data)
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_GET(self):
        with self.server.lock:
            self.server.gets.append((self.path, {key.lower(): value
                                               for key, value in self.headers.items()}))
        if self.path == "/v1/models":
            return self.reply({"data": [{"id": "qwen-fixture"}]})
        if self.path == "/health":
            return self.reply({"ok": True})
        if self.path == "/v1/default/banks/synthetic-bank/stats":
            return self.reply({}, self.server.stats_status)
        self.reply({}, 404)

    def do_POST(self):
        value = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        with self.server.lock:
            self.server.calls.append((self.path, dict(self.headers), value))
        if self.path == "/v1/chat/completions":
            judge = value.get("max_tokens") == 128
            content = '{"target":"target"}' if judge else "synthetic answer"
            return self.reply({"model": "qwen-fixture", "choices": [{
                "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 701 if judge else 11,
                          "completion_tokens": 103 if judge else 7,
                          "total_tokens": 804 if judge else 18}})
        if not self.path.startswith("/v1/default/banks/synthetic-bank/"):
            return self.reply({}, 404)
        query = value.get("query")
        if query == "hold":
            self.server.arrived.set()
            self.server.release.wait(8)
        elif query == "slow":
            self.server.arrived.set()
            time.sleep(2)
        elif query == "redirect":
            return self.reply({}, 307, {"Location": self.server.origin + "/redirect-target"})
        elif query == "oversize":
            return self.reply(b'{"results":[],"padding":"' + b'x' * (2 * 1024 * 1024) + b'"}', chunked=True)
        elif query == "malformed":
            return self.reply({"error": "synthetic-private-error"})
        elif query == "callback":
            code, data, headers = request(self.server.gateway, "/v1/chat/completions",
                                          chat("memory-policy"), CALLBACK)
            self.server.callback = (code, json.loads(data), headers)
            if code != 200:
                return self.reply({}, 502)
            return self.reply({"text": json.loads(data)["choices"][0]["message"]["content"]})
        if self.path.endswith("/memories"):
            return self.reply({"success": True, "bank_id": "synthetic-bank", "items_count": 1,
                               "async": value.get("async", False)})
        if self.path.endswith("/reflect"):
            return self.reply({"text": "synthetic memory"})
        self.reply({"results": [{"text": "synthetic memory"}]})


class HindsightHttpTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.root = Path(tempfile.mkdtemp(prefix="hm-"))
        cls.root.chmod(0o700)
        print("ARTIFACT_ROOT=" + str(cls.root), flush=True)

    @contextmanager
    def gateway(self, edit=lambda _config: None):
        directory = Path(tempfile.mkdtemp(prefix="g-", dir=self.root))
        directory.chmod(0o700)
        server = ThreadingHTTPServer(("127.0.0.1", 0), Backend)
        server.daemon_threads = True
        server.calls, server.gets, server.lock = [], [], Lock()
        server.arrived, server.release = Event(), Event()
        server.stats_status, server.callback = 200, None
        server.origin = f"http://127.0.0.1:{server.server_port}"
        Thread(target=server.serve_forever, daemon=True).start()
        with socket.socket() as allocator:
            allocator.bind((HOST, 0))
            port = allocator.getsockname()[1]
        config = {"bind": HOST, "port": port, "refresh_seconds": 3600,
                  "credentials": [], "routes": [{
                      "name": "local", "kind": "local", "base_url": server.origin + "/v1",
                      "model": "qwen-fixture", "safe_concurrency": 1, "context_limit": 2000,
                      "adapter_version": "synthetic-v1", "capabilities": ["text"],
                      "streaming": False, "timeout_seconds": 3, "enabled": True}],
                  "aliases": {}, "routing_policies": {"memory-policy": {
                      "decision_model": "judge", "decision_targets": ["target"],
                      "decision_timeout_seconds": 1}},
                  "hindsight": {"enabled": True, "endpoint": server.origin,
                                "api_key_file": str(directory / "upstream"), "timeout_ms": 800,
                                "bindings": {"workload": {"bank_id": "synthetic-bank",
                                                          "allow_retain": True}}}}
        for name in ["target", "judge"]:
            config["aliases"][name] = {"context_limit": 2000, "capabilities": ["text"],
                                       "streaming": False,
                                       "routes": [{"route": "local", "priority": 0}]}
        for name, key in [("workload", WORKLOAD), ("callback", CALLBACK), ("operator", OPERATOR)]:
            path = directory / name
            path.write_text(key)
            path.chmod(0o600)
            config["credentials"].append({
                "name": name, "key_file": str(path), "class": "same-class",
                "max_in_flight": 1, "burst": 1000, "requests_per_minute": 600,
                "aliases": ["target", "judge", "memory-policy"],
                "allowed_routes": ["local"], "operator": name == "operator"})
        (directory / "upstream").write_text(UPSTREAM)
        (directory / "upstream").chmod(0o600)
        edit(config)
        path = directory / "p.json"
        path.write_text(json.dumps(config))
        log_path = directory / "gateway.log"
        log = log_path.open("w")
        process = None
        server.gateway = (HOST, port)
        try:
            check = subprocess.run([BINARY, "--check-config", str(path)], capture_output=True,
                                   text=True, timeout=10)
            self.assertEqual(check.returncode, 0, check.stderr)
            process = subprocess.Popen([BINARY, str(path)], stdout=log, stderr=log,
                                       env={**os.environ, "RUST_LOG": "slotline=info"})
            for _ in range(150):
                self.assertIsNone(process.poll(), log_path.read_text())
                status = subprocess.run([BINARY, "status", str(path), "--json"],
                                        capture_output=True, text=True, timeout=3)
                if status.returncode == 0:
                    report = json.loads(status.stdout)
                    if all(route["state"] == "serving" for route in report["routes"].values()):
                        break
                time.sleep(.05)
            else:
                self.fail("fixture did not qualify: " + log_path.read_text())
            with server.lock:
                server.calls.clear()
            yield server, config, path, log_path
        finally:
            server.release.set()
            if process is not None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
            log.close()
            server.shutdown()
            server.server_close()

    def memory(self, server, operation="recall", value=None, **kwargs):
        return request(server.gateway, "/v1/memory/" + operation,
                       {"query": "synthetic"} if value is None else value, **kwargs)

    def reload(self, path, config, success):
        path.write_text(json.dumps(config))
        result = subprocess.run([BINARY, "reload", str(path)], capture_output=True,
                                text=True, timeout=10)
        self.assertEqual(result.returncode == 0, success, result.stderr)

    def identity(self, server):
        code, data, _ = request(server.gateway, "/status", key=OPERATOR)
        self.assertEqual(code, 200)
        return json.loads(data)["audit"]

    def test_same_class_virtual_callback_and_judge_accounting(self):
        with self.gateway() as (server, _config, _path, log):
            self.assertEqual(self.memory(server, "reflect", {"query": "callback"})[0], 200)
            self.assertEqual(server.callback[0], 200)
            self.assertEqual(server.callback[1]["model"], "target")
            calls = server.calls
            self.assertEqual([item[0] for item in calls], [
                "/v1/default/banks/synthetic-bank/reflect",
                "/v1/chat/completions", "/v1/chat/completions"])
            self.assertEqual([item[2]["max_tokens"] for item in calls[1:]], [128, 16])
            request_id = server.callback[2]["x-request-id"]
            for _ in range(50):
                events = [json.loads(line) for line in log.read_text().splitlines() if line.startswith("{")]
                terminals = [event["fields"] for event in events
                             if event.get("span", {}).get("request_id") == request_id
                             and event["fields"].get("message") == "request body completed"]
                if terminals:
                    break
                time.sleep(.02)
            self.assertEqual(len(terminals), 1)
            self.assertEqual([terminals[0][key] for key in
                              ["attempts", "retries", "input_tokens", "output_tokens", "total_tokens"]],
                             [1, 0, 11, 7, 18])
            self.assertEqual(self.memory(server, key=CALLBACK)[0], 403)
            self.assertEqual(self.memory(server, key=OPERATOR)[0], 403)
            self.assertEqual(len(server.calls), 3)

    def test_callback_alias_judge_and_target_grants(self):
        for missing in ["memory-policy", "judge", "target"]:
            with self.subTest(missing=missing):
                def edit(config):
                    config["credentials"][1]["aliases"].remove(missing)
                with self.gateway(edit) as (server, _config, _path, _log):
                    self.assertEqual(request(server.gateway, "/v1/chat/completions",
                                             chat("memory-policy"), CALLBACK)[0], 403)
                    self.assertEqual(server.calls, [])

    def test_protocol_paths_header_isolation_and_retain(self):
        with self.gateway() as (server, _config, _path, _log):
            headers = {"Cookie": "synthetic-cookie", "X-Bank-ID": "other-bank",
                       "X-Forwarded-Host": "synthetic.invalid", "X-API-Key": "synthetic-private"}
            for operation, value, suffix in [
                ("recall", {"query": "synthetic"}, "memories/recall"),
                ("reflect", {"query": "synthetic"}, "reflect"),
                ("retain", {"items": [{"content": "synthetic memory"}], "async": True}, "memories")]:
                code, _data, response_headers = self.memory(server, operation, value, headers=headers)
                self.assertEqual(code, 200)
                self.assertEqual(response_headers["cache-control"], "no-store")
                self.assertNotIn("x-upstream-private", response_headers)
                path, outgoing, body = server.calls[-1]
                self.assertEqual(path, "/v1/default/banks/synthetic-bank/" + suffix)
                outgoing = {key.lower(): value for key, value in outgoing.items()}
                self.assertEqual(outgoing["authorization"], "Bearer " + UPSTREAM)
                for key in headers:
                    self.assertNotIn(key.lower(), outgoing)
                self.assertEqual(body, value)

    def test_bank_path_body_and_stream_overrides_are_denied(self):
        with self.gateway() as (server, _config, _path, _log):
            for field in ["bank", "bank_id", "tenant", "endpoint", "headers", "extra_body",
                          "api_key", "stream", "stream_options"]:
                value = {"query": "synthetic", field: False if field == "stream" else "other"}
                self.assertEqual(self.memory(server, value=value)[0], 400, field)
            self.assertEqual(request(server.gateway, "/v1/memory/recall?bank_id=other", {})[0], 400)
            self.assertEqual(self.memory(server, headers={"Accept": "text/event-stream"})[0], 400)
            self.assertEqual(request(server.gateway, "/v1/memory/other-bank/recall", {})[0], 404)
            self.assertEqual(server.calls, [])

    def test_retain_requires_explicit_permission(self):
        def edit(config):
            config["hindsight"]["bindings"]["workload"]["allow_retain"] = False
        with self.gateway(edit) as (server, _config, _path, _log):
            self.assertEqual(self.memory(server, "retain", {"items": [{"content": "synthetic"}]})[0], 403)
            self.assertEqual(server.calls, [])
            self.assertEqual(self.memory(server)[0], 200)

    def test_disabled_and_omitted_memory_leave_inference_unchanged(self):
        for omitted in [False, True]:
            def edit(config):
                if omitted:
                    del config["hindsight"]
                else:
                    config["hindsight"]["enabled"] = False
                    config["hindsight"]["api_key_file"] = "/nonexistent/synthetic-secret"
            with self.gateway(edit) as (server, _config, _path, _log):
                self.assertEqual(self.memory(server)[0], 404)
                code, data, _ = request(server.gateway, "/v1/chat/completions", chat())
                self.assertEqual(code, 200)
                self.assertEqual(json.loads(data)["choices"][0]["message"]["content"], "synthetic answer")
                self.assertEqual([item[0] for item in server.calls], ["/v1/chat/completions"])

    def test_missing_memory_secret_never_blocks_inference(self):
        def edit(config):
            config["hindsight"]["api_key_file"] = "/nonexistent/synthetic-secret"
        with self.gateway(edit) as (server, _config, _path, log):
            self.assertEqual(self.memory(server)[0], 503)
            self.assertEqual(request(server.gateway, "/v1/chat/completions", chat())[0], 200)
            self.assertEqual(len(server.calls), 1)
            self.assertNotIn("/nonexistent/synthetic-secret", log.read_text())

    def test_redirect_response_cap_malformed_and_request_cap(self):
        with self.gateway() as (server, _config, _path, _log):
            for query in ["redirect", "oversize", "malformed"]:
                code, data, _ = self.memory(server, value={"query": query})
                self.assertEqual(code, 502, query)
                self.assertNotIn(b"synthetic-private", data)
            self.assertEqual(len(server.calls), 3, "no redirects or retries")
            self.assertEqual(self.memory(server, raw=b"x" * (256 * 1024 + 1))[0], 413)
            self.assertEqual(len(server.calls), 3)

    def test_timeout_upload_cancellation_and_admission_recovery(self):
        with self.gateway() as (server, _config, _path, _log):
            with ThreadPoolExecutor(max_workers=1) as pool:
                pending = pool.submit(self.memory, server, value={"query": "slow"})
                self.assertTrue(server.arrived.wait(3))
                self.assertEqual(self.memory(server)[0], 429)
                self.assertEqual(request(server.gateway, "/v1/chat/completions", chat())[0], 200)
                self.assertEqual(pending.result(timeout=4)[0], 504)
            self.assertEqual(self.memory(server)[0], 200)
            # A partial upload holds memory admission and never reaches upstream.
            for cancel in [False, True]:
                connection = HTTPConnection(*server.gateway, timeout=4)
                connection.putrequest("POST", "/v1/memory/recall")
                connection.putheader("Authorization", "Bearer " + WORKLOAD)
                connection.putheader("Content-Type", "application/json")
                connection.putheader("Content-Length", "100")
                connection.endheaders(b'{"query":')
                try:
                    for _ in range(50):
                        code = self.memory(server)[0]
                        if code == 429:
                            break
                        time.sleep(.01)
                    self.assertEqual(code, 429)
                    self.assertEqual(request(server.gateway, "/v1/chat/completions", chat())[0], 200)
                    if cancel:
                        connection.close()
                    else:
                        response = connection.getresponse()
                        self.assertEqual(response.status, 504)
                        response.read()
                finally:
                    connection.close()
                # HTTP disconnect may retain the handler until its fixed deadline.
                for _ in range(100):
                    code = self.memory(server)[0]
                    if code != 429:
                        break
                    time.sleep(.02)
                self.assertEqual(code, 200)

    def test_reload_identity_atomic_rejection_and_preserved_admission(self):
        def edit(config):
            config["hindsight"]["timeout_ms"] = 6000
        with self.gateway(edit) as (server, config, path, _log):
            before = self.identity(server)
            with ThreadPoolExecutor(max_workers=1) as pool:
                pending = pool.submit(self.memory, server, value={"query": "hold"})
                self.assertTrue(server.arrived.wait(3))
                config["aliases"]["target"]["context_limit"] = 1900
                self.reload(path, config, True)
                accepted = self.identity(server)
                self.assertEqual(accepted["policy_generation"], before["policy_generation"] + 1)
                self.assertNotEqual(accepted["policy_sha256"], before["policy_sha256"])
                self.assertEqual(self.memory(server)[0], 429)
                for field, value in [("enabled", False), ("timeout_ms", 5000),
                                     ("api_key_file", "/nonexistent/synthetic-other"),
                                     ("bindings", {"callback": {"bank_id": "synthetic-other"}})]:
                    candidate = deepcopy(config)
                    candidate["hindsight"][field] = value
                    candidate["aliases"]["target"]["context_limit"] = 1800
                    self.reload(path, candidate, False)
                    current = self.identity(server)
                    self.assertEqual(current["policy_generation"], accepted["policy_generation"])
                    self.assertEqual(current["policy_sha256"], accepted["policy_sha256"])
                path.write_text(json.dumps(config))
                server.release.set()
                self.assertEqual(pending.result(timeout=4)[0], 200)
            self.assertEqual(self.memory(server)[0], 200)

    def test_check_contacts_only_configured_health_and_stats(self):
        with self.gateway() as (server, _config, path, _log):
            server.stats_status = 403
            with server.lock:
                server.gets.clear()
            result = subprocess.run([BINARY, "hindsight", "check", str(path)],
                                    capture_output=True, text=True, timeout=5)
            self.assertNotEqual(result.returncode, 0)
            report = json.loads(result.stdout)
            self.assertEqual(report["health"], "reachable")
            self.assertEqual([path for path, _headers in server.gets],
                             ["/health", "/v1/default/banks/synthetic-bank/stats"])
            self.assertNotIn("authorization", server.gets[0][1])
            self.assertEqual(server.gets[1][1]["authorization"], "Bearer " + UPSTREAM)
            self.assertEqual(report["banks"], {"synthetic-bank": "unauthorized"})
            self.assertFalse(report["ok"])
            self.assertEqual(server.calls, [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
