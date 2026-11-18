"""Compiled routing HTTP acceptance on an explicitly assigned test listener.

Requires SLOTLINE_TEST_BINARY, SLOTLINE_TEST_HOST and private TMPDIR. Gateway and
backend are always terminated/joined. --manifest records generated named checks.
"""
import argparse
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
from threading import Lock, Thread
import time
import urllib.error
import urllib.request

KEY = "synthetic-routing-http-workload-key-0123456789"
SINGLE_KEY = "synthetic-routing-http-single-key-0123456789"
DENIED_KEY = "synthetic-routing-http-denied-key-0123456789"


class FixtureBlocked(Exception):
    pass


class Backend(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    lock = Lock()
    requests = []
    decision = '{"target":"small"}'

    def log_message(self, *_args):
        pass

    def reply(self, value):
        data = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("x-slotline-routing-mode", "spoofed")
        self.send_header("x-slotline-routing-policy-sha256", "spoofed")
        self.send_header("x-slotline-routing-advisory-sha256", "spoofed")
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/v1/models":
            self.reply({"data": [{"id": "synthetic-" + name} for name in ["small", "large", "judge"]]})
        else:
            self.reply({"ok": True})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if body.get("max_tokens") != 1:
            with self.lock:
                self.requests.append(body)
        is_judge = body.get("max_tokens") == 128
        if self.path.endswith("/responses"):
            self.reply({"object": "response", "model": body["model"], "status": "completed", "output": []})
        else:
            self.reply({"model": body["model"], "choices": [{"finish_reason": "stop", "message": {
                "role": "assistant", "content": self.decision if is_judge else "synthetic answer"}}]})


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", required=True)
    args = parser.parse_args()
    binary = str(Path(os.environ["SLOTLINE_TEST_BINARY"]).resolve())
    host = os.environ.get("SLOTLINE_TEST_HOST")
    checks = []
    process = server = thread = None
    directory = tempfile.mkdtemp(prefix="routing-http-")
    # Retain fixtures and logs for caller-controlled inspection and cleanup.
    root = Path(directory)
    log = None
    try:
        if not host:
            raise FixtureBlocked("SLOTLINE_TEST_HOST is required")
        server = ThreadingHTTPServer(("127.0.0.1", 0), Backend)
        thread = Thread(target=server.serve_forever)
        thread.start()
        with socket.socket() as reservation:
            reservation.bind((host, 0))
            port = reservation.getsockname()[1]
        for name, value in [("workload.key", KEY), ("single.key", SINGLE_KEY), ("denied.key", DENIED_KEY)]:
            path = root / name
            path.write_text(value)
            path.chmod(0o600)
        base_url = f"http://127.0.0.1:{server.server_port}/v1"
        routes = [{"name": name, "kind": "local", "base_url": base_url,
                   "model": "synthetic-" + name, "safe_concurrency": 1,
                   "context_limit": 32768, "adapter_version": "synthetic-v1", "enabled": True,
                   "capabilities": ["text", "responses"], "streaming": True, "disable_thinking": True}
                  for name in ["small", "large", "judge"]]
        aliases = {name: {"context_limit": 32768, "capabilities": ["text", "responses"],
                   "routes": [{"route": name, "priority": 0}]} for name in ["small", "large", "judge"]}
        text_rule = {"id": "Unicode λ text rule", "target": "small", "when": {
                     "credential_classes": ["batch"], "user_text": {"keywords": ["rust"]}}}
        policies = {"smart": {"version": "synthetic-v1", "default_target": "large", "rules": [text_rule]},
                    "shadow": {"mode": "shadow", "default_target": "small", "rules": [dict(text_rule, target="large", when={"user_text": {"keywords": ["rust"]}})],
                               "decision_model": "judge", "decision_targets": ["large"]},
                    "judge-policy": {"decision_model": "judge", "decision_targets": ["small"], "default_target": "large"},
                    "denied": {"mode": "shadow", "default_target": "large"},
                    "fit-policy": {"decision_model": "judge", "decision_targets": ["small"]},
                    "native-policy": {"decision_model": "judge", "decision_targets": ["cloud"]},
                    "byte-policy": {"default_target": "large", "rules": [{"id": "exact-bytes", "target": "small", "when": {"min_input_bytes": 100, "max_input_bytes": 100}}]}}
        allowed = list(aliases) + list(policies) + ["cloud"]
        credentials = [{"name": "synthetic", "key_file": str(root / "workload.key"), "aliases": allowed,
                        "allowed_routes": ["small", "large", "judge", "codex"], "cloud_allowed": True, "class": "batch", "operator": True,
                        "requests_per_minute": 10000, "burst": 10000, "max_in_flight": 1},
                       {"name": "synthetic-single", "key_file": str(root / "single.key"),
                        "aliases": ["shadow", "denied", "small"], "allowed_routes": ["small"],
                        "class": "single", "requests_per_minute": 1, "burst": 1, "max_in_flight": 1},
                       {"name": "synthetic-denied", "key_file": str(root / "denied.key"),
                        "aliases": ["denied", "small"], "allowed_routes": ["small"],
                        "class": "denied", "requests_per_minute": 1, "burst": 1, "max_in_flight": 1}]
        config = {"bind": host, "port": port, "routes": routes, "aliases": aliases,
                  "routing_policies": policies, "credentials": credentials,
                  "codex": {"enabled": True, "alias": "cloud", "model": "synthetic",
                            "account_file": str(root / "absent-synthetic-account.json")},
                  "refresh_seconds": 30, "retry_policy": {"max_attempts": 1, "total_seconds": 5}}
        config_path = root / "policy.json"
        config_path.write_text(json.dumps(config))
        log = (root / "gateway.log").open("w+")
        process = subprocess.Popen([binary, str(config_path)], stdout=log, stderr=subprocess.STDOUT)
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

        def request(path, body=None, key=KEY):
            data = body.encode() if isinstance(body, str) else json.dumps(body).encode() if body is not None else None
            req = urllib.request.Request(f"http://{host}:{port}" + path, data=data,
                headers={"Authorization": "Bearer " + key, "Content-Type": "application/json"})
            try:
                response = opener.open(req, timeout=8)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                return response.status, dict(response.headers.items()), response.read()

        def check(name, condition):
            checks.append({"name": name, "status": "passed" if condition else "failed"})
            if not condition:
                raise AssertionError(name)

        deadline = time.monotonic() + 30
        ready = False
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError("gateway exited before qualification")
            try:
                status, _, body = request("/status")
                ready = status == 200 and all(route["state"] == "serving" for route in json.loads(body)["routes"].values())
                if ready:
                    break
            except (OSError, ValueError):
                pass
            time.sleep(0.1)
        check("qualified synthetic upstreams", ready)
        with Backend.lock:
            Backend.requests.clear()
        status, _, body = request("/v1/models")
        catalog = {entry["id"]: entry for entry in json.loads(body)["data"]}
        check("catalog routing metadata", status == 200 and catalog["smart"]["model_info"]["routing_version"] == "synthetic-v1"
              and catalog["shadow"]["model_info"]["routing_mode"] == "shadow")
        status, _, body = request("/status")
        original_audit = json.loads(body)["audit"]
        status_cli = subprocess.run([binary, "status", str(config_path)], capture_output=True, text=True, timeout=10)
        check("compiled status reader metadata", status_cli.returncode == 0 and "synthetic-v1" in status_cli.stdout
              and original_audit["routing_policies"]["smart"]["policy_sha256"] in status_cli.stdout)
        config["routing_policies"]["smart"]["version"] = "synthetic-v2"
        config_path.write_text(json.dumps(config))
        reload_result = subprocess.run([binary, "reload", str(config_path)], capture_output=True, text=True, timeout=10)
        status, _, body = request("/status")
        check("version reload rejected preserves generation", reload_result.returncode != 0
              and json.loads(body)["audit"]["policy_generation"] == original_audit["policy_generation"])
        config["routing_policies"]["smart"]["version"] = "synthetic-v1"
        config_path.write_text(json.dumps(config))
        for endpoint in ["chat", "responses"]:
            path = "/v1/chat/completions" if endpoint == "chat" else "/v1/responses"
            payload = {"model": "smart", **({"messages": [{"role": "user", "content": "RUST"}]} if endpoint == "chat" else {"input": "RUST"})}
            raw = json.dumps(payload) + " \n"
            status, headers, body = request(path, raw)
            headers = {name.lower(): value for name, value in headers.items()}
            check(endpoint + " text selection", status == 200 and json.loads(body)["model"] == "small")
            check(endpoint + " provenance", headers["x-slotline-routing-mode"] == "active"
                  and headers["x-slotline-routing-version"] == "synthetic-v1"
                  and headers["x-slotline-routing-rule-sha256"] == hashlib.sha256("Unicode λ text rule".encode()).hexdigest()
                  and headers.get("x-slotline-routing-advisory-sha256") is None)
            case = {"schema_version": 1, "id": "synthetic-live-" + endpoint, "endpoint": endpoint,
                    "raw_body": raw, "input_bytes": len(raw.encode()), "grants": {"synthetic": True,
                    "credential_class": "batch", "aliases": allowed,
                    "allowed_routes": ["small", "large", "judge", "codex"], "cloud_allowed": True}}
            case_path = root / "case.json"
            case_path.write_text(json.dumps(case))
            offline = subprocess.run([binary, "routing", "explain", str(config_path), str(case_path)], capture_output=True, text=True, timeout=10)
            explanation = json.loads(offline.stdout)
            check(endpoint + " shared offline/live plan", offline.returncode == 0
                  and explanation["prediction"]["target"] == "small"
                  and explanation["policy_sha256"] == headers["x-slotline-routing-policy-sha256"])
        raw = json.dumps({"model": "byte-policy", "messages": [{"role": "user", "content": "hi"}]})
        for size, target in [(100, "small"), (101, "large")]:
            status, _, body = request("/v1/chat/completions", raw + " " * (size - len(raw)))
            check("inclusive original bytes " + str(size), status == 200 and json.loads(body)["model"] == target)
            case = {"schema_version": 1, "id": "synthetic-byte-" + str(size), "endpoint": "chat",
                    "raw_body": raw + " " * (size - len(raw)), "input_bytes": size,
                    "grants": {"synthetic": True, "credential_class": "batch", "aliases": allowed,
                               "allowed_routes": ["small", "large", "judge", "codex"], "cloud_allowed": True}}
            case_path.write_text(json.dumps(case))
            offline = subprocess.run([binary, "routing", "explain", str(config_path), str(case_path)], capture_output=True, text=True, timeout=10)
            check("shared offline/live byte count " + str(size), offline.returncode == 0
                  and json.loads(offline.stdout)["prediction"]["target"] == target)
        with Backend.lock:
            before = len(Backend.requests)
        status, headers, body = request("/v1/chat/completions", {"model":"denied","messages":[{"role":"user","content":"rust"}]}, DENIED_KEY)
        with Backend.lock:
            sent = Backend.requests[before:]
        check("denied baseline no judge or target", status == 403 and not sent and b"large" not in body)
        status, headers, _ = request("/v1/chat/completions", {"model": "shadow", "messages": [{"role": "user", "content": "rust"}]}, SINGLE_KEY)
        headers = {name.lower(): value for name, value in headers.items()}
        with Backend.lock:
            sent = Backend.requests[before:]
        check("shadow one baseline no judge", status == 200 and len(sent) == 1 and sent[0]["model"] == "synthetic-small")
        check("denied advisory hidden", "x-slotline-routing-advisory-sha256" not in headers)
        status, _, _ = request("/v1/chat/completions", {"model": "shadow", "messages": [{"role": "user", "content": "hi"}]}, SINGLE_KEY)
        check("shadow single rate debit", status == 429)
        with Backend.lock:
            before = len(Backend.requests)
        status, _, _ = request("/v1/chat/completions", {"model": "judge-policy", "messages": [{"role": "user", "content": "hi"}]})
        with Backend.lock:
            sent = Backend.requests[before:]
        check("one candidate still judges", status == 200 and len(sent) == 2 and sent[0]["max_tokens"] == 128)
        for endpoint in ["chat/completions", "responses"]:
            payload = {"model": "native-policy", "extra_body": {"extra_body": {"stream": False}},
                **({"input": "hello"} if endpoint == "responses" else {"messages": [{"role": "user", "content": "hello"}]})}
            with Backend.lock:
                before_native = len(Backend.requests)
            status, _, _ = request("/v1/" + endpoint, payload)
            with Backend.lock:
                sent = Backend.requests[before_native:]
            check("native prefilter before judge " + endpoint, status == 403 and not sent)
        with Backend.lock:
            before_fit = len(Backend.requests)
        status, _, _ = request("/v1/chat/completions", {"model": "fit-policy", "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object"}}}]})
        with Backend.lock:
            sent = Backend.requests[before_fit:]
        check("mandatory local fit before judge", status == 403 and not sent)
        Backend.decision = '{"target":"small","confidence":1}'
        with Backend.lock:
            before = len(Backend.requests)
        status, _, _ = request("/v1/chat/completions", {"model": "judge-policy", "messages": [{"role": "user", "content": "hi"}]})
        with Backend.lock:
            sent = Backend.requests[before:]
        check("target-only error no default rescue", status == 502 and len(sent) == 1 and sent[0]["max_tokens"] == 128)
        for endpoint in ["chat/completions", "responses"]:
            payload = {"model": "small", **({"input": "hi"} if endpoint == "responses" else {"messages": [{"role": "user", "content": "hi"}]})}
            status, headers, _ = request("/v1/" + endpoint, payload)
            check("ordinary upstream cannot spoof " + endpoint, status == 200
                  and not any(name.lower().startswith("x-slotline-routing-") for name in headers))
    except FixtureBlocked as error:
        checks.append({"name": "assigned listener", "status": "blocked", "reason": str(error)})
    except OSError as error:
        checks.append({"name": "native listener", "status": "blocked", "reason": error.__class__.__name__})
    except Exception as error:
        checks.append({"name": "HTTP acceptance", "status": "failed", "reason": str(error)})
    finally:
        if process is not None:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
        if server is not None:
            if thread is not None:
                server.shutdown()
                thread.join(timeout=10)
            server.server_close()
        if log is not None:
            log.close()
        report = {"kind": "routing_http_acceptance", "fixture_directory": str(root), "checks": checks,
                  "counts": {state: sum(item["status"] == state for item in checks)
                             for state in ["passed", "failed", "blocked"]}}
        Path(args.manifest).write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report["counts"]))
    return int(any(item["status"] != "passed" for item in checks))


if __name__ == "__main__":
    raise SystemExit(main())
