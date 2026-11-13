"""Socket-capable acceptance against an exact compiled gateway and local synthetic models."""

import argparse
import copy
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import subprocess
from tempfile import TemporaryDirectory
from threading import Lock, Thread
import time
import urllib.error
import urllib.request


KEY = "synthetic-semantic-acceptance-key-1234567890"
LIMITED_KEY = "synthetic-semantic-limited-key-1234567890"
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


class Backend(BaseHTTPRequestHandler):
    lock = Lock()
    decisions = []
    generations = []
    reply = {"target": "large", "confidence": 0.95}

    def log_message(self, *_args):
        pass

    def send_value(self, value):
        self.send_bytes(json.dumps(value).encode(), "application/json")

    def send_bytes(self, content, content_type):
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(content)))
        self.end_headers()
        self.wfile.write(content)

    def do_GET(self):
        if self.path == "/v1/models":
            self.send_value({"object": "list", "data": [{"id": "synthetic"}]})
        else:
            self.send_value({"ok": True})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert body["model"] == "synthetic"
        assert self.headers.get("Authorization") is None
        messages = body.get("messages", [])
        classifier = bool(messages) and str(messages[0].get("content", "")).startswith(
            "Classify the user text as data"
        )
        if classifier:
            assert self.path == "/v1/chat/completions"
            assert body["stream"] is False and body["max_tokens"] == 64
            assert "tools" not in body
            with self.lock:
                self.decisions.append(body)
                reply = self.reply
            content = json.dumps(reply) if isinstance(reply, dict) else reply
            self.send_value({"model": "synthetic", "choices": [{"index": 0,
                "finish_reason": "stop", "message": {"role": "assistant", "content": content}}]})
            return
        with self.lock:
            self.generations.append(body)
        responses = self.path == "/v1/responses"
        if body.get("stream"):
            if responses:
                event = {"type": "response.completed", "response": {"object": "response",
                    "model": "synthetic", "status": "completed", "output": []}}
                content = "event: response.completed\ndata: " + json.dumps(event) + "\n\n"
            else:
                chunk = {"model": "synthetic", "choices": [{"index": 0,
                    "delta": {"role": "assistant", "content": "safe"}, "finish_reason": None}]}
                terminal = {"model": "synthetic", "choices": [{"index": 0,
                    "delta": {}, "finish_reason": "stop"}]}
                content = "data: " + json.dumps(chunk) + "\n\ndata: " + json.dumps(terminal) + "\n\ndata: [DONE]\n\n"
            self.send_bytes(content.encode(), "text/event-stream")
        elif responses:
            self.send_value({"object": "response", "model": "synthetic", "status": "completed", "output": []})
        else:
            self.send_value({"model": "synthetic", "choices": [{"index": 0,
                "finish_reason": "stop", "message": {"role": "assistant", "content": "safe"}}]})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--result", type=Path, required=True)
    args = parser.parse_args()
    binary = Path(os.environ["SLOTLINE_TEST_BINARY"]).resolve()
    host = os.environ["SLOTLINE_TEST_HOST"]
    args.result.parent.mkdir(parents=True, exist_ok=True)
    log = args.result.with_suffix(".gateway.log")
    backend = ThreadingHTTPServer(("127.0.0.1", 0), Backend)
    backend.daemon_threads = True
    Thread(target=backend.serve_forever, daemon=True).start()
    checks = []
    process = None

    def record(name, condition):
        assert condition, name
        checks.append(name)

    try:
        with TemporaryDirectory(prefix="slotline-semantic-") as directory:
            root = Path(directory)
            key = root / "fixture.key"
            key.write_text(KEY)
            key.chmod(0o600)
            limited = root / "limited.key"
            limited.write_text(LIMITED_KEY)
            limited.chmod(0o600)
            with socket.socket() as reservation:
                reservation.bind((host, 0))
                port = reservation.getsockname()[1]
            capabilities = ["text", "tools", "json_schema", "responses", "responses_json_schema"]
            alias = {"context_limit": 8192, "capabilities": capabilities,
                "routes": [{"route": "native", "priority": 1}]}
            config = {"bind": host, "port": port, "credentials": [
                {"name": "fixture", "key_file": str(key), "class": "test", "max_in_flight": 1,
                    "operator": True, "burst": 2048, "aliases": ["auto", "judge", "small", "large"], "allowed_routes": ["native"]},
                {"name": "limited", "key_file": str(limited), "class": "test", "max_in_flight": 1,
                    "burst": 100, "aliases": ["auto", "judge", "small"], "allowed_routes": ["native"]}],
                "routes": [{"name": "native", "kind": "local",
                    "base_url": f"http://127.0.0.1:{backend.server_port}/v1", "model": "synthetic",
                    "safe_concurrency": 1, "context_limit": 8192, "adapter_version": "mock-v1",
                    "capabilities": capabilities, "enabled": True}],
                "aliases": {name: alias for name in ["small", "large", "judge"]},
                "semantic_routing": {"enabled": True, "max_in_flight": 1, "profiles": {"auto": {
                    "targets": ["small", "large"], "default_target": "small",
                    "rules": [{"id": "code", "priority": 10, "target": "large", "keywords": ["debug"]}],
                    "decision": {"classifier": "judge", "confidence_threshold": 0.8,
                        "timeout_ms": 2000, "max_output_tokens": 64, "on_failure": "default"}}}}}
            config["copilot"] = {"enabled": True, "auth_source": "oauth_token_file",
                "oauth_token_file": str(root / "absent-synthetic-oauth"), "models": [
                    {"alias": "cloud-chat", "model": "synthetic-cloud", "endpoint": "chat_completions"},
                    {"alias": "cloud-responses", "model": "synthetic-cloud", "endpoint": "responses"}]}
            config["credentials"][0]["cloud_allowed"] = True
            config["credentials"][0]["allowed_routes"].append("copilot")
            config["credentials"][0]["aliases"].extend(["cloud-chat", "cloud-responses"])
            config["aliases"]["no-stream"] = copy.deepcopy(alias)
            config["aliases"]["no-stream"]["streaming"] = False
            for selector in ["shadow", "preflight"]:
                profile = copy.deepcopy(config["semantic_routing"]["profiles"]["auto"])
                profile["policy_version"] = "acceptance-v1"
                profile["compatibility_preflight"] = True
                if selector == "shadow":
                    profile["mode"] = "shadow"
                    config["credentials"][1]["aliases"].append(selector)
                else:
                    profile["targets"] = ["small", "no-stream"]
                    profile["rules"][0]["target"] = "no-stream"
                config["semantic_routing"]["profiles"][selector] = profile
                config["credentials"][0]["aliases"].append(selector)
            config["credentials"][0]["aliases"].append("no-stream")
            for endpoint in ["chat", "responses"]:
                for branch in ["rule", "default", "model", "fallback"]:
                    selector = f"auto-cloud-{endpoint}-{branch}"
                    target = f"cloud-{endpoint}"
                    profile = {"targets": [target], "default_target": target, "rules": []}
                    if branch == "rule":
                        profile["rules"] = [{"id": "cloud-rule", "priority": 1,
                            "target": target, "keywords": ["hello"]}]
                    elif branch in ["model", "fallback"]:
                        profile["decision"] = {"classifier": "judge", "confidence_threshold": 0.8,
                            "timeout_ms": 2000, "max_output_tokens": 64, "on_failure": "default"}
                    config["semantic_routing"]["profiles"][selector] = profile
                    config["credentials"][0]["aliases"].append(selector)
            policy = root / "policy.json"
            policy.write_text(json.dumps(config))
            checked = subprocess.run([str(binary), "--check-config", str(policy)], capture_output=True, timeout=15)
            record("compiled_cli_config_validation", checked.returncode == 0)

            def request(path, body=None, token=KEY):
                headers = {"Content-Type": "application/json"}
                if token is not None:
                    headers["Authorization"] = "Bearer " + token
                req = urllib.request.Request(f"http://{host}:{port}{path}",
                    data=None if body is None else json.dumps(body).encode(), headers=headers)
                try:
                    response = OPENER.open(req, timeout=10)
                except urllib.error.HTTPError as error:
                    response = error
                with response:
                    return response.status, response.headers, response.read()

            with log.open("wb") as output:
                process = subprocess.Popen([str(binary), str(policy)], stdout=output, stderr=subprocess.STDOUT)
                deadline = time.monotonic() + 15
                ready = False
                while time.monotonic() < deadline:
                    if process.poll() is not None:
                        raise AssertionError("gateway exited before readiness; see gateway log")
                    try:
                        ready = request("/v1/chat/completions", {"model": "small",
                            "messages": [{"role": "user", "content": "readiness"}], "max_tokens": 16})[0] == 200
                    except (OSError, urllib.error.URLError):
                        pass
                    if ready:
                        break
                    time.sleep(0.05)
                record("qualified_mock_backend", ready)
                status, _, body = request("/v1/models")
                record("selector_discovery", status == 200 and any(item["id"] == "auto" for item in json.loads(body)["data"]))
                status, _, body = request("/status")
                snapshot = json.loads(body)
                record("semantic_operator_status", status == 200
                    and snapshot["semantic_routing"]["enabled"] is True
                    and snapshot["semantic_routing"]["selectors"] == sorted(config["semantic_routing"]["profiles"])
                    and snapshot["semantic_routing"]["profiles"]["shadow"]["mode"] == "shadow"
                    and snapshot["semantic_routing"]["profiles"]["shadow"]["policy_version"] == "acceptance-v1"
                    and snapshot["semantic_routing"]["decision_admission"] == {"limit": 1, "in_flight": 0, "available": 1, "closed": False}
                    and snapshot["credentials"]["limited"]["semantic_selectors"] == ["auto", "shadow"]
                    and "auto" not in snapshot["credentials"]["limited"]["aliases"]
                    and list(snapshot["routes"]) == ["native"])
                record("semantic_status_operator_only", request("/status", token=LIMITED_KEY)[0] == 403)
                plain_status = subprocess.run([str(binary), "status", str(policy)], capture_output=True, text=True, timeout=15)
                record("semantic_plain_status_cli", plain_status.returncode == 0
                    and "Semantic routing: enabled" in plain_status.stdout
                    and "Decision admission: 0/1 in flight, 1 available, open" in plain_status.stdout
                    and "Selector grants: auto" in plain_status.stdout
                    and "small -> native" in plain_status.stdout
                    and "auto ->" not in plain_status.stdout and KEY not in plain_status.stdout)
                for endpoint in ["chat", "responses"]:
                    path = "/v1/responses" if endpoint == "responses" else "/v1/chat/completions"
                    for branch in ["rule", "default", "model", "fallback"]:
                        selector = f"auto-cloud-{endpoint}-{branch}"
                        with Backend.lock:
                            Backend.reply = {"target": f"cloud-{endpoint}" if branch == "model" else "large", "confidence": 0.95}
                        payload = {"model": selector, "input": "hello", "store": False} if endpoint == "responses" else {
                            "model": selector, "messages": [{"role": "user", "content": "hello"}], "max_tokens": 16}
                        for field, denial in [("cloud_allowed", False), ("local_only", True)]:
                            for nested in [False, True]:
                                restricted = {**payload, **({"extra_body": {field: denial}} if nested else {field: denial})}
                                with Backend.lock:
                                    before = (len(Backend.decisions), len(Backend.generations))
                                status, _, body = request(path, restricted)
                                with Backend.lock:
                                    after = (len(Backend.decisions), len(Backend.generations))
                                record(f"request_policy_rejected_{endpoint}_{branch}_{field}_extra_{nested}", status == 400
                                    and json.loads(body)["error"]["code"] == "unsupported_parameter"
                                    and json.loads(body)["error"]["message"] == f"Unsupported semantic request policy argument: {field}"
                                    and before == after)
                        if endpoint == "responses":
                            unsupported_items = [
                                {"id": "msg_previous"},
                                {"id": "msg_previous", "role": "user", "content": "hello"},
                                {"type": "item_reference", "id": "msg_previous"},
                                {"type": "message", "role": "assistant"},
                                {"type": None, "role": "user", "content": "hello"},
                                {"role": None, "content": "hello"},
                                {"role": "assistant", "content": None},
                            ]
                            for index, item in enumerate(unsupported_items):
                                for nested in [False, True]:
                                    replacement = [item, {"role": "user", "content": "hello"}]
                                    restricted = {**payload, **({"extra_body": {"input": replacement}} if nested else {"input": replacement})}
                                    if nested:
                                        del restricted["input"]
                                    with Backend.lock:
                                        before = (len(Backend.decisions), len(Backend.generations))
                                    status, _, body = request(path, restricted)
                                    with Backend.lock:
                                        after = (len(Backend.decisions), len(Backend.generations))
                                    record(f"responses_complete_boundary_{branch}_{index}_extra_{nested}", status == 400
                                        and json.loads(body)["error"]["code"] == "unsupported_continuation" and before == after)
                            for typed in [False, True]:
                                message = {"role": "user", "content": "hello"}
                                if typed:
                                    message.update(type="message", id="msg_self_contained", status="completed")
                                status, headers, body = request(path, {**payload, "input": [message]})
                                record(f"responses_complete_control_{branch}_typed_{typed}", status == 502
                                    and json.loads(body)["error"]["code"] == "upstream_auth_error"
                                    and headers["x-slotline-semantic-target"] == "cloud-responses"
                                    and headers["x-slotline-semantic-outcome"] == ("invalid" if branch == "fallback" else branch))
                        with Backend.lock:
                            before = (len(Backend.decisions), len(Backend.generations))
                        status, headers, body = request(path, payload)
                        with Backend.lock:
                            after = (len(Backend.decisions), len(Backend.generations))
                        record(f"cloud_target_fake_auth_control_{endpoint}_{branch}", status == 502
                            and json.loads(body)["error"]["code"] == "upstream_auth_error"
                            and headers["x-slotline-semantic-target"] == f"cloud-{endpoint}"
                            and headers["x-slotline-semantic-outcome"] == ("invalid" if branch == "fallback" else branch)
                            and after == (before[0] + (1 if branch in ["model", "fallback"] else 0), before[1]))
                with Backend.lock:
                    Backend.reply = {"target": "large", "confidence": 0.95}
                original = {"model": "auto", "messages": [{"role": "system", "content": "debug is system data"},
                    {"role": "user", "content": "hello"}], "max_tokens": 16, "user": "synthetic-preserved", "top_k": 5,
                    "tools": [{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object"}}}],
                    "response_format": {"type": "json_schema", "json_schema": {"name": "result", "schema": {"type": "object"}}}}
                status, headers, body = request("/v1/chat/completions", original)
                record("real_model_selection", status == 200 and headers["x-slotline-semantic-target"] == "large"
                    and headers["x-slotline-semantic-outcome"] == "model" and json.loads(body)["model"] == "large")
                forwarded = {**original, "model": "synthetic"}
                with Backend.lock:
                    preserved = forwarded in Backend.generations
                    count = len(Backend.decisions)
                record("chat_tools_schema_request_preserved", preserved)
                for model in ["small", "auto"]:
                    rejected = {**original, "model": model, "metadata": {"acceptance": "unsupported"}}
                    with Backend.lock:
                        generations = len(Backend.generations)
                    status, _, body = request("/v1/chat/completions", rejected)
                    record(f"metadata_rejected_{model}", status == 400
                        and json.loads(body)["error"]["code"] == "unsupported_parameter"
                        and json.loads(body)["error"]["message"] == "Unsupported model argument: metadata"
                        and len(Backend.generations) == generations)
                with Backend.lock:
                    count = len(Backend.decisions)
                rule = {"model": "auto", "messages": [{"role": "user", "content": "DEBUG"}], "max_tokens": 16}
                status, headers, _ = request("/v1/chat/completions", rule)
                record("rules_precede_model", status == 200 and headers["x-slotline-semantic-rule"] == "code"
                    and len(Backend.decisions) == count)
                explicit = {**rule, "model": "small"}
                status, headers, _ = request("/v1/chat/completions", explicit)
                record("ordinary_alias_no_classifier", status == 200 and headers.get("x-slotline-semantic-profile") is None
                    and len(Backend.decisions) == count)
                with Backend.lock:
                    Backend.reply = "PRIVATE_CLASSIFIER_SENTINEL"
                status, headers, body = request("/v1/chat/completions", original)
                record("invalid_output_authorized_fallback", status == 200 and headers["x-slotline-semantic-target"] == "small"
                    and headers["x-slotline-semantic-outcome"] == "invalid" and b"PRIVATE_CLASSIFIER_SENTINEL" not in body)
                with Backend.lock:
                    Backend.reply = {"target": "large", "confidence": 1}
                status, headers, _ = request("/v1/chat/completions", original, LIMITED_KEY)
                with Backend.lock:
                    prompt = json.loads(Backend.decisions[-1]["messages"][1]["content"])
                record("forbidden_candidate_not_visible_or_selectable", status == 200
                    and headers["x-slotline-semantic-target"] == "small" and prompt["candidates"] == ["small"])
                for responses in [False, True]:
                    for stream in [False, True]:
                        payload = {"model": "auto", "input": "hello", "max_output_tokens": 16} if responses else {
                            "model": "auto", "messages": [{"role": "user", "content": "hello"}], "max_tokens": 16}
                        payload["stream"] = stream
                        status, headers, content = request("/v1/responses" if responses else "/v1/chat/completions", payload)
                        record(f"generation_responses_{responses}_stream_{stream}", status == 200
                            and headers["x-slotline-semantic-target"] == "large" and (
                                (b"response.completed" if responses else b"[DONE]") in content if stream else json.loads(content)["model"] == "large"))
                count = len(Backend.decisions)
                record("auth_before_classifier", request("/v1/chat/completions", original, None)[0] == 401)
                record("embedding_selector_rejected", request("/v1/embeddings", {"model": "auto", "input": "hello"})[0] == 400)
                record("hidden_stateful_continuation_rejected", request("/v1/responses", {"model": "auto", "input": "hello",
                    "extra_body": {"previous_response_id": "synthetic-private-state"}})[0] == 400)
                record("denials_have_no_classifier_calls", len(Backend.decisions) == count)
                for responses in [False, True]:
                    path = "/v1/responses" if responses else "/v1/chat/completions"
                    for stream in [False, True]:
                        for text, token, outcome in [("debug", KEY, "rule"), ("hello", KEY, "classifier_not_run"), ("debug", LIMITED_KEY, "denied")]:
                            payload = {"model": "shadow", "stream": stream}
                            payload.update({"input": text, "max_output_tokens": 16} if responses else {
                                "messages": [{"role": "user", "content": text}], "max_tokens": 16})
                            before = (len(Backend.decisions), len(Backend.generations))
                            status, headers, body = request(path, payload, token)
                            suffix = f"{responses}_{stream}_{outcome}"
                            record("shadow_baseline_only_" + suffix, status == 200
                                and headers["x-slotline-semantic-target"] == "small"
                                and headers["x-slotline-semantic-mode"] == "shadow"
                                and headers["x-slotline-semantic-classifier"] == "not_run"
                                and headers["x-slotline-semantic-advisory-outcome"] == outcome
                                and len(Backend.decisions) == before[0] and len(Backend.generations) == before[1] + 1)
                            record("shadow_safe_advisory_" + suffix,
                                headers.get("x-slotline-semantic-advisory-target") == ("large" if outcome == "rule" else None)
                                and headers.get("x-slotline-semantic-rule") is None)
                            payload["model"] = "small"
                            baseline_status, baseline_headers, baseline_body = request(path, payload, token)
                            record("shadow_identity_" + suffix, status == baseline_status and body == baseline_body
                                and headers.get("Content-Type") == baseline_headers.get("Content-Type")
                                and baseline_headers.get("x-slotline-semantic-mode") is None)
                before = (len(Backend.decisions), len(Backend.generations))
                status, _, _ = request("/v1/chat/completions", {"model": "preflight",
                    "messages": [{"role": "user", "content": "debug"}], "extra_body": {"stream": True}})
                record("preflight_matched_rule_fails_closed_before_work", status == 400
                    and before == (len(Backend.decisions), len(Backend.generations)))
                status, _, _ = request("/v1/responses", {"model": "shadow", "input": [
                    {"type": "item_reference", "id": "synthetic"}, {"role": "user", "content": "hello"}]})
                record("shadow_history_denied_before_work", status == 400
                    and before == (len(Backend.decisions), len(Backend.generations)))
                record("unique_check_names", len(checks) == len(set(checks)))
                assert len(checks) == 165, f"declared 165 checks, collected {len(checks)}"
        result = {"status": "passed", "binary": str(binary), "checks": checks, "count": len(checks), "declared_count": 165,
            "boundaries": "synthetic local HTTP only; no real model quality or live providers exercised", "gateway_log": str(log)}
        args.result.write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result, indent=2))
    except Exception as error:
        args.result.write_text(json.dumps({"status": "failed", "checks": checks, "error_type": type(error).__name__,
            "gateway_log": str(log)}, indent=2) + "\n")
        raise
    finally:
        if process is not None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
        backend.shutdown()
        backend.server_close()


if __name__ == "__main__":
    main()
