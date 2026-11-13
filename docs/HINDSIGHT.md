# Optional Hindsight connection

Slotline can expose **explicit** retain, recall and reflect operations backed by
an existing Hindsight HTTP server. The integration is disabled by default.
Hindsight is not an inference dependency: Slotline never calls memory from chat,
Responses or embeddings, never captures transcripts, and never injects recalled
content into prompts. No Hindsight server, installer, lifecycle management, MCP
proxy, general plugin runtime or Memlake integration is included.

## Operator configuration

Merge this optional section into your existing Slotline policy. These are
illustrative values, not a command to modify a running service:

```yaml
hindsight:
  enabled: true
  endpoint: http://127.0.0.1:9180
  api_key_env: SLOTLINE_HINDSIGHT_API_KEY
  # Alternative, not additional:
  # api_key_file: /absolute/private/path/hindsight.key
  timeout_ms: 60000
  bindings:
    example-reader:
      bank_id: example-read-bank
      allow_retain: false
    example-writer:
      bank_id: example-write-bank
      allow_retain: true
```

- `endpoint` is an operator-approved HTTP(S) **origin**. Userinfo, paths (other
  than `/`), query strings, fragments and backslash/whitespace variants are
  rejected. HTTPS is recommended. HTTP sends the upstream token and memory in
  cleartext at the HTTP layer: use it only over a trusted loopback/private
  encrypted network. Slotline does not discover endpoints or follow redirects,
  and this client ignores proxy environment variables. Egress/DNS restrictions
  remain the operator's responsibility.
- Configure **at most one** of `api_key_env` and `api_key_file`. Inline secrets
  are unsupported. Environment variable names use ordinary ASCII identifier
  syntax. Secret files must be absolute, regular, service-owned and private
  (`0600` or stricter); final-component symlinks are refused. Secret values are
  nonempty printable ASCII tokens of at most 4096 bytes; file contents may have
  surrounding whitespace. File reads are bounded and do not wait on FIFOs.
- Secrets are resolved only when an enabled, authorized memory operation or an
  authenticated CLI bank check needs them. A missing reference, unset variable,
  absent/unreadable file or invalid secret does **not** prevent startup or
  ordinary inference. It makes memory operations fail with a safe `503`.
  Reference syntax and other invalid configuration still fail configuration
  validation. An enabled connection requires an endpoint, not a resolved key.
- `timeout_ms` defaults to 10000, range **100–120000**. Memory requests have one
  wall-clock deadline covering authentication, request upload, secret resolution,
  connection, response headers and response body. There are no memory retries.
  The example uses 60000 because bank statistics and LLM-backed operations can
  take tens of seconds. Choose a budget for your backend; do not remove deadlines.
- `bindings` maps an existing Slotline credential identity to exactly one bank.
  For file credentials use `credentials[].name`; for the verifier store use the
  key policy's `id`, **not** the key prefix or token. A missing mapping denies
  all memory operations. Unknown identities stay inert; adding a mapping does
  not create or authorize a Slotline credential. Rotation retaining an identity
  retains its mapping; revocation and expiry still apply. Do not reuse an old
  identity without reviewing its mapping.
- A mapping grants recall and reflect; retain additionally requires
  `allow_retain: true` (default **false**, even for operator credentials).
  Bank IDs are 1–64 ASCII letters/digits/underscores/hyphens. Mapping multiple
  identities to one bank deliberately shares its data. Tags are filters, not an
  authorization boundary: use separate banks for isolation.
- Credential rate limits still apply. Memory has a separate class-scoped
  in-flight pool, bounded by that class's configured `max_in_flight`. Pending
  memory requests do not consume inference permits, including when Hindsight
  calls a Slotline model using a dedicated credential in the same class.
  Grant memory access only to intended workloads. These mappings do not grant
  model aliases, route access or cloud permission.

Omitting `hindsight` or setting `enabled: false` causes **no Hindsight network
traffic, secret reads or background tasks**, including CLI checks. Connection,
mapping, timeout and secret-reference changes require a restart; policy reload
rejects them so a routing-only reload cannot silently expand memory authority.
Changing an existing secret's contents does not require restart. Diagnostics
never print its value, reference path, endpoint or upstream error body.

## Status and read-only checks

```sh
slotline hindsight status /absolute/path/slotline.yaml
slotline hindsight check /absolute/path/slotline.yaml
```

Both commands print a JSON object. They read local policy configuration without
loading Slotline workload keys, opening the verifier store or starting a router.
`status` is entirely offline: it reports enablement, reference presence, timeout
and binding count, with `health: "not_checked"`. It does **not** claim the secret
exists or the server is reachable.

`check` makes an unauthenticated `GET /health`, then an authenticated
`GET /v1/default/banks/{configured-bank}/stats` for each distinct mapped bank.
Each individual probe has the configured deadline and response cap. A large
binding set therefore takes up to `(1 + distinct banks) * timeout_ms` in total.
It never lists banks, calls the unsupported bank-root GET, or performs
retain/recall/reflect. Health and bank outcomes are separate:

```json
{"state":"enabled","health":"reachable","banks":{"example-read-bank":"unauthorized"},"ok":false}
```

Output also includes the configuration-presence fields described above. `401`
and `403` bank responses become `unauthorized`; timeouts, redirects, transport,
secret and response failures use fixed diagnostic labels. HTTP success with an
object JSON response is required. A successful `/stats` check demonstrates only
that read endpoint's access, not permission for every operation or persistence.
Public health/OpenAPI access is **not** bank authorization, even when the upstream
OpenAPI document omits security schemes. `check` exits nonzero if health or any
bank check fails, or an enabled integration has no mapped banks. A disabled
check exits successfully without probing anything.

## Explicit authenticated HTTP API

All three endpoints require the existing Slotline workload Bearer credential:

| Slotline endpoint | Fixed upstream POST path |
| --- | --- |
| `/v1/memory/retain` | `/v1/default/banks/{mapped-bank}/memories` |
| `/v1/memory/recall` | `/v1/default/banks/{mapped-bank}/memories/recall` |
| `/v1/memory/reflect` | `/v1/default/banks/{mapped-bank}/reflect` |

Send `Content-Type: application/json`. There are no URL query parameters, bank
selectors, arbitrary paths or upstream headers. The only upstream authorization
is the configured Hindsight key. Caller Authorization, Host, cookies, tenant/bank
headers and forwarded-authority headers are **not** forwarded. Upstream response
headers are not exposed. Successful memory responses use `Cache-Control: no-store`.

Example JSON bodies (send with your own workload client; do not put secrets in
shell history):

```json
{"query":"What did we decide?","budget":"mid","max_tokens":1024,"tags":["project-a"]}
```

```json
{"query":"Summarize our decision","context":"Focus on project A","max_tokens":1024}
```

```json
{"items":[{"content":"The team selected option A.","tags":["project-a"]}],"async":true}
```

The facade implements a bounded, text-oriented subset of the Hindsight
1.2.0 HTTP contract and generated Python API models in
[`vectorize-io/hindsight`](https://github.com/vectorize-io/hindsight). Newer
upstream versions may contain additional fields:

- Retain requires a nonempty `items` array with nonempty string `content`.
  Supported item keys are `content`, `timestamp`, `context`, `metadata` (string
  values), `document_id`, `entities`, `resolve_entities`, `tags`,
  `observation_scopes`, `strategy`, `update_mode`. Top-level options are `async`,
  `document_tags`, `operation_id`. Multimodal content/attachments are unsupported.
- Recall requires nonempty string `query`; options are `types`,
  `prefer_observations`, `rerank`, `budget`, `max_tokens`, `trace`,
  `query_timestamp`, `include`, `tags`, `tags_match`, `tag_groups`, `min_scores`,
  `temporal_window`.
  `rerank` is an object with optional boolean/null `prune` and `dedup`, not a
  boolean flag. Retain `observation_scopes` is a string (for example `combined`
  or `per_tag`) or a list of lists of tag strings, not an object.
- Reflect requires nonempty string `query`; options are `context`, `budget`,
  `max_tokens`, `include`, `response_schema`, `tags`, `tags_match`, `tag_groups`,
  `fact_types`, `apply_all_directives`, `exclude_mental_models`,
  `exclude_mental_model_ids`, `reflect_search_observations_max_tokens`,
  `reflect_search_observations_include_entities`.
- Top-level and item unknown keys are rejected, including `bank`, `bank_id`,
  `tenant`, `tenant_id`, `endpoint`, credentials and `extra_body`. Basic field
  types are validated locally. Nested option objects retain upstream semantics;
  their complete version-specific validation remains upstream. Token limits
  must be integers from 1 to 1000000. Budgets are `low`, `mid`, `high`;
  `tags_match` supports `any`, `all`, `any_strict`, `all_strict`, `exact`.
- `stream` and `stream_options` are rejected **even when false**. SSE Accept
  requests and SSE/non-JSON upstream responses are rejected, not buffered into
  an unbounded pseudo-stream. Request bodies are capped at **256 KiB**, response
  bodies at **2 MiB**, including chunked bodies. Malformed/non-object JSON and
  invalid success envelopes fail safely.

Successful operation responses preserve authorized memory content and the
upstream HTTP success status. Recall must contain a results array, reflect a
text string, and retain a successful acknowledgement for the mapped bank with
`items_count` and `async`. Async acknowledgement is **not** completed retention
or an independent durability guarantee: inspect `async` and `operation_id` in
the returned response. This facade deliberately provides no job polling or
administrative operations. Use separately authorized upstream tooling where
needed; do not mistake that tooling for facade-protected access.
The optional reflect `structured_output_error` diagnostic is replaced with a
fixed message when non-null; reflection text and structured memory content remain
intact. This avoids exposing raw upstream diagnostic text inside a success body.

Disabled memory returns `404`; unauthenticated/expired credentials `401`;
unmapped identities or forbidden retain `403`; invalid body/unsupported flags
`400`; request overflow `413`; wrong content type `415`; existing workload
admission limits `429`; secret/transport initialization unavailability `503`;
deadline expiry `504`. Upstream authorization, redirects, upstream errors and
malformed/oversized responses become fixed `502` errors. Raw upstream error text,
headers, URLs and token values are never included. Do not automatically retry a
failed retain: a timeout or disconnect may occur after upstream accepted it.

No operation task is detached. Dropping a handler future cancels its local
upstream future and releases its permit. A network disconnect does not guarantee
that the HTTP stack immediately drops a buffered handler; it may run until the
same fixed deadline. Cancellation cannot undo work already accepted upstream,
including async retention. No body, transcript or memory content is logged by
this integration; existing router request correlation records generic metadata.

## Hindsight using Slotline models

Hindsight can independently act as an OpenAI-compatible **model client** of
Slotline's `/v1/chat/completions` and `/v1/embeddings` endpoints. Configure that
existing Hindsight deployment separately with a Slotline base URL ending in
`/v1`, an approved chat/model alias, and a dedicated least-privilege Slotline
workload key. If using remote OpenAI-compatible embeddings, select a compatible
embedding alias and preserve the bank's existing embedding model/dimensionality;
changing it is a separate data-migration decision. Consult your installed
Hindsight version's model/provider settings rather than copying unverified env
variable names. This integration does not alter those settings.

Hindsight's Slotline model key is **different** from the Hindsight API key held by
this addon. Its credential needs only approved model aliases/routes; normally
give it **no memory binding**. Existing `cloud_allowed` and provider-specific
gates remain required for cloud inference. Virtual routing policies additionally require independent grants for the policy
alias, judge alias and selected concrete alias, plus their physical routes. Judges
and targets must be concrete model aliases; policy recursion and memory endpoints
as model targets are rejected by existing policy validation. The judge retains
the router's existing audit isolation; only the selected target contributes to
public inference attempt, retry and usage accounting. Explicit
memory operations may cause Hindsight to call its configured models, but ordinary
Slotline model inference does not invoke memory, so there is no enrichment loop.

Keep existing agent-owned memory banks and ownership policies. Direct Hindsight
SDK/HTTP/MCP clients remain separate: the facade does **not** magically protect
them with Slotline credentials or mappings. Protect upstream keys, MCP tools,
network access and bank authorization independently. A broad upstream key is an
operator-held trust boundary; prefer the least privilege your Hindsight server
supports and never give that key to facade callers. No upstream service, bank,
model setting, retention policy or live data is changed by configuring Slotline.

## Verification

Tests use disposable local mocks and in-process fixtures. Run in an environment
that permits loopback sockets and has a private temporary directory:

```sh
cargo test --locked hindsight_
cargo build --locked
python3 tests/hindsight_cli.py /path/to/slotline
SLOTLINE_TEST_BINARY=/path/to/slotline SLOTLINE_TEST_HOST=YOUR_ASSIGNED_TAILNET_IPV4 \
  python3 tests/hindsight_http.py
```

The HTTP fixture needs an assigned Tailnet bind address, private short `TMPDIR`
for control sockets, and permission to create loopback listeners. It uses no live
memory banks or cloud services. See [development checks](DEVELOPMENT.md) for the
full suite and tokenizer prerequisites.
