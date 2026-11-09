"""Exercise Slotline stream failures through the installed Rust Headroom sidecar."""

from http.client import IncompleteRead
from pathlib import Path
from tempfile import TemporaryDirectory
from threading import Thread
import json
import os
import subprocess
import time
import urllib.error
import urllib.request

from mock_validation import Backend


ROOT = Path(__file__).resolve().parents[1]
SLOTLINE = Path(os.environ.get("SLOTLINE_TEST_BINARY", str(ROOT / "target/release/slotline")))
HEADROOM = Path(os.environ.get("SLOTLINE_HEADROOM_PROXY", str(Path.home() / ".local/bin/slotline-headroom-proxy")))
HOST = os.environ.get("SLOTLINE_TEST_HOST", "100.127.255.254")
PORT = 4013
KEY = "slotline-headroom-failure-fixture-1234567890"


def fetch(path, body=None):
    request = urllib.request.Request(
        f"http://{HOST}:{PORT}{path}",
        data=json.dumps(body).encode() if body is not None else None,
        headers={"Authorization": "Bearer " + KEY, "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=12) as response:
        return response.status, response.read()


def wait_serving():
    for _ in range(100):
        try:
            _, data = fetch("/healthz")
            routes = json.loads(data)["routes"]
            if all(routes[name]["state"] == "serving" for name in ("physical-26", "adapter-28")):
                return routes
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(0.1)
    raise AssertionError("isolated Slotline routes did not become serving")


def chat(model, content, stream=False):
    return {"model": model, "messages": [{"role": "user", "content": content}], "max_tokens": 8, "stream": stream}


def check_case(case, config):
    process = subprocess.Popen([str(SLOTLINE), str(config)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        before = wait_serving()
        initial = before["adapter-28"]["completed_total"]
        _, metrics = fetch("/metrics")
        errors_before = int(next(line.split()[-1] for line in metrics.decode().splitlines() if line.startswith("slotline_upstream_errors_total ")))
        request = urllib.request.Request(
            f"http://{HOST}:{PORT}/v1/chat/completions",
            data=json.dumps(chat("headroom-test", case, True)).encode(),
            headers={"Authorization": "Bearer " + KEY, "Content-Type": "application/json"},
        )
        stream_failed = False
        with urllib.request.urlopen(request, timeout=12) as response:
            assert response.status == 200
            first = response.readline()
            assert first.startswith(b"data: "), first
            assert json.loads(first[6:])["model"] == "headroom-test", first
            if case == "timeout_stream":
                _, data = fetch("/healthz")
                active = json.loads(data)["routes"]
                assert active["adapter-28"]["active"] == active["physical-26"]["active"] == 1, active
            try:
                rest = response.read()
                stream_failed = b"data: [DONE]" not in rest
            except (ConnectionError, OSError, IncompleteRead):
                stream_failed = True
        assert stream_failed, f"{case} was presented as a complete SSE response"
        _, data = fetch("/healthz")
        routes = json.loads(data)["routes"]
        adapter = routes["adapter-28"]
        physical = routes["physical-26"]
        assert adapter["state"] == "cooling_down" and adapter["active"] == 0, adapter
        assert physical["state"] == "serving" and physical["active"] == 0, physical
        assert adapter["completed_total"] == initial, adapter
        _, metrics = fetch("/metrics")
        errors_after = int(next(line.split()[-1] for line in metrics.decode().splitlines() if line.startswith("slotline_upstream_errors_total ")))
        assert errors_after == errors_before + 1, (errors_before, errors_after)
        assert fetch("/v1/chat/completions", chat("direct-test", "rotate"))[0] == 200
        return adapter["attempts_total"]
    finally:
        process.terminate()
        process.wait(timeout=5)


def main():
    from http.server import ThreadingHTTPServer

    backend = ThreadingHTTPServer(("127.0.0.1", 18126), Backend)
    backend.daemon_threads = True
    thread = Thread(target=backend.serve_forever, daemon=True)
    thread.start()
    sidecar = subprocess.Popen(
        [str(HEADROOM), "--listen", "127.0.0.1:18128", "--upstream", "http://127.0.0.1:18126",
         "--compression-mode", "off", "--upstream-connect-timeout", "1s", "--upstream-timeout", "3s"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        for _ in range(100):
            try:
                with urllib.request.urlopen("http://127.0.0.1:18128/healthz", timeout=1) as response:
                    if response.status == 200:
                        break
            except (OSError, urllib.error.URLError):
                pass
            time.sleep(0.1)
        else:
            raise AssertionError("Headroom sidecar did not start")
        with TemporaryDirectory() as temp:
            root = Path(temp)
            key_file = root / "operator.key"
            key_file.write_text(KEY)
            key_file.chmod(0o600)
            config = root / "config.yaml"
            config.write_text(f"""
bind: {HOST}
port: {PORT}
refresh_seconds: 30
credentials:
  - {{name: operator, key_file: {key_file}, aliases: [headroom-test, direct-test], class: test, requests_per_minute: 1000, burst: 100, max_in_flight: 10, operator: true}}
routes:
  - {{name: physical-26, kind: local, base_url: 'http://127.0.0.1:18126/v1', model: qwen38flash, safe_concurrency: 2, context_limit: 262144, adapter_version: fixture, capabilities: [text], enabled: true}}
  - {{name: adapter-28, kind: headroom, physical_route: physical-26, base_url: 'http://127.0.0.1:18128/v1', model: qwen38flash, health_path: /healthz, safe_concurrency: 2, context_limit: 262144, adapter_version: fixture, capabilities: [text], timeout_seconds: 7, enabled: true}}
aliases:
  headroom-test: {{context_limit: 262144, capabilities: [text], routes: [{{route: adapter-28, priority: 1}}]}}
  direct-test: {{context_limit: 262144, capabilities: [text], routes: [{{route: physical-26, priority: 1}}]}}
""")
            assert subprocess.run([str(SLOTLINE), "--check-config", str(config)], capture_output=True).returncode == 0
            results = {case: check_case(case, config) for case in ("truncated", "timeout_stream")}
            print(json.dumps({"result": "pass", "sidecar": "real", "cases": results}, sort_keys=True))
    finally:
        sidecar.terminate()
        try:
            sidecar.wait(timeout=5)
        except subprocess.TimeoutExpired:
            sidecar.kill()
            sidecar.wait(timeout=5)
        backend.shutdown()


if __name__ == "__main__":
    main()
