"""Offline regression checks against an explicitly supplied compiled gateway."""

import copy
import ctypes
import json
import os
import platform
import resource
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


BINARY = str(Path(sys.argv.pop(1)).resolve())
FIXTURES = Path(__file__).parent / "fixtures"


def deny_network():
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    class Instruction(ctypes.Structure):
        _fields_ = [("code", ctypes.c_ushort), ("yes", ctypes.c_ubyte),
            ("no", ctypes.c_ubyte), ("value", ctypes.c_uint)]

    class Program(ctypes.Structure):
        _fields_ = [("length", ctypes.c_ushort), ("instructions", ctypes.POINTER(Instruction))]

    instructions = [Instruction(0x20, 0, 0, 4), Instruction(0x15, 1, 0, 0xC000003E),
        Instruction(0x06, 0, 0, 0x80000000), Instruction(0x20, 0, 0, 0)]
    for syscall in [41, 42, 43, 44, 45, 46, 47, 49, 50, 51, 52, 53, 54, 55, 288, 299, 307, 425]:
        instructions.extend([Instruction(0x15, 0, 1, syscall), Instruction(0x06, 0, 0, 0x80000000)])
    instructions.append(Instruction(0x06, 0, 0, 0x7FFF0000))
    array = (Instruction * len(instructions))(*instructions)
    program = Program(len(instructions), array)
    library = ctypes.CDLL(None, use_errno=True)
    if library.prctl(38, 1, 0, 0, 0) != 0 or library.prctl(22, 2, ctypes.byref(program), 0, 0) != 0:
        raise RuntimeError("network-denying seccomp unavailable")


class SemanticOfflineTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="semantic-offline-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.dataset = json.loads((FIXTURES / "semantic-v1.json").read_text())

    def invoke(self, command, value, *options, policy=None, raw=False):
        path = self.root / "input.json"
        path.write_text(value if raw else json.dumps(value))
        return subprocess.run([BINARY, "semantic", command,
            str(policy or FIXTURES / "semantic-policy-v1.yaml"), str(path), *options],
            capture_output=True, text=True, timeout=15,
            env=dict(os.environ, HOME=str(self.root / "nonexistent-home"),
                HTTP_PROXY="http://127.0.0.1:9", HTTPS_PROXY="http://127.0.0.1:9"))

    def test_all_checked_in_labels_metrics_and_declared_counts(self):
        result = self.invoke("evaluate", self.dataset, "--require-match")
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        output = json.loads(result.stdout)
        aggregate = output["aggregate"]
        self.assertEqual(aggregate["total"], self.dataset["declared_cases"])
        self.assertEqual(aggregate["matched"], len(self.dataset["cases"]))
        self.assertEqual(aggregate["unauthorized_selections"], 0)
        self.assertEqual(aggregate["route_selection_accuracy"], 1)
        self.assertLess(aggregate["route_selection_coverage"], 1)
        self.assertEqual(sum(sum(row.values()) for row in aggregate["confusion"].values()), aggregate["total"])
        self.assertEqual(sum(aggregate["outcomes"].values()), aggregate["total"])

    def test_repair_v3_nested_codex_explain_and_evaluate(self):
        source = (FIXTURES / "semantic-policy-v1.yaml").read_text()
        source = source.replace("targets: [small, large]", "targets: [small, cloud]")
        source = source.replace("target: large", "target: cloud")
        source += "\ncodex:\n  enabled: true\n  alias: cloud\n  model: synthetic\n"
        policy = self.root / "codex-projection.yaml"
        for default in [False, True]:
            policy.write_text(source.replace("default_target: small", "default_target: cloud") if default else source)
            cases = []
            for endpoint in ["chat", "responses"]:
                for payload in [{}, {"stream": False}]:
                    request = {"model": "preflight", "extra_body": {"extra_body": payload}}
                    request.update({"input": "hello"} if endpoint == "responses" else
                        {"messages": [{"role": "user", "content": "hello"}]})
                    for branch in ["unrecorded", "untrusted", "rule"]:
                        case = {"schema_version": 1, "id": f"nested-{len(cases)}",
                            "profile": "preflight", "endpoint": endpoint,
                            "grants": {"aliases": ["preflight", "small", "cloud", "judge"],
                                "allowed_routes": ["native", "codex"], "cloud_allowed": True},
                            "request": copy.deepcopy(request)}
                        if branch == "untrusted":
                            case["recorded_classifier"] = {"choices": [{"finish_reason": "stop",
                                "message": {"role": "assistant", "content": '{"target":"cloud","confidence":0.95}'}}]}
                        if branch == "rule":
                            if endpoint == "responses":
                                case["request"]["input"] = "debug code"
                            else:
                                case["request"]["messages"][0]["content"] = "debug code"
                        outcome = "incompatible" if default or branch == "rule" else (
                            "invalid" if branch == "untrusted" else "classifier_not_run")
                        state = "unresolved" if outcome == "classifier_not_run" else "rejected"
                        if outcome == "invalid":
                            state = "resolved"
                        target = "small" if outcome == "invalid" else None
                        case["expected"] = {"state": state, "outcome": outcome, "target": target}
                        result = self.invoke("explain", case, policy=policy)
                        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                        output = json.loads(result.stdout)
                        self.assertEqual(output["compatibility"]["cloud"], "incompatible")
                        self.assertTrue(output["matched"], output)
                        self.assertNotIn("cloud", output["prediction"]["candidates"])
                        cases.append(case)
            result = self.invoke("evaluate", {"schema_version": 1, "declared_cases": len(cases),
                "cases": cases}, "--require-match", policy=policy)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(json.loads(result.stdout)["aggregate"]["matched"], len(cases))

    def test_unresolved_is_not_default_and_output_is_deterministic_nonsecret(self):
        case = copy.deepcopy(self.dataset["cases"][5])
        case["request"]["messages"][0]["content"] = "PRIVATE_PROMPT_SENTINEL"
        first = self.invoke("explain", case)
        second = self.invoke("explain", case)
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(first.stdout, second.stdout)
        output = json.loads(first.stdout)
        self.assertEqual(output["prediction"]["state"], "unresolved")
        self.assertIsNone(output["prediction"]["target"])
        self.assertEqual(output["prediction"]["outcome"], "classifier_not_run")
        evaluated = self.invoke("evaluate", self.dataset)
        for private in ["PRIVATE_PROMPT_SENTINEL", "PRIVATE_RAW_OUTPUT", "nonexistent-slotline-keys", str(self.root), "http://127.0.0.1"]:
            self.assertNotIn(private, first.stdout + first.stderr + evaluated.stdout + evaluated.stderr)

    def test_strict_schema_and_dataset_errors_are_sanitized(self):
        edits = [lambda data: data.update(schema_version=2), lambda data: data.update(declared_cases=999),
            lambda data: data.update(cases=[]), lambda data: data.update(PRIVATE_UNKNOWN="PRIVATE_VALUE"),
            lambda data: data["cases"].append(copy.deepcopy(data["cases"][0])),
            lambda data: data["cases"][0].update(schema_version=99),
            lambda data: data["cases"][0]["grants"].update(aliases=["auto", "auto"]),
            lambda data: data["cases"][0]["grants"].update(aliases=["unknown"]),
            lambda data: data["cases"][0].pop("expected"),
            lambda data: data["cases"][0].update(PRIVATE_UNKNOWN="PRIVATE_VALUE")]
        for edit in edits:
            with self.subTest(edit=edit):
                data = copy.deepcopy(self.dataset)
                edit(data)
                if len(data["cases"]) != self.dataset["declared_cases"]:
                    data["declared_cases"] = len(data["cases"])
                result = self.invoke("evaluate", data)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("error", json.loads(result.stdout))
                self.assertNotIn("PRIVATE_", result.stdout + result.stderr)

    def test_ci_score_threshold_and_failed_case_are_reported(self):
        self.dataset["cases"][0]["expected"]["target"] = "small"
        for option, expected_status in [("1", 1), ("0.9", 0)]:
            result = self.invoke("evaluate", self.dataset, "--min-score", option)
            self.assertEqual(result.returncode, expected_status)
            self.assertEqual(json.loads(result.stdout)["aggregate"]["mismatched"], 1)
        for score in ["NaN", "inf", "-1", "1.1"]:
            self.assertNotEqual(self.invoke("evaluate", self.dataset, "--min-score", score).returncode, 0)

    def test_input_bounds_recordings_and_urls(self):
        case = copy.deepcopy(self.dataset["cases"][0])
        case["request"]["messages"][0]["content"] = "x" * 65537
        self.assertNotEqual(self.invoke("explain", case).returncode, 0)
        self.assertNotEqual(self.invoke("evaluate", " " * (8 * 1024 * 1024 + 1), raw=True).returncode, 0)
        case = copy.deepcopy(self.dataset["cases"][6])
        case["classifier_failure"] = "timeout"
        self.assertNotEqual(self.invoke("explain", case).returncode, 0)
        result = subprocess.run([BINARY, "semantic", "explain", "https://invalid/private", "https://invalid/fixture"], capture_output=True, text=True, timeout=5)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("https://invalid", result.stdout + result.stderr)

    def test_configuration_errors_do_not_echo_private_paths(self):
        policy = self.root / "PRIVATE_POLICY.yaml"
        policy.write_text("PRIVATE_INVALID: PRIVATE_VALUE")
        result = self.invoke("explain", self.dataset["cases"][0], policy=policy)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("PRIVATE", result.stdout + result.stderr)
        fifo = self.root / "input-fifo"
        os.mkfifo(fifo)
        result = subprocess.run([BINARY, "semantic", "explain", str(FIXTURES / "semantic-policy-v1.yaml"), str(fifo)],
            capture_output=True, text=True, timeout=5)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(json.loads(result.stdout)["error"], "regular file required")

    @unittest.skipUnless(sys.platform == "linux" and platform.machine() == "x86_64", "Linux x86_64 seccomp proof")
    def test_offline_evaluation_survives_network_killing_seccomp(self):
        try:
            control = subprocess.run([sys.executable, "-B", "-c", "import socket; socket.socket()"],
                capture_output=True, timeout=5, preexec_fn=deny_network)
        except subprocess.SubprocessError:
            self.skipTest("sandbox does not permit nested seccomp")
        self.assertLess(control.returncode, 0, "seccomp control must kill a socket attempt")
        result = subprocess.run([BINARY, "semantic", "evaluate", str(FIXTURES / "semantic-policy-v1.yaml"),
            str(FIXTURES / "semantic-v1.json"), "--require-match"], capture_output=True, text=True,
            timeout=15, preexec_fn=deny_network)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["aggregate"]["matched"], self.dataset["declared_cases"])


if __name__ == "__main__":
    unittest.main()
