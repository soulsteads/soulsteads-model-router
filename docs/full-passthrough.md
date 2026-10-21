# Full authenticated passthrough

Passthrough pins an authorized alias to one configured backend and forwards its
response without interpreting it. Use it only when your caller/backend controls
supply the limits that managed routing would otherwise enforce.

> [!CAUTION]
> **Passthrough bypasses managed physical admission, rate limits, context and
> capability checks, response validation, and gateway deadlines.** It buffers
> authenticated JSON uploads without an application size cap. It is not a safer
> or faster default; a large upload can consume gateway RAM.

## What stays enforced

- Credential verification, expiry/revocation, alias authorization, endpoint
  authorization, and configured route permissions.
- A single configured target: callers cannot supply an arbitrary upstream URL.
- Separate upstream credentials; the caller's bearer credential is not forwarded.
- Credential-bearing and hop-by-hop header filtering in both directions.
- Disabled HTTP redirects and environment proxies.

A request must be a JSON object with an authorized `model` and use a supported
inference endpoint. Slotline rewrites that model to the route's backend model.
This is an authenticated model gateway, not an arbitrary URL tunnel or a
byte-for-byte upload stream.

## What is bypassed

For opted-in inference, Slotline does **not** enforce:

- body/context/capability/schema limits;
- rate limits, workload concurrency, or physical admission slots;
- route qualification or cooldown;
- request/connect/read deadlines;
- retries, fallback, or response validation.

Backend and caller limits still apply. Slotline returns upstream status and
streams response bytes without parsing or rewriting model fields. An upstream
HTTP success is not validated model completion.

## Configure explicitly

An alias sets `passthrough_route` and has exactly one enabled alias-route entry
matching that enabled backend. The entry cannot be fallback-only, and its
chat/embedding endpoint kind must match. Route/alias configuration still has to
pass the normal policy parser.

For example, this **replacement alias block** uses `local-a` from the text-only
example policy; it is not a complete policy and must not be appended as a second
`aliases` mapping:

```yaml
aliases:
  local-chat:
    context_limit: 8192
    capabilities: [text]
    streaming: true
    passthrough_route: local-a
    routes:
      - {route: local-a, priority: 10}
```

The context/capability fields remain configuration/catalog metadata; they do not
become passthrough enforcement. Validate with `slotline --check-config CONFIG.yaml`
and use the [safe update procedure](OPERATIONS.md#reload-policy-or-restart-safely).
Do not assume a previously configured client or backend now has adequate limits.

## Mixed credentials and observability

Managed aliases retain their own admission, body, schema, context, timeout, and
response contracts. However, a credential authorized for **any** passthrough
alias can upload unbounded JSON before the model field is known. Selecting a
managed alias afterward still rejects a body over 8 MiB, but only after buffering.
Use separate narrowly scoped credentials and account for this memory exposure.

`safe_concurrency`, `context_limit`, timeout, health state, and slot telemetry do
not gate passthrough requests. Slot active counts are not passthrough in-flight
counts: zero slots are not evidence that a restart is safe. `/v1/models`,
`/status`, and other control endpoints remain managed/authenticated APIs.
See [request auditing](request-audit.md) for header-only passthrough attempt
accounting and its measurement limits.
