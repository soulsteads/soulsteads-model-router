# Migrate an existing client

Move one local-model workload at a time. Soulsteads Model Router is not a
universal replacement for an existing OpenAI-compatible gateway. It builds as
`soulsteads-router`; changing a client's base URL alone does not establish compatibility.

## Upgrade the router name

The executable is already named `soulsteads-router`. This change also renames
its configuration and telemetry interfaces. Before deploying it:

- Back up the existing policy and provider credential stores. Move or copy
  them securely to the `soulsteads-router` configuration directory (normally
  `${XDG_CONFIG_HOME:-$HOME/.config}/soulsteads-router`), preserving ownership and
  permissions, or keep explicit paths. Check absolute paths in the policy and
  generated or hand-written units. Validate the new policy path with
  `soulsteads-router --check-config` before switching services.
- Update launcher and fixture environment variables to the `SOULSTEADS_ROUTER_`
  prefix, including `SOULSTEADS_ROUTER_CONFIG` where used. Update custom
  `RUST_LOG` directives to the `soulsteads_router` target.
- Update clients that send `X-Soulsteads-Router-Session` and tools that consume
  `X-Soulsteads-Router-Routing-*` headers or `soulsteads_router_*` metrics.
  Coordinate both sides of the change: old names are not accepted as aliases.
  Drain pinned conversations before switching, since a restart clears pins.

## 1. Inventory the client contract

Record the workload's current base URL, model alias, authentication method,
timeouts, fallbacks, and request shapes. Keep credentials and request samples
outside Git and chat.

Check which endpoints and features the client actually uses:

- Chat Completions, Responses, or embeddings.
- Streaming, tools and tool-result continuation, structured output, or images.
- Context/output budgets, embedding dimensions/encoding, and extra parameters.
- Cancellation behavior, retries, and stateful Responses features.

Compare these requirements with [compatibility and limits](docs/COMPATIBILITY.md).
Do not silently strip required fields, remap model identities, or enable an
unverified capability to make a request pass. Stateful Responses continuation
is not supported by managed routing.

Codex CLI to a managed local Responses alias is qualified only as recorded in
[qualification](docs/QUALIFICATION.md): one local model, flat function tools and
Codex-known model slugs that send developer `additional_tools` and namespaced custom
tools, on the recorded runtime revisions. Hosted tools, other local models and
task-quality parity remain unqualified. This does not make the router a replacement
for any existing routing path. Verify each capability your client requires before
moving traffic.

For an active routing policy, also check whether the client supplies an explicit
session identity. [Session pins](docs/ROUTING.md#session-pins-memory-only) are
held in memory and disappear on restart. A tool-bearing or stateful continuation
without a pin returns HTTP 409 `continuation_pinned` before judge inference or
upstream dispatch. Include restart and continuation in the canary; don't hide a
refusal by removing the continuation state.

If a client also uses cloud models, retain its existing provider route for those
calls. This gateway targets local inference servers, does not provide cloud
subscriptions, and does not depend on LiteLLM.

## 2. Start a parallel gateway

Follow [setup](docs/SETUP.md) on a separate listener. Keep the current gateway,
credentials, and client configuration available for rollback. Do not take over
port `4001`; the validator reserves it. The listener requires an assigned
Tailnet IPv4 address by default. For same-host use, configure `listener_mode:
loopback` explicitly. The gateway serves HTTP without built-in TLS.

Use the backend's exact model ID, verified context/concurrency limits, and only
capabilities qualified for that model and endpoint. Create a dedicated client
credential with the minimum required alias and route access; see
[key management](KEYS.md).

Avoid counting the same backend capacity twice. Admission counters are
process-local: traffic through another gateway or directly to the backend does
not share them. Coordinate concurrency across both paths during the migration.
Leave [passthrough](docs/full-passthrough.md) off unless you explicitly accept its
bypassed admission, deadline, validation, and buffering protections.

## 3. Verify before switching traffic

Validate the policy, authentication, authorized catalog, and a real request.
Then exercise representative client requests, including applicable tools,
structured output, streaming, and embedding formats. Test rejected credentials,
unsupported inputs, overload, upstream failures, deadlines, and cancellation.

HTTP 200 does not establish a complete or correct response. Check stream
termination, tool-call/result pairing, output shape, model identity, and whether
admission permits are released after cancellation or failure. Use
[status and operations](docs/OPERATIONS.md) and [request audit](docs/request-audit.md)
without logging private payloads.

The [development guide](docs/DEVELOPMENT.md) lists fixtures and known test
limitations. A passing unit-test summary does not replace real-client
acceptance or a sustained workload check.

## 4. Canary one workload

Back up the client's configuration privately and rehearse restoring its original
URL, alias, and credential reference. Change only the selected local workload;
leave unrelated defaults, cloud providers, and fallback chains unchanged.

Observe a representative period of traffic. Check latency, errors, output
correctness, queue/capacity behavior, retries, and client cancellation. Define
acceptable bounds and rollback triggers for your workload before expanding it.
Do not infer readiness from a one-shot probe or another installation's results.

## 5. Roll back or expand deliberately

If acceptance fails, restore the original client configuration and verify a real
request through the original path. Coordinate in-flight requests before stopping
or replacing the parallel gateway; a restart can interrupt streams. Restore
protected policy/key-store backups only through the documented
[key-store restore procedure](KEYS.md).

If the canary succeeds, migrate remaining workloads individually. Retire the
old path and revoke obsolete credentials only after confirming no remaining
clients or fallback paths depend on them. Keep rollback material private and
available until the migration is accepted.

Policy reload does not support physical-route changes or credential-authority
changes. Use the [reload and update limits](docs/OPERATIONS.md) when changing
configuration; do not assume a successful file edit changes the running gateway.
