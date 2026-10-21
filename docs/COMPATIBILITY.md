# Compatibility and limits

Soulsteads Model Router exposes an OpenAI-compatible subset, not every OpenAI
feature. Capabilities belong to the exact backend model, server version, and
endpoint you have tested. A shared model-family name or a successful
`/v1/models` request does not prove tools, structured output, or image support.

This page describes **managed routing**. Opt-in
[passthrough](full-passthrough.md) bypasses many of these checks and limits.

## Platform and network

- The implementation uses Unix control sockets and Unix file permissions;
  the setup guide targets Linux.
- The listener must bind an explicit, assigned Tailnet IPv4 address in
  `100.64.0.0/10`. Loopback, LAN, wildcard, and IPv6 listener binds are rejected.
- Port `4001` is reserved by the current validator.
- Local upstreams use literal loopback or private IPv4 addresses, not DNS names.
- The gateway serves HTTP without built-in TLS. Keep it on a protected network;
  it is not an internet-facing authentication or TLS termination service.

These are current implementation constraints, not requirements to replace your
existing gateway or change your network. Models run in separately managed
inference servers; Slotline does not load/unload models or control their KV cache.

## HTTP endpoints

Every endpoint requires a bearer credential. Inference and catalog access are
filtered by that credential's policy. Operator access is additionally required
for `/healthz`, `/status`, and `/metrics`.

- `GET /v1/models`: authorized configured aliases, with stable context metadata.
- `POST /v1/chat/completions`: managed chat and eligible inline-image requests.
- `POST /v1/responses`: bounded text Responses requests.
- `POST /v1/embeddings`: separate embedding-mode aliases.
- `GET /healthz`, `/status`, `/metrics`: operator health, state, and metrics.

The alias is the client-facing model name. Managed responses normalize model
identity to that alias. Upstream models must match their configured route.
Caller authorization is never forwarded; upstream credentials are separate
private route configuration.

## Supported request shapes

### Chat Completions

Text, bounded function tools, tool-result continuation, and streamed tool calls
are supported on capable routes. Structured output supports `json_schema` and
`json_object` only when declared on the alias and selected route.

Chat model arguments can appear at the top level or inside `extra_body`, which
is flattened before forwarding. Bounded extra model arguments are left for the
upstream to interpret; forwarding is not a guarantee it supports them. Requests
cannot choose an upstream URL, credential, provider, or backend override.

### Responses

Bounded text input, function tools, stateless function-call/result continuation,
and streaming are supported on capable routes. Structured Responses output
requires the endpoint-specific `responses_json_schema` or
`responses_json_object` capability; chat format support alone is insufficient.

Stateful continuation with `previous_response_id` or `conversation` is rejected.
Responses image input, audio, and video are not supported by the managed path.

### Images

Only Chat Completions on explicitly image-capable routes accepts images:

- inline PNG, JPEG, or WebP `data:*;base64,` URLs;
- at most four images per request and 1 MiB decoded per image;
- 16,384 context tokens reserved per image.

Remote image URLs are rejected to prevent backend fetches. Slotline preserves
accepted content parts and never falls back from an image request to a text-only
route. Verify image behavior on each exact backend before declaring it capable.

### Embeddings

Embedding aliases declare `endpoint: embedding` and use `/v1/embeddings`, not the
chat endpoint. `float` and `base64` output formats are supported when the
selected backend can serve them. The response must have one unique index per
input and consistent, nonempty, finite vectors.

## Context, capacity, and failure behavior

- Configure total context ceilings for both alias and route. Admission accounts
  for the prompt and completion reserve. Chat can use a checksum-pinned tokenizer;
  unverified templates use a conservative byte-based fallback.
- Embedding context uses a byte-based estimate, not a verified embedding-model
  tokenizer contract. Do not treat the configured estimate as proof that an
  upstream token limit cannot be exceeded.
- Managed inference request bodies are limited to 8 MiB. Nonstream upstream
  JSON reads are capped at 16 MiB, and parsed SSE events at 1 MiB.
- Physical capacity is shared across a direct route and its declared adapter.
  Alias priorities and advisory affinity leases do not create more slots.
  Admission and leases are process-local: another gateway instance or direct
  backend traffic does not share these counters. Coordinate those limits
  separately rather than treating one process's view as a global GPU cap.
- Retry eligibility is rechecked for each distinct attempted route. Retryable
  upstream failures may fail over before response exposure. Client errors and
  exposed stream failures are not replayed.
- A valid terminal closes the upstream and releases admission. A failure after
  SSE handoff closes the body with an error; the committed HTTP status cannot
  change. A client must handle incomplete streams, even after HTTP 200.
- Expiry of an affinity lease does not cancel requests, release their permits,
  unload a model, or guarantee cache retention.

The default retry policy is at most eight attempts over 1,800 total seconds;
route and class deadlines can shorten this. Choose client deadlines with those
budgets in mind. See [operations](OPERATIONS.md#request-deadlines).

## Validate your deployment

Start with the text-only [example policy](../config/slotline.example.yaml) and
[setup guide](SETUP.md). Add capabilities only after testing the exact request
shapes your clients use, including streaming, tools, schema handling, deadlines,
and cancellation. Do not silently move unrelated cloud-provider traffic or
fallback chains to this local-model endpoint.

`src/config.rs`, `src/request_validation.rs`, `src/validation.rs`, and
`src/embedding_context.rs` define the current schema and validation boundaries.
The [development guide](DEVELOPMENT.md) lists isolated tests and known limits;
passing mocks alone does not qualify every backend version or sustained workload.
