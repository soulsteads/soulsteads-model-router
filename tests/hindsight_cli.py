"""Offline CLI regression checks; pass the freshly built slotline binary."""

import ctypes
import json
import os
import platform
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


BINARY = str(Path(sys.argv.pop(1)).resolve())


def deny_network():
    """Kill any socket/connect syscall from process startup (Linux fixtures)."""
    numbers = {"x86_64": (41, 42), "aarch64": (198, 203)}
    if platform.system() != "Linux" or platform.machine() not in numbers:
        raise RuntimeError("network exclusion requires Linux x86_64/aarch64")
    class Filter(ctypes.Structure):
        _fields_ = [("code", ctypes.c_ushort), ("jt", ctypes.c_ubyte),
                    ("jf", ctypes.c_ubyte), ("k", ctypes.c_uint)]
    class Program(ctypes.Structure):
        _fields_ = [("len", ctypes.c_ushort), ("filter", ctypes.POINTER(Filter))]
    socket_call, connect_call = numbers[platform.machine()]
    rules = (Filter * 5)(Filter(0x20, 0, 0, 0),
                         Filter(0x15, 1, 0, socket_call),
                         Filter(0x15, 0, 1, connect_call),
                         Filter(0x06, 0, 0, 0x80000000),
                         Filter(0x06, 0, 0, 0x7fff0000))
    program = Program(len(rules), rules)
    libc = ctypes.CDLL(None, use_errno=True)
    libc.prctl(4, 0, 0, 0, 0)  # no core artifacts from the SIGSYS control
    if libc.prctl(38, 1, 0, 0, 0) != 0 or libc.prctl(22, 2, ctypes.byref(program), 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "seccomp unavailable")


class HindsightCliTests(unittest.TestCase):
    def invoke(self, command, hindsight, offline=False):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "policy.json"
            path.write_text(json.dumps({
                "bind": "100.127.255.254", "port": 14022,
                "routes": [], "aliases": {"synthetic-alias": {
                    "context_limit": 2000, "routes": [],
                    "tokenizer": {"file": "/nonexistent/synthetic-tokenizer", "sha256": "0" * 64},
                }},
                "key_store": "/nonexistent/synthetic-verifier-store",
                "codex": {"enabled": True, "account_file": "/nonexistent/synthetic-provider-secret"},
                "credentials": [{
                    "name": "fixture", "key_file": "/nonexistent/workload-key",
                    "aliases": [], "class": "test",
                }],
                "hindsight": hindsight,
            }))
            return subprocess.run(
                [BINARY, "hindsight", command, str(path)],
                capture_output=True, text=True, timeout=5,
                env={**os.environ, "SLOTLINE_HINDSIGHT_CLI_TEST": "synthetic-secret-sentinel", "RUST_LOG": "off"},
                preexec_fn=deny_network if offline else None,
            )

    def test_status_without_workload_or_upstream_secret_files(self):
        result = self.invoke("status", {
            "enabled": True, "endpoint": "http://127.0.0.1:1",
            "api_key_file": "/nonexistent/private-secret-sentinel",
            "bindings": {"fixture": {"bank_id": "agent-fixture"}},
        })
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["state"], "enabled")
        self.assertEqual(report["health"], "not_checked")
        self.assertEqual(report["bindings_count"], 1)
        self.assertNotIn("sentinel", result.stdout + result.stderr)

    def test_disabled_check_needs_neither_network_nor_secret(self):
        result = self.invoke("check", {
            "enabled": False, "endpoint": "http://127.0.0.1:1",
            "api_key_file": "/nonexistent/private-secret-sentinel",
        })
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["state"], "disabled")
        self.assertTrue(report["ok"])
        self.assertEqual(report["health"], "not_checked")

    def test_env_reference_is_not_printed_or_required(self):
        for name in ["SLOTLINE_HINDSIGHT_CLI_TEST", "SLOTLINE_HINDSIGHT_ABSENT_51885"]:
            result = self.invoke("status", {
                "enabled": True, "endpoint": "http://127.0.0.1:1", "api_key_env": name,
            })
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(json.loads(result.stdout)["secret_reference_configured"])
            self.assertNotIn("sentinel", result.stdout + result.stderr)
            self.assertNotIn(name, result.stdout + result.stderr)

    def test_invalid_configuration_does_not_echo_secret_values(self):
        for config in [
            {"endpoint": "http://user:secret-sentinel@127.0.0.1:1"},
            {"endpoint": "http://127.0.0.1:1?key=secret-sentinel"},
            {"timeout_ms": "secret-sentinel"},
            {"api_key": "secret-sentinel"},
        ]:
            result = self.invoke("status", config)
            self.assertNotEqual(result.returncode, 0)
            self.assertNotIn("sentinel", result.stdout + result.stderr)

    def test_status_and_disabled_check_are_offline_from_exec(self):
        for command, enabled in [("status", True), ("check", False)]:
            result = self.invoke(command, {
                "enabled": enabled, "endpoint": "http://127.0.0.1:1",
                "api_key_file": "/nonexistent/synthetic-secret",
            }, offline=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout)["health"], "not_checked")

    def test_network_exclusion_control(self):
        # Prove the child filter is active rather than crediting a no-op hook.
        result = subprocess.run([sys.executable, "-c", "import socket; socket.socket()"],
                                capture_output=True, preexec_fn=deny_network, timeout=5)
        self.assertEqual(result.returncode, -31)  # SIGSYS

    def test_existing_outcome_evaluator_remains_offline(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "outcomes.json"
            path.write_text(json.dumps({"schema_version": 1, "outcomes": [{
                "scope": "synthetic-scope", "workload": "synthetic-workload",
                "task_family": "synthetic-family", "case_id": "synthetic-case",
                "target_alias": "synthetic-target", "model": {
                    "model_id": "synthetic-model", "model_revision": "v1", "adapter_version": "v1"},
                "outcome": "quality_pass", "evidence_kind": "executable_test",
                "evidence_ref": "synthetic-test", "latency_ms": 10,
            }], "settings": {"minimum_samples": 1, "minimum_success_bps": 1}}))
            result = subprocess.run([BINARY, "routing", "evaluate", str(path)],
                                    capture_output=True, text=True, preexec_fn=deny_network, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads(result.stdout)
            self.assertEqual(report["schema_version"], 1)
            self.assertTrue(report["advisory_only"])
            self.assertEqual(report["groups"][0]["status"], "qualified")

    def test_usage(self):
        result = self.invoke("not-a-command", {})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("usage:", result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
