"""Offline CLI regression checks; pass the freshly built slotline binary."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


BINARY = str(Path(sys.argv.pop(1)).resolve())


class HindsightCliTests(unittest.TestCase):
    def invoke(self, command, hindsight):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "policy.json"
            path.write_text(json.dumps({
                "bind": "100.127.255.254", "port": 14022,
                "routes": [], "aliases": {},
                "credentials": [{
                    "name": "fixture", "key_file": "/nonexistent/workload-key",
                    "aliases": [], "class": "test",
                }],
                "hindsight": hindsight,
            }))
            return subprocess.run(
                [BINARY, "hindsight", command, str(path)],
                capture_output=True, text=True, timeout=5,
                env={"SLOTLINE_HINDSIGHT_CLI_TEST": "synthetic-secret-sentinel", "RUST_LOG": "off"},
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

    def test_usage(self):
        result = self.invoke("not-a-command", {})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("usage:", result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
