#!/usr/bin/env python3
"""Bounded authenticated OpenAI-compatible Qwen shadow contract acceptance."""
import argparse
import json
import pathlib
import sys
import urllib.error
import urllib.request


class AcceptanceError(Exception):
    pass


class Normalized:
    def __init__(self, text="", tool_calls=None, done=False):
        self.text = text
        self.tool_calls = tool_calls or []
        self.done = done


def normalize_response(data):
    try:
        choice = data["choices"][0]
        message = choice.get("message", {})
    except (KeyError, IndexError, TypeError):
        raise AcceptanceError("response missing choices/message")
    calls = message.get("tool_calls") or []
    if not isinstance(calls, list):
        raise AcceptanceError("tool_calls is not a list")
    content = message.get("content") or ""
    if not isinstance(content, str):
        raise AcceptanceError("message content is not text")
    return Normalized(content, calls, choice.get("finish_reason") == "stop")


def parse_sse(raw):
    text, done = [], False
    for line in raw.splitlines():
        line = line.strip()
        if not line or line.startswith(":") or not line.startswith("data:"):
            continue
        payload = line[5:].strip()
        if payload == "[DONE]":
            done = True
            continue
        try:
            data = json.loads(payload)
            for choice in data.get("choices", []):
                delta = choice.get("delta", {})
                if isinstance(delta.get("content"), str):
                    text.append(delta["content"])
        except (ValueError, TypeError, AttributeError):
            raise AcceptanceError("invalid SSE data event")
    if not done:
        raise AcceptanceError("SSE stream missing [DONE]")
    return Normalized("".join(text), done=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--key-file", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("--max-tokens", type=int, default=256)
    parser.add_argument("--max-cases", type=int, default=7)
    args = parser.parse_args()
    if not (0 < args.timeout <= 120 and 1 <= args.max_tokens <= 2048 and 1 <= args.max_cases <= 7):
        parser.error("timeout, max-tokens, or max-cases outside safety bounds")
    try:
        key = pathlib.Path(args.key_file).read_text().strip()
    except OSError:
        print("FAIL credential file unreadable (secret suppressed)")
        return 2
    if not key:
        print("FAIL credential file empty (secret suppressed)")
        return 2
    base = args.base_url.rstrip("/")
    endpoint = base if base.endswith("/chat/completions") else base + ("/chat/completions" if base.endswith("/v1") else "/v1/chat/completions")

    def request(label, messages, **extra):
        payload = {"model": args.model, "messages": messages, "max_tokens": args.max_tokens, **extra}
        req = urllib.request.Request(endpoint, json.dumps(payload).encode(), {"Authorization": "Bearer " + key, "Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, timeout=args.timeout) as response:
                raw = response.read(2_000_001)
                if len(raw) > 2_000_000:
                    raise AcceptanceError("response exceeds 2MB bound")
        except urllib.error.HTTPError as exc:
            raise AcceptanceError(f"HTTP {exc.code}")
        except (urllib.error.URLError, TimeoutError, OSError) as exc:
            raise AcceptanceError(type(exc).__name__)
        if extra.get("stream"):
            result = parse_sse(raw.decode("utf-8", "replace"))
        else:
            try:
                result = normalize_response(json.loads(raw))
            except (ValueError, TypeError):
                raise AcceptanceError("malformed JSON response")
        return result

    cases = []
    def run(label, fn):
        if len(cases) >= args.max_cases:
            return
        try:
            fn()
            cases.append((label, True, ""))
        except AcceptanceError as exc:
            cases.append((label, False, str(exc)[:180]))
        except (ValueError, TypeError, KeyError, IndexError):
            cases.append((label, False, "invalid response contract"))
    run("normal", lambda: _require_text(request("normal", [{"role": "user", "content": "Reply with exactly: SLOTLINE_OK"}]), "SLOTLINE_OK"))
    def tool_case():
        result = request("tool", [{"role": "user", "content": "Call the weather function for Oslo."}], tools=[{"type":"function","function":{"name":"weather","description":"Get weather","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}], tool_choice="required")
        if not result.tool_calls or result.tool_calls[0].get("function", {}).get("name") != "weather":
            raise AcceptanceError("tool call absent or wrong")
        call = result.tool_calls[0]
        continuation = request("continuation", [{"role":"user","content":"Call weather for Oslo."},{"role":"assistant","content":None,"tool_calls":[call]},{"role":"tool","tool_call_id":call.get("id"),"content":"sunny"},{"role":"user","content":"Summarize the weather in one word."}])
        _require_text(continuation, "")
    run("tool_call_and_continuation", tool_case)
    def stream_case():
        result = request("stream", [{"role":"user","content":"Reply with exactly STREAM_OK"}], stream=True)
        _require_text(result, "STREAM_OK")
    run("stream_sse_done", stream_case)
    def json_object():
        result = request("json_object", [{"role":"user","content":"Return an object with ok set to true."}], response_format={"type":"json_object"})
        parsed = json.loads(result.text)
        if not isinstance(parsed, dict) or parsed.get("ok") is not True: raise AcceptanceError("json_object did not return expected object")
    run("json_object", json_object)
    def json_schema():
        schema={"type":"object","properties":{"status":{"type":"string","enum":["ready"]}},"required":["status"],"additionalProperties":False}
        result=request("json_schema", [{"role":"user","content":"Return status ready."}], response_format={"type":"json_schema","json_schema":{"name":"readiness","strict":True,"schema":schema}})
        parsed=json.loads(result.text)
        if parsed != {"status":"ready"}: raise AcceptanceError("json_schema result violates schema")
    run("json_schema", json_schema)
    run("malformed_no_secret", lambda: _malformed_probe(endpoint, key, args))
    for label, passed, detail in cases:
        print(("PASS" if passed else "FAIL") + " " + label + (" — " + detail if detail else ""))
    print(f"RESULT {sum(p for _,p,_ in cases)}/{len(cases)} cases passed")
    return 0 if all(p for _,p,_ in cases) else 1


def _require_text(result, expected):
    if not result.text.strip(): raise AcceptanceError("empty assistant content")
    if expected and expected not in result.text: raise AcceptanceError("expected marker absent")


def _malformed_probe(endpoint, key, args):
    # Invalid requests must fail with a structured error that never echoes the bearer.
    req=urllib.request.Request(endpoint, b"{", {"Authorization":"Bearer "+key,"Content-Type":"application/json"})
    try:
        with urllib.request.urlopen(req,timeout=args.timeout) as response:
            raw=response.read(2_000_001)
        raise AcceptanceError("malformed request accepted (HTTP 200)")
    except urllib.error.HTTPError as exc:
        if exc.code != 400: raise AcceptanceError(f"malformed request HTTP {exc.code}, expected 400")
        raw=exc.read(2_000_001)
    except (urllib.error.URLError,TimeoutError,OSError) as exc:
        raise AcceptanceError(type(exc).__name__)
    if key.encode() in raw: raise AcceptanceError("malformed response contains bearer")
    try:
        data=json.loads(raw)
    except (ValueError,TypeError):
        raise AcceptanceError("malformed request returned non-JSON error")
    if not isinstance(data,dict) or not isinstance(data.get("error"),dict):
        raise AcceptanceError("malformed request lacks structured error")


if __name__ == "__main__":
    sys.exit(main())
