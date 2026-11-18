"""Compiled offline routing acceptance. Uses only synthetic inputs; no live config.

Set SLOTLINE_TEST_BINARY and TMPDIR. --manifest records named checks, counts and
blocked network-exclusion controls. No expected counts are copied from another suite.
"""
import argparse
import copy
import ctypes
import json
import os
from pathlib import Path
import platform
import resource
import re
import shutil
import signal
import subprocess
import sys
import tempfile


def deny_network():
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))

    class Instruction(ctypes.Structure):
        _fields_ = [("code", ctypes.c_ushort), ("yes", ctypes.c_ubyte),
                   ("no", ctypes.c_ubyte), ("value", ctypes.c_uint)]

    class Program(ctypes.Structure):
        _fields_ = [("length", ctypes.c_ushort),
                   ("instructions", ctypes.POINTER(Instruction))]

    instructions = [Instruction(0x20, 0, 0, 4),
                    Instruction(0x15, 1, 0, 0xC000003E),
                    Instruction(0x06, 0, 0, 0x80000000),
                    Instruction(0x20, 0, 0, 0)]
    for syscall in [41, 42, 43, 44, 45, 46, 47, 49, 50, 51, 52, 53, 54, 55,
                    288, 299, 307, 425]:
        instructions.extend([Instruction(0x15, 0, 1, syscall),
                             Instruction(0x06, 0, 0, 0x80000000)])
    instructions.append(Instruction(0x06, 0, 0, 0x7FFF0000))
    array = (Instruction * len(instructions))(*instructions)
    program = Program(len(instructions), array)
    library = ctypes.CDLL(None, use_errno=True)
    if library.prctl(38, 1, 0, 0, 0) or library.prctl(22, 2, ctypes.byref(program), 0, 0):
        raise RuntimeError("network-denying seccomp unavailable")


def policy():
    route = {"name": "local", "kind": "local", "base_url": "http://127.0.0.1:9/v1",
             "model": "synthetic", "safe_concurrency": 1, "context_limit": 32768,
             "adapter_version": "synthetic-v1", "enabled": True,
             "capabilities": ["text", "responses", "tools"], "streaming": True,
             "credential_file": "/synthetic/SYNTHETIC_SECRET_SENTINEL"}
    alias = {"context_limit": 32768, "capabilities": ["text", "responses", "tools"],
             "routes": [{"route": "local", "priority": 0}]}
    return {"bind": "100.127.255.254", "port": 4000, "routes": [route],
            "key_store": "/synthetic/SYNTHETIC_STORE_SENTINEL", "aliases": {
                "small": alias, "judge": copy.deepcopy(alias)},
            "codex": {"enabled": True, "alias": "cloud", "model": "synthetic",
                      "account_file": "/synthetic/SYNTHETIC_ACCOUNT_SENTINEL"},
            "copilot": {"enabled": True, "models": [{"alias": "copilot-chat",
                        "model": "synthetic", "endpoint": "chat_completions"}]},
            "routing_policies": {
                "smart": {"version": "synthetic-v1", "default_target": "cloud", "rules": [
                    {"id": "Unicode λ rule", "target": "small", "when": {
                        "credential_classes": ["batch"], "user_text": {"keywords": ["rust"]}}}]},
                "judge-policy": {"decision_model": "judge", "decision_targets": ["small", "cloud"],
                                 "default_target": "cloud"},
                "shadow": {"mode": "shadow", "default_target": "small",
                           "decision_model": "judge", "decision_targets": ["cloud"],
                           "rules": [{"id": "cloud-advice", "target": "cloud", "when": {"stream": False}}]},
                "denied-baseline": {"mode": "shadow", "default_target": "cloud"}}}


def case(name, model="smart", text="rust", endpoint="chat", expected=None):
    request = {"model": model}
    request.update({"messages": [{"role": "user", "content": text}]} if endpoint == "chat"
                   else {"input": text})
    raw = json.dumps(request, ensure_ascii=False) + "  \n"
    return {"schema_version": 1, "id": name, "endpoint": endpoint, "raw_body": raw,
            "input_bytes": len(raw.encode()), "grants": {"synthetic": True,
                "credential_class": "batch", "aliases": ["smart", "judge-policy", "shadow",
                "denied-baseline", "native-prefilter", "copilot-policy", "byte-policy", "small", "judge", "cloud", "copilot-chat"],
                "allowed_routes": ["local", "codex", "copilot"], "cloud_allowed": True},
            **({"expected": expected} if expected else {})}


def prediction(outcome, target=None, state="deferred"):
    return {"state": state, "outcome": outcome, "target": target}


def judge(content):
    return {"choices": [{"finish_reason": "stop", "message": {"content": content}}]}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", required=True)
    args = parser.parse_args()
    binary = str(Path(os.environ["SLOTLINE_TEST_BINARY"]).resolve())
    checks = []
    cases = []
    directory = tempfile.mkdtemp(prefix="routing-offline-")
    # Retain fixtures and logs for caller-controlled inspection and cleanup.
    root = Path(directory)
    config = policy()
    account_path = root / "private-storage" / "nested" / "SYNTHETIC_ACCOUNT_SENTINEL"
    config["codex"]["account_file"] = str(account_path)
    # Include the account path itself and every parent, even outside this fixture.
    account_git_paths = [path / ".git" for path in [account_path, *account_path.parents]]

    def account_metadata_lines(content):
        return [line for line in content.splitlines()
                if any(json.dumps(str(path)) in line for path in account_git_paths)]
    config["routing_policies"]["native-prefilter"] = {"decision_model": "judge", "decision_targets": ["cloud"], "default_target": "small"}
    config["routing_policies"]["copilot-policy"] = {"default_target": "copilot-chat"}
    config["routing_policies"]["byte-policy"] = {"default_target": "cloud", "rules": [{"id": "inclusive", "target": "small",
        "when": {"min_input_bytes": 100, "max_input_bytes": 100}}]}
    config_path = root / "policy.json"
    config_path.write_text(json.dumps(config))
    input_path = root / "input.json"
    env = dict(os.environ, HOME=str(root / "absent-home"), XDG_CONFIG_HOME=str(root / "absent-xdg"),
               XDG_CACHE_HOME=str(root / "absent-cache"), SLOTLINE_CONFIG="/synthetic/SYNTHETIC_LIVE_CONFIG_SENTINEL",
               HTTP_PROXY="http://127.0.0.1:9", HTTPS_PROXY="http://127.0.0.1:9")

    def check(name, condition):
        checks.append({"name": name, "status": "passed" if condition else "failed"})
        if not condition:
            raise AssertionError(name)

    def invoke(command, value, *options, preexec_fn=None):
        input_path.write_text(json.dumps(value, ensure_ascii=False))
        return subprocess.run([binary, "routing", command, str(config_path), str(input_path), *options],
                              env=env, capture_output=True, text=True, timeout=20, preexec_fn=preexec_fn)

    def accept(item):
        result = invoke("explain", item)
        check(item["id"] + ": exit", result.returncode == 0)
        value = json.loads(result.stdout)
        check(item["id"] + ": expectation", value["matched"] is True)
        check(item["id"] + ": authorization", value["authorization_assertion"] is True)
        cases.append(copy.deepcopy(item))
        return value

    try:
        first = case("text-batch", expected=prediction("rule", "small"))
        absent = invoke("explain", first)
        check("account ancestor Git absent", absent.returncode == 0)
        # Above the account directory, wholly inside our private TMPDIR fixture.
        (root / ".git").mkdir()
        present = invoke("explain", first)
        check("identical offline inputs ignore account ancestor Git",
              present.returncode == 0 and present.stdout == absent.stdout)
        accept(first)
        different = copy.deepcopy(first)
        different["id"] = "explicit-class"
        different["grants"]["credential_class"] = "different"
        accept(dict(different, expected=prediction("default", "cloud")))
        for endpoint in ["chat", "responses"]:
            accept(case("text-" + endpoint, endpoint=endpoint, expected=prediction("rule", "small")))
            pending = case("judge-absent-" + endpoint, model="judge-policy", endpoint=endpoint,
                           expected=prediction("judge_not_run"))
            output = accept(pending)
            check(pending["id"] + ": deferred", "judge_not_run" in output["deferred"])
            supplied = copy.deepcopy(pending)
            supplied.update(id="judge-fixture-" + endpoint, judge_response=judge('{"target":"small"}'),
                            expected=prediction("fixture_decision", "small"))
            accept(supplied)
            malformed = copy.deepcopy(supplied)
            malformed.update(id="judge-strict-" + endpoint, judge_response=judge('{"target":"small","confidence":1}'),
                             expected=prediction("invalid_decision", state="rejected"))
            accept(malformed)
        shadow = case("shadow-no-judge", model="shadow", expected=prediction("shadow", "small"))
        accept(shadow)
        shadow["grants"]["cloud_allowed"] = False
        shadow.update(id="shadow-denied-advice")
        check("shadow denied advice hidden", accept(shadow)["advisory"] is None)
        denied = case("shadow-denied-baseline", model="denied-baseline",
                      expected=prediction("model_not_allowed", state="rejected"))
        denied["grants"]["cloud_allowed"] = False
        accept(denied)
        single = case("judge-single", model="judge-policy", expected=prediction("judge_not_run"))
        single["grants"]["cloud_allowed"] = False
        check("one candidate still judge", accept(single)["candidates"] == ["small"])
        config_path.write_text(json.dumps(config))
        for endpoint in ["chat", "responses"]:
            for name, extra, top, outcome in [
                ("empty", {}, {}, "judge_not_run"),
                ("stream-extra", {"stream": False}, {}, "judge_not_run"),
                ("nested-empty", {"extra_body": {}}, {}, "model_not_allowed"),
                ("nested-stream", {"extra_body": {"stream": False}}, {}, "model_not_allowed"),
                ("thinking", {"enable_thinking": False}, {}, "model_not_allowed"),
                ("conflicting", {"stream": True}, {"stream": False}, "unsupported_parameter"),
                ("stream-type", {}, {"stream": "false"}, "invalid_request")]:
                item = case("codex-" + endpoint + "-" + name, model="native-prefilter", endpoint=endpoint)
                body = json.loads(item["raw_body"])
                body.update(top)
                body["extra_body"] = extra
                item["raw_body"] = json.dumps(body)
                item["input_bytes"] = len(item["raw_body"].encode())
                item["expected"] = prediction(outcome, state="deferred" if outcome == "judge_not_run" else "rejected")
                accept(item)
        for size, target in [(100, "small"), (101, "cloud")]:
            item = case("inclusive-bytes-" + str(size), model="byte-policy", text="hi")
            raw = json.dumps(json.loads(item["raw_body"]), separators=(",", ":"))
            item.update(raw_body=raw + " " * (size - len(raw.encode())), input_bytes=size,
                        expected=prediction("rule" if size == 100 else "default", target))
            accept(item)
        item = case("copilot-known-deferred", model="copilot-policy", expected=prediction("default", "copilot-chat"))
        check("Copilot capability deferral", "copilot_request_capabilities" in accept(item)["deferred"])
        item = case("opaque-responses-history", model="native-prefilter", endpoint="responses",
                    expected=prediction("fixture_decision", "cloud"))
        body = {"model": "native-prefilter", "input": [
            {"type": "reasoning", "id": "rs_synthetic", "encrypted_content": "synthetic", "summary": []},
            {"type": "function_call", "namespace": "functions", "name": "run", "call_id": "call_synthetic", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_synthetic", "output": "synthetic"}],
            "tools": [{"type": "namespace", "name": "functions", "tools": [{"type": "function", "name": "run", "parameters": {"type": "object"}}]}],
            "include": ["reasoning.encrypted_content"]}
        item.update(raw_body=json.dumps(body), judge_response=judge('{"target":"cloud"}'))
        item["input_bytes"] = len(item["raw_body"].encode())
        accept(item)
        private = case("nonsecret-output", model="judge-policy", text="PRIVATE_PROMPT_SENTINEL",
                       expected=prediction("judge_not_run"))
        result = invoke("explain", private)
        check("prompts absent from reports", result.returncode == 0 and "PRIVATE_" not in result.stdout + result.stderr)
        unknown = case("tokenizer-deferred", expected=prediction("rule", "small"))
        config["aliases"]["small"]["tokenizer"] = {
            "file": "/synthetic/SYNTHETIC_TOKENIZER_SENTINEL", "sha256": "0" * 64}
        config_path.write_text(json.dumps(config))
        output = invoke("explain", unknown)
        check("unknown tokenizer is deferred", "tokenizer_context" in json.loads(output.stdout)["deferred"])
        # Known reserve bounds remain conclusive even with an unreadable tokenizer.
        for endpoint in ["chat", "responses"]:
            fields = ["max_tokens", "max_completion_tokens"] if endpoint == "chat" else ["max_output_tokens"]
            for field in fields:
                for name, alias_limit, route_limit, reserve, rejected in [
                    ("alias-bound", 32768, 65536, 32769, True),
                    ("route-bound", 32768, 8192, 8193, True),
                    ("compatible", 32768, 8192, 8192, False),
                    ("default-alias-bound", 4095, 32768, None, True),
                    ("default-route-bound", 32768, 4095, None, True),
                    ("default-compatible", 4096, 4096, None, False)]:
                    variant = copy.deepcopy(config)
                    variant["aliases"]["small"]["context_limit"] = alias_limit
                    variant["routes"][0]["context_limit"] = route_limit
                    # This route has capacity but no grant; it must not rescue local.
                    larger = copy.deepcopy(variant["routes"][0])
                    larger.update(name="larger", context_limit=65536)
                    variant["routes"].append(larger)
                    variant["aliases"]["small"]["routes"].append({"route": "larger", "priority": 1})
                    item = case("reserve-" + endpoint + "-" + field + "-" + name, endpoint=endpoint)
                    body = json.loads(item["raw_body"])
                    if reserve is not None:
                        body["extra_body"] = {field: reserve}
                    item.update(raw_body=json.dumps(body))
                    item["input_bytes"] = len(item["raw_body"].encode())
                    config_path.write_text(json.dumps(variant))
                    result = invoke("explain", item)
                    output = json.loads(result.stdout)
                    check(item["id"], result.returncode == 0 and output["prediction"] == (
                        prediction("incompatible", state="rejected") if rejected else prediction("rule", "small")))
                    if not rejected:
                        check(item["id"] + ": prompt sizing deferred", "tokenizer_context" in output["deferred"])
        config["aliases"]["small"].pop("tokenizer")
        config_path.write_text(json.dumps(config))
        dataset = {"schema_version": 1, "declared_cases": len(cases), "cases": cases}
        result = invoke("evaluate-selection", dataset, "--require-match")
        output = json.loads(result.stdout)
        check("dataset exact manifest", result.returncode == 0 and output["aggregate"]["total"] == len(cases)
              and output["aggregate"]["matched"] == len(cases) and output["aggregate"]["unauthorized_selections"] == 0)
        check("deterministic output", invoke("evaluate-selection", dataset).stdout == result.stdout)
        mismatched = copy.deepcopy(dataset)
        mismatched["cases"][0]["expected"]["target"] = "cloud"
        check("CI mismatch fails", invoke("evaluate-selection", mismatched, "--require-match").returncode != 0)
        check("CI threshold accepts", invoke("evaluate-selection", mismatched, "--min-score", "0").returncode == 0)
        for score in ["NaN", "inf", "-1", "1.1"]:
            check("invalid threshold " + score, invoke("evaluate-selection", dataset, "--min-score", score).returncode != 0)
        for name, edit in [
            ("schema", lambda c: c.update(schema_version=2)),
            ("unknown", lambda c: c.update(PRIVATE_UNKNOWN="PRIVATE_SENTINEL")),
            ("bytes", lambda c: c.update(input_bytes=0)),
            ("synthetic", lambda c: c["grants"].update(synthetic=False)),
            ("class", lambda c: c["grants"].pop("credential_class")),
            ("duplicate grant", lambda c: c["grants"]["aliases"].append("small")),
            ("unknown grant", lambda c: c["grants"]["aliases"].append("unknown"))]:
            invalid = copy.deepcopy(first)
            edit(invalid)
            result = invoke("explain", invalid)
            check("strict " + name, result.returncode != 0 and "PRIVATE" not in result.stdout + result.stderr)
        for name, edit in [
            ("case count", lambda data: data.update(declared_cases=999)),
            ("dataset version", lambda data: data.update(schema_version=2)),
            ("empty dataset", lambda data: data.update(cases=[], declared_cases=0)),
            ("missing expected", lambda data: data["cases"][0].pop("expected")),
            ("duplicate ID", lambda data: data["cases"][1].update(id=data["cases"][0]["id"]))]:
            invalid = copy.deepcopy(dataset)
            edit(invalid)
            check("strict dataset " + name, invoke("evaluate-selection", invalid).returncode != 0)
        input_path.write_bytes(b" " * (8 * 1024 * 1024 + 1))
        command = [binary, "routing", "explain", str(config_path), str(input_path)]
        result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=20)
        check("bounded file", result.returncode != 0 and json.loads(result.stdout)["error"] == "input exceeds size limit")
        fifo = root / "fifo"
        os.mkfifo(fifo)
        result = subprocess.run(command[:-1] + [str(fifo)], env=env, capture_output=True, text=True, timeout=5)
        check("FIFO nonblocking regular requirement", result.returncode != 0 and json.loads(result.stdout)["error"] == "regular file required")
        # The original quality evaluator keeps its own schema and CLI.
        result = invoke("evaluate", dataset)
        check("outcome schema isolation", result.returncode != 0 and "routing_selection" not in result.stdout)
        if sys.platform == "linux" and platform.machine() == "x86_64":
            try:
                control = subprocess.run([sys.executable, "-B", "-c", "import socket; socket.socket()"],
                                         capture_output=True, timeout=5, preexec_fn=deny_network)
                check("seccomp socket positive SIGSYS control", control.returncode == -signal.SIGSYS)
                result = invoke("evaluate-selection", dataset, "--require-match", preexec_fn=deny_network)
                checks[-1]["returncode"] = control.returncode
                check("offline startup under network kill", result.returncode == 0)
                checks[-1]["returncode"] = result.returncode
            except subprocess.SubprocessError:
                checks.append({"name": "seccomp", "status": "blocked", "reason": "nested seccomp unavailable"})
        else:
            checks.append({"name": "seccomp", "status": "blocked", "reason": "requires Linux x86_64"})
        if shutil.which("strace"):
            trace = root / "network-control.trace"
            control = subprocess.run(["strace", "-f", "-s", "4096", "-e", "trace=network", "-o", str(trace),
                sys.executable, "-B", "-c", "import socket; socket.socket()"], capture_output=True, timeout=10)
            (root / "strace-control.stderr").write_bytes(control.stderr)
            network_traced = "socket(" in trace.read_text()
            if network_traced:
                check("strace socket positive control", True)
            else:
                checks.append({"name": "strace socket positive control", "status": "blocked", "reason": "ptrace unavailable"})
            metadata_trace = root / "account-metadata-control.trace"
            # Probe only synthetic paths inside this fixture. The detector also
            # covers every ancestor outside it without visiting those paths.
            probes = [account_path / ".git", root / ".git"]
            program = "import os,sys\nfor path in sys.argv[1:]:\n try: os.stat(path)\n except OSError: pass\n"
            metadata_control = subprocess.run([
                "strace", "-f", "-s", "4096", "-e", "trace=%file", "-o", str(metadata_trace),
                sys.executable, "-B", "-c", program, *map(str, probes)],
                capture_output=True, timeout=10)
            (root / "strace-metadata-control.stderr").write_bytes(metadata_control.stderr)
            metadata_content = metadata_trace.read_text()
            detected = account_metadata_lines(metadata_content)
            metadata_traced = metadata_control.returncode == 0 and all(
                any(json.dumps(str(path)) in line and re.match(
                    r"^\s*(?:(?:\d+|\[pid\s+\d+\])\s+)?(?:stat|stat64|newfstatat|statx)\(", line)
                    for line in detected) for path in probes)
            if metadata_traced:
                check("strace account metadata positive control", True)
            elif b"ptrace" in metadata_control.stderr or b"Operation not permitted" in metadata_control.stderr:
                checks.append({"name": "strace account metadata positive control", "status": "blocked", "reason": "ptrace unavailable"})
            else:
                check("strace account metadata positive control", False)
            # Attempt the actual command even when a native control is blocked.
            # Configured sentinels cover tokenizer, account, credential and store.
            traced_config = copy.deepcopy(config)
            traced_config["aliases"]["small"]["tokenizer"] = {
                "file": "/synthetic/SYNTHETIC_TOKENIZER_SENTINEL", "sha256": "0" * 64}
            config_path.write_text(json.dumps(traced_config))
            input_path.write_text(json.dumps(dataset))
            trace = root / "network-offline.trace"
            result = subprocess.run(["strace", "-f", "-s", "4096", "-e", "trace=network,%file", "-o", str(trace),
                binary, "routing", "evaluate-selection", str(config_path), str(input_path), "--require-match"],
                env=env, capture_output=True, timeout=20)
            (root / "strace-offline.stderr").write_bytes(result.stderr)
            content = trace.read_text()
            if network_traced and metadata_traced:
                calls = [line for line in content.splitlines() if any(call in line for call in
                    ["socket(", "socketpair(", "connect(", "sendto(", "recvfrom(", "bind(", "listen(",
                     "accept(", "accept4(", "sendmsg(", "recvmsg(", "sendmmsg(", "recvmmsg(", "shutdown(",
                     "getsockname(", "getpeername(", "getsockopt(", "setsockopt("])]
                check("strace startup no network", result.returncode == 0 and not calls)
                check("strace no account ancestor metadata accesses", not account_metadata_lines(content))
                check("strace no tokenizer store key account accesses", not any(marker in content for marker in
                    ["SYNTHETIC_", "absent-home", "absent-xdg", "absent-cache"]))
            else:
                checks.append({"name": "strace offline exclusion", "status": "blocked", "reason": "positive controls unavailable"})
        else:
            checks.append({"name": "strace", "status": "blocked", "reason": "strace unavailable"})
            checks.append({"name": "strace account metadata positive control", "status": "blocked", "reason": "strace unavailable"})
    except Exception as error:
        checks.append({"name": "acceptance exception", "status": "failed", "reason": type(error).__name__})
    finally:
        report = {"kind": "routing_offline_acceptance", "cases": [c["id"] for c in cases],
                  "case_count": len(cases), "fixture_directory": str(root),
                  "account_git_paths": list(map(str, account_git_paths)), "checks": checks,
                  "counts": {state: sum(c["status"] == state for c in checks)
                             for state in ["passed", "failed", "blocked"]}}
        Path(args.manifest).write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report["counts"]))
    return int(any(item["status"] != "passed" for item in checks))


if __name__ == "__main__":
    sys.exit(main())
