# Request audit

Request audit records explain routing decisions and request delivery. Audit events use the existing structured tracing JSON output; they do not change routing, retry, deadline, concurrency, or admission behavior. The worker writes records to stdout. Under the optional user service, the systemd journal supplies retention and rotation; foreground runs need their own stdout capture. Audit adds no service, durable store, or dedicated file/database/network I/O on the inference path.

## Privacy and identity

- `request_id` is a server-generated opaque correlation ID. Audit events exclude prompts, responses, tool arguments, keys, raw affinity IDs, and credential/customer names. Reasons, outcomes, and labels are bounded; labels allow at most 64 ASCII alphanumeric/dot/dash/underscore characters beginning with an alphanumeric character. Invalid labels become `unknown`.
- `build_source_sha256` identifies sorted Rust source paths and contents, `Cargo.toml`, `Cargo.lock`, and `build.rs` at build time, including dirty source. It does not identify the compiler/toolchain, flags, assets, or external tokenizer bytes. Git revision/state are best-effort cached build metadata. There is no runtime Git lookup per request.
- `policy_sha256` hashes a canonical schema-4 nonsecret policy projection at startup and accepted reload. It includes route/alias order and eligibility, priorities, capacity, capabilities, model/adapter, timeouts, health paths, affinity leases, credential workload grants/limits, and matcher source/categories. Native Copilot enablement and alias-keyed model/endpoint mappings participate; OAuth credential contents and private storage paths do not. Copilot mapping order is canonicalized. Hindsight enablement, endpoint, timeout and explicit credential-to-bank/retain mappings also participate; its secret contents and secret references do not. Route destinations use parsed nonsecret URL components: scheme, host, default port, and dot segments are normalized; upstream path remains part of the identity. Route URL userinfo, query, and fragment are excluded, even from defensive projections of route URLs that production validation rejects. The Hindsight endpoint is serialized verbatim in the internal projection, so equivalent endpoint spellings may yield different policy identities. Full configuration validation rejects unsupported secret-bearing Hindsight URL forms (userinfo, query, fragment, or non-root paths) before normal runtime hashing; the projection itself does not defensively redact them. Destination/health paths are nonsecret policy fields: do not put secrets in path segments, workload names, or route names.
- The projection excludes key material, verifiers and secret-derived fingerprints, credential names outside explicit Hindsight bindings, key-store/file paths, tokenizer paths/hash, and matcher regex values. Therefore `policy_sha256` identifies routing policy, not every behavior-bearing authentication, tokenizer, or matcher change. The projection itself is not logged. Policy activation events carry old/new hashes and generations. A request retains its actual runtime and generation through body polling, cancellation, and reload; accumulated metrics are shared across reload.

## Events and fields

Audit records are correlated by the request span and have a `fields.message` event name. The offline report consumes tracing JSONL records. Relevant event families are:

- `request policy`: build and policy identity for the runtime bound to the request. This event can precede the report time window for a request that finishes inside it.
- `selection decision`: the existing algorithm's reason and up to 16 candidate occupancy/status/eligibility snapshots. Omitted candidates are counted. This describes the actual decision, not a counterfactual claim that a route is faster.
- `upstream attempt started` and `upstream attempt finished`: the actual outbound-send invocation number, normalized route, route status at start, elapsed milliseconds, received HTTP status, and safe outcome. Accounting/timing begin immediately before `send()`, after deadline checks and outbound construction. For nonstream validated responses, elapsed time includes bounded body reading/validation. `stream_headers` and `passthrough_headers` end at response headers, not valid stream completion. Outcomes include `status`, `transport`, `timeout`, `invalid_body`, and cancellation. Existing route-attempt counters count admitted selections. A selection whose deadline expires before dispatch creates no audit attempt or latency sample. Request summaries retain failed-send counts and the last attempt's status/category; detailed outcomes are bounded correlated events, not an unbounded request ledger.
- `request first output`: emitted once for the first accepted meaningful streamed text, refusal, reasoning, or tool-argument bytes. Empty roles, heartbeats, and creation metadata do not qualify. Responses output items and completed/incomplete result items qualify when actual output exists. Observation reuses the existing parser; it does not add another inference-path parse. Nonstream first-output timing is unavailable.
- Terminal body completion, stream error, body/handler cancellation records include duration, attempts/retries, first-output time, and reported usage when available. Missing usage remains absent; zero is a real sample. Only nonnegative integers up to one billion tokens per field are accepted, and the latest cumulative report is retained. Usage is reported, not estimated.

Final request outcomes distinguish `success_first_attempt`, `recovered_after_retry`, `failed`, `failed_after_retry`, `overload`, `rejected`, `cancelled`, and `passthrough_complete`. A later failed retry is not a recovery. Nonvalidated passthrough has no inferred backend success or usage.

Backend outcome and body delivery are separate. Validated JSON or valid SSE terminal completion supplies backend `complete`; semantically incomplete Responses remain `incomplete`; stream errors supply `failed`; otherwise backend outcome is `unavailable`. A body can be cancelled after backend completion. HTTP 200 alone does not prove backend/body completion. Body EOF means the server finished polling the body; it does not prove remote receipt/display or answer quality. Cancellation is observed before the handler response, before the first body poll, and mid-stream. Existing permit release, qualification/failure behavior, and no-replay semantics remain unchanged.

For model-routing policies, request attempts, retries, backend outcome, and usage describe target inference only, not internal judge inference. Missing target usage remains unknown. Judge selection and failure diagnostics retain the request correlation ID, and judge inference still participates in global route health and capacity accounting. End-to-end request duration includes judging; a judge failure or cancellation before target dispatch records no target attempts.

## Output, limits, and coverage

The worker owns stdout I/O. Producers format whole records into a fixed 8 KiB buffer and use a nonblocking `try_send` to a 256-record queue. Oversized records are dropped whole, not intentionally truncated. The queue uses about 2 MiB plus one worker record and transient producer buffers (approximately 16 KiB per concurrently formatting producer). This is a queue bound, **not** a concurrency-independent global allocation cap. Producers do not wait for sink I/O or retry a full queue. A stalled sink drops new records.

Protected operator status exposes identity, group/loss counts, and limits. Protected `/metrics` exposes `slotline_audit_*`:

- Fixed cumulative histograms for duration, headers, meaningful first output, and attempts, with `_bucket`, `_count`, and `_sum` in milliseconds and an infinity bucket.
- Final request outcomes, backend outcomes, and independently counted successful body EOF.
- Usage sums and per-field sample counts; missing usage is not treated as zero.
- Full-queue, oversized-format, sink-error, disconnected, written, and abandoned record counters, plus bounded metric-group observation drops.

Histogram categories are configured route/class plus static endpoint, with a hard 1,024-group cap shared across generations. No request/affinity IDs or build/policy hashes are Prometheus labels. Missing first output has no observation; operator calls do not populate inference histograms. Metrics do not affect admission.

I/O failure or panic disconnects the worker. RAII accounting counts the failed in-flight record and accepted queued records abandoned during receiver destruction, including racing sends. Sink-error events and lost-record counts are separate. Disconnected/full rejections do not double-count abandonment. Counter snapshots are eventually consistent during concurrent producer/worker updates. Metric groups are copied under a short lock and rendered outside it. `written` means write and flush succeeded, not durable journal storage. Partial external writes can leave a malformed physical line on I/O failure. There is no blocking shutdown drain; termination can lose queued records without an observable final counter. Journal rotation/rate limiting, tracing filters, and sink losses can all reduce coverage.

The report is bounded as well: 64 KiB per input line, 4,096 pending requests, 1,024 output groups, 64 retained attempts per request, 64 attempt-timeline categories per group, up to 16 candidates per selection event, and fixed histograms/enums. Invalid labels/enums/scalars are normalized or rejected. Coverage reports malformed/oversized lines, missing starts, evictions, pending requests, group/attempt/timeline overflow, and omitted candidates. Sink/journal loss cannot be reconstructed from surviving JSONL; compare protected sink counters and retention context. Partial history is not proof of total traffic, availability, clean success, or answer quality.

## Run the report

Run from the repository root. These examples use the optional setup guide's `slotline.service`; substitute your own unit name. The script reads tracing JSONL from standard input by default, or from `--input`:

```sh
journalctl --user -u slotline.service --since '2 hours ago' -o cat --no-pager \
  | python3 scripts/audit_report.py
python3 scripts/audit_report.py --input /path/to/slotline.jsonl
```

To filter by terminal-event time, use an inclusive `--since` and/or exclusive `--until`. Timestamps must use strict ISO `T` syntax with a valid timezone offset; `--since` must precede `--until`. Replace the illustrative timestamps below and export a journal range covering your chosen window, including earlier request starts.

```sh
journalctl --user -u slotline.service --since '2 hours ago' -o cat --no-pager \
  | python3 scripts/audit_report.py --since 2026-10-02T17:00:00Z --until 2026-10-02T19:00:00Z
```

Schema 2 groups by alias, final route, class, endpoint, build/policy hash, and policy generation. Output includes bounded attempt timelines, selection reasons/eligibility, first-output histograms, and usage with sample counts. `retried` and `retried_rate` count requests where any retry occurred, regardless of final outcome. `recovered_after_retry` counts successful delivery after retries; `failed_after_retry` counts failed terminal delivery after retries. `retries` sums retry count and `failed_attempts` counts observed failed sends. Backend validity and successful delivery are independent counters; legacy records have unavailable backend/usage. The report emits no request IDs or payload fields.

Window selection uses terminal time. Earlier correlation is retained when present, so export sufficient preceding history for requests that straddle a window. Percentiles are fixed-bucket upper bounds, not exact quantiles; missing/overflow bounds are null. `coverage.pending_at_end` and other coverage values describe report input/processing, not all traffic. Check the source of the JSONL for truncation and retention limits.
