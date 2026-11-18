#!/usr/bin/env python3
"""Summarise internal routing decision records (unstable format, see docs/ROUTING.md).

Reads tracing JSONL from stdin or --input, joins each `routing policy selected` event to the
same request's terminal `request body completed` event by request_id, and prints JSON:
request counts by provider kind and reason, local share of requests and of reported tokens,
refusals, and decision-overhead percentiles. Requests without reported usage are counted
separately; token share covers only requests that reported tokens. Not an acceptance tool.
"""
import argparse, json, sys
from collections import Counter


def pct(values, q):
    if not values:
        return None
    values = sorted(values)
    return values[min(len(values) - 1, int(round(q * (len(values) - 1))))]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--input")
    args = parser.parse_args()
    stream = open(args.input) if args.input else sys.stdin
    selected, done, refused = {}, {}, Counter()
    for line in stream:
        try:
            event = json.loads(line)
        except ValueError:
            continue
        fields = event.get("fields", {})
        rid = (event.get("span") or {}).get("request_id")
        message = fields.get("message")
        if message == "routing policy selected" and rid:
            selected[rid] = fields
        elif message == "routing policy refused":
            refused[fields.get("refused", "unknown")] += 1
        elif message == "request body completed" and rid:
            done[rid] = fields
    kinds, reasons = Counter(), Counter()
    tokens, no_usage, overhead = Counter(), 0, []
    for rid, f in selected.items():
        kind = f.get("provider_kind", "unknown")
        kinds[kind] += 1
        reasons[f.get("reason", "unknown")] += 1
        if isinstance(f.get("decision_ms"), int):
            overhead.append(f["decision_ms"])
        total = (done.get(rid) or {}).get("total_tokens")
        if isinstance(total, int):
            tokens[kind] += total
        else:
            no_usage += 1
    n = sum(kinds.values())
    token_total = sum(tokens.values())
    print(json.dumps({
        "requests_placed": n,
        "by_provider_kind": dict(kinds),
        "by_reason": dict(reasons),
        "local_request_share": (kinds["local"] / n) if n else None,
        "tokens_by_provider_kind": dict(tokens),
        "local_token_share": (tokens["local"] / token_total) if token_total else None,
        "placed_without_reported_tokens": no_usage,
        "note": "token shares cover only requests that reported usage (native cloud routes may report none)",
        "refused": dict(refused),
        "decision_ms": {"p50": pct(overhead, 0.5), "p95": pct(overhead, 0.95), "max": max(overhead, default=None)},
    }, indent=2))


if __name__ == "__main__":
    main()
