# Optional semantic routing

Semantic routing is disabled by default. It resolves an **operator-configured
selector alias** to an existing, explicitly granted generation alias. It neither
loads models nor creates new provider authority. Ordinary model requests never
invoke a decision model, even when semantic routing is enabled.

## Configuration

Add this optional block to a policy whose `general`, `coding` and `judge` aliases
are already configured and qualified. These are illustrative operator aliases,
not suggested production models. There is no default classifier model.

```yaml
semantic_routing:
  enabled: true
  max_in_flight: 2
  max_input_bytes: 8192
  profiles:
    auto:
      mode: active
      compatibility_preflight: false
      policy_version: example-v1
      targets: [general, coding]
      default_target: general
      rules:
        - id: code-keywords
          priority: 100
          target: coding
          case_sensitive: false
          regex: '\b(code|debug|refactor|function|stack[- ]trace)\b'
        - id: short-help
          priority: 10
          target: general
          keywords: ["quick question", "summarize"]
      decision:
        classifier: judge
        confidence_threshold: 0.8
        timeout_ms: 2000
        max_output_tokens: 128
        on_failure: default
```

The code/debug/refactor/function/stack-trace regex is an **example only**, not an
active deployment rule. No live policy is changed by this feature.

Grant the workload `auto`, `general`, `coding`, and (when used) `judge`, and its
required physical/provider routes using the existing credential/key policy.
The selector grants no target or classifier privileges. It needs no invented
route entry of its own. Rules-only profiles omit `decision`; profiles may omit
`rules`. `targets` and `default_target` are always explicit. With no rule match
and no decision model, the default is used without an extra inference call.

Selectors cannot collide with native, Codex or Copilot aliases. Targets and the
classifier cannot be selectors, including selectors in other profiles; recursive
profiles and classifier callbacks are rejected at configuration load. The
classifier may also be an ordinary target alias: internal dispatch is explicitly
non-recursive, so this does not create a callback loop.

`slotline --check-config CONFIG.yaml` validates this block along with the normal
policy. Changing **any** semantic configuration requires a restart. Atomic
policy reload rejects such changes before replacing the runtime. Existing
permitted alias/route reloads still undergo normal validation. The policy digest
includes semantic configuration. In-flight requests retain their runtime
snapshot; the decision admission pool is shared across allowed reloads.

Disabling a configured block hides selectors from discovery and makes selector
requests unavailable without calling the classifier. Existing selector grants
may remain in file/key policies, but their referenced configuration must still
be valid. Explicit aliases remain usable independently of classifier health.

## Deterministic rules and extraction

- Higher numeric `priority` wins. Equal priorities keep YAML list order; the
  first matching rule wins. Hash-map iteration, server load and randomness do
  not affect rule selection. A denied matched target fails closed, not by trying
  a different rule. The configured default must itself be authorized before
  routing starts, including when a rule would match.
- Each rule contains **either** a nonempty `keywords` list **or** a `regex`.
  Keywords are literal substrings, ORed together, not regular expressions or
  word-boundary matches. Both forms use Rust regex Unicode simple case folding
  by default. `case_sensitive: true` turns that off. Inline regex flags can
  override the builder's case setting. The example uses explicit word boundaries.
- Chat uses the **last message whose explicit role is `user`**, not the last
  message overall. String content is used directly; `type: text` parts are
  joined with newlines. Image/non-text parts are not classifier input. System,
  developer, assistant, tool and function messages are never inferred to be
  user text, even if they follow the latest user message.
- Responses string `input` is user text. For a message array, only the last
  explicit `role: user` message is used; `input_text` parts are joined with
  newlines. **Every** array entry must first be a complete self-contained text
  message, including earlier system/assistant entries. Supported roles are
  `system`, `developer`, `user` and `assistant`; content must be a string or a
  nonempty array of `input_text`/`output_text` parts containing string `text`
  and no other fields. Non-text Responses content is unsupported through
  selectors. Unroled items, `instructions` and tool outputs are never treated as
  user text. No user text, an empty latest user message, or oversized text fails
  with HTTP 400; the gateway does not silently truncate or search older messages.
- A complete message may omit `type` when it has no `id`, or explicitly use
  `type: message`. Only explicitly typed complete messages may carry a nonempty
  string `id` (at most 128 bytes). Optional `status` must be `in_progress`,
  `completed` or `incomplete`. Other message fields, malformed/null fields,
  partial messages, explicit non-message types, and **all id-bearing type-less
  items** are rejected before classification or target work. This includes
  references camouflaged with additional role/content fields. Literal `id`
  data in user text or a JSON schema is not an API item reference.
- Extraction inspects a normalized **copy**, including `extra_body`, so hidden
  continuation fields cannot slip past the semantic boundary. The original
  request is retained for normal selected-target normalization and validation.
  `extra_body.input` is checked identically when top-level `input` is absent;
  supplying both remains an unsupported duplicate parameter, not an override.

The extracted latest text is capped by `max_input_bytes` (default 8192, allowed
1–16384 UTF-8 bytes). The normal managed request body cap remains 8 MiB. Each of
at most 16 profiles has at most 32 distinct targets and 32 rules. Identifiers are
1–64 ASCII alphanumeric/`_`/`-`/`.` characters, starting with an alphanumeric.
Each rule has at most 16 keywords of 1–128 bytes, or one 1–512-byte regex.
Regex compilation uses 64 KiB compiled-size and DFA-cache budgets per rule;
invalid/oversized regexes fail configuration loading, not request handling.

## Decision model contract

Only unmatched rules invoke the optional classifier. It must be a configured
**managed native chat alias** (local or Headroom), not an embedding, raw
passthrough, Codex, Copilot or selector alias. This deliberately excludes cloud
classifiers: their provider-specific adapters do not share the native bounded
decision-body contract. Cloud targets are supported only through their existing
native provider adapters and permissions; this is not full provider parity.

The gateway sends a non-streaming Chat Completions request through the same
authorized base handler, request/context validation, retry, class capacity,
physical capacity, credential and provider path used for ordinary native chat.
The fixed system instruction treats user text as **data**, not instructions.
A separate JSON-encoded user message contains only that text and the finite,
authorized, endpoint-compatible candidate aliases. No private forbidden aliases,
original system instructions, tools, headers or credentials are added to this
prompt. Use meaningful operator target names; no model quality or calibration
guarantee is implied.

The generated content must be exactly one JSON object, for example:

```json
{"target":"coding","confidence":0.93}
```

Only `target` and `confidence` are accepted; unknown or duplicate fields,
markdown wrappers, unknown/non-candidate targets, non-finite/out-of-range
confidence, multiple choices, tool calls, refusals and a finish reason other
than `stop` fail closed. Confidence must be in `[0,1]` and at least the configured
threshold. The enclosing response must pass normal Chat validation, including
its **matching backend model field**. The gateway never interprets model output
as a route, URL, credential, model configuration or policy instruction.

Confidence is an **uncalibrated model assertion**, not a measured probability.
A low-confidence reply abstains from selecting its proposed target and follows
`on_failure`. The strict live wire contract is unchanged: arbitrary abstention
objects, null targets and prose are invalid, not new routing instructions.

The classifier request sets `temperature: 0` and the explicit token limit.
This requests reproducibility but does not make a remote model intrinsically
deterministic. Decision envelopes are bounded to 4096 bytes **while reading the
upstream body**; content is bounded to 2048 bytes. `max_output_tokens` is required
and allowed from 32 to 256. `timeout_ms` is required, 1–30000, and covers the
entire classifier dispatch, retries and response parsing.

`max_in_flight` (default 2, allowed 1–16) is an independent, gateway-wide,
fail-fast decision gate, not a bypass of inference controls. No outer class
inference permit is held during classification. Classifier and generation
acquire normal class/physical permits **sequentially**. A one-slot class can
therefore classify and then generate without self-deadlock. Dropping the
handler future or reaching the deadline releases classifier permits; already
submitted inference may continue on a backend that ignores connection closure.

Authentication/expiry is checked before decision network use. Ordinary request
admission charges once; an actual classifier dispatch also consumes a normal
rate token. Inspecting a profile, matching a rule or finding the decision gate
busy does not add a classifier rate charge. Normal admission may charge failed
work attempts, as it does for explicit requests. Classifier failures never
expand grants. Alias and allowed-route restrictions apply to both calls;
Codex/Copilot targets additionally require native `cloud_allowed` and the
provider's explicit route grant. Native Headroom routes retain their normal
shared physical-backend capacity contract.

`on_failure` is required:

- `default`: invalid/unknown/unauthorized output, low confidence, upstream error,
  deadline, rate rejection or busy/unavailable classifier capacity uses only
  the already authorized `default_target`.
- `error`: those failures return sanitized HTTP 503 `semantic_decision_failed`.

Choosing the configured default is not a guarantee of available generation.
Fallback still requires normal serving health, class/physical capacity and target
validation. A classifier HTTP/protocol failure can quarantine its physical route;
if the default shares that route, fallback remains unavailable (normally HTTP
503 `no_serving_capacity`). Configure independent healthy classifier/generation
routes if fallback must survive a classifier backend failure. Semantic routing
does not clear quarantine or bypass admission to make fallback succeed.

Missing selector, classifier, default or selected-target grants fail HTTP 403
before the denied call, regardless of failure policy. A classifier grant is
required only for the decision-model branch; a matching rule does not depend
on it. Normal target endpoint/capability/context errors remain normal errors;
they do not trigger a second semantic choice or silently strip request fields.

## API behavior, provenance and continuation

Use `model: auto` on `/v1/chat/completions` or supported `/v1/responses` generation
requests. Discovery lists only explicitly granted, enabled selectors and marks
their `model_info.semantic_selector` as true; it does not expose target lists.
Other request fields, tools, streaming flags and structured-output requests
remain intact. Only the model alias is resolved before ordinary dispatch.

Selectors do **not** support request-level policy overrides. The top-level fields
`cloud_allowed` and `local_only`, including either supplied through `extra_body`,
are rejected with HTTP 400 `unsupported_parameter` after extra-body normalization
and before any classifier or target work. Presence is rejected regardless of value
(including `false`, `true` or `null`); these fields are never consumed, dropped,
forwarded or treated as authority. This applies to Chat and Responses, rules,
defaults, model decisions and decision-fallback branches. A property with the same
name inside a tool/schema is not a request policy field. There are no supported
request-scoped locality-policy equivalents. Credential-level `cloud_allowed` and
provider/physical-route grants remain the supported authorization controls; use
a cloud-denied, appropriately route-restricted credential for private workloads.
Explicit non-semantic aliases retain their existing provider-specific behavior;
this selector guard does not add a new policy API to them.

Normal managed model validation is unchanged. In particular, managed Chat rejects
unsupported `metadata` with HTTP 400 `unsupported_parameter`; routing does not
strip it to make a request succeed. Supported fields such as `user`, `top_k`,
tools and structured-output schemas remain in the successful preservation tests.

Responses include bounded, operator-controlled provenance headers:

- `x-slotline-semantic-profile`: selector/profile name;
- `x-slotline-semantic-rule`: matched rule ID, only for a rule;
- `x-slotline-semantic-target`: selected authorized alias, when dispatched;
- `x-slotline-semantic-outcome`: `rule`, `model`, `default`, or a bounded fallback/
  error reason (`invalid`, `low_confidence`, `unavailable`, `timeout`, `busy`,
  `rate_limited`, `classifier_incompatible`, `denied`, `incompatible`). Active
  failure reasons with a target mean configured-default fallback; `shadow`
  identifies serving the baseline with advisory identity kept separate.

These headers never contain prompt text, raw classifier output/errors, secrets,
or a model-supplied identifier. Internal provider attempts still use the normal
audit/usage machinery. Existing request audit endpoint counts refer to the
external request; route attempts include internal classifier work. There is no
new durable semantic routing state, prompt log or conversation database.

**The selector does not disguise response model identity.** Managed native and
Codex responses retain their existing selected-alias identity behavior; Copilot
retains its existing provider response bytes, whose `model` may be the upstream
provider model rather than the operator alias. The authoritative selected alias
for pinning is `x-slotline-semantic-target`. There is no new semantic SSE rewrite
layer. Clients needing a stable target should pin that explicit alias on later
requests (with its existing grants). Do not resend the selector expecting sticky
conversation routing. Stateless full Chat history replay is supported subject to
normal validation, but can be routed differently on each request.

Embeddings, Hindsight memory/health operations and raw passthrough do not run
semantic logic. Selectors on the embeddings endpoint are explicitly rejected.
Passthrough aliases are forbidden as targets/classifiers. Provider-specific
continuation is **not** supported through selectors: `previous_response_id`,
`conversation`, stored/background requests, and Responses non-message input
items (including function-call continuations) are rejected before classification,
even when supplied inside `extra_body`. Pin an ordinary provider alias instead.
No automatic conversation transfer, provider handoff, stateful Responses/WebSocket
parity or semantic tool-continuation parity is claimed.

## Operator status

Operator-authenticated `/status` and the owner-only local `slotline status` CLI
include `semantic_routing.enabled`, sorted configured `selectors`, and
`decision_admission` (`limit`, `in_flight`, `available`, `closed`). The admission
snapshot reads the running decision gate, not inference-route capacity; an
unrelated reload retains that gate and its held permits. Semantic policy and
its limit remain restart-only. Counts are point-in-time observations, not
reservations or a guarantee that a target can serve a request.

Each active credential has a separate sorted `semantic_selectors` grant list;
the existing physical-route `aliases` and root `routes` maps are unchanged.
Configured selectors and grants remain visible when the feature is disabled;
that does not activate them or confer classifier/target/physical-route access.
The new metadata is bounded by the configured profile/identifier limits and
contains no prompts, raw classifier responses, secrets or private paths.
Plain output labels these as “Configured selectors”, “Decision admission” and
“Selector grants”, rather than inventing physical routes for selectors.

## Profile modes, compatibility and provenance

All new fields are optional. `mode` defaults to `active`; existing configurations,
disabled selectors and explicit provider/native aliases retain their behavior.
`policy_version`, when present, is 1–64 ASCII alphanumeric/`_`/`-`/`.` bytes,
starting with an alphanumeric. Invalid labels, modes and unknown options fail
configuration loading, even for disabled profiles. These fields participate in
the existing policy identity digest and restart-only semantic comparison.

`mode: shadow` serves **only `default_target`**, using its ordinary generation
handler, admission, retries, body/error/status and SSE pipeline. It never sends
generation to a recommended alias. Advisory rules use the same ordered engine
as active routing; a forbidden or incompatible advisory is redacted and cannot
fail an otherwise valid baseline. Normal selector authorization, normalized
input/continuation denials and the baseline's authoritative validation still
apply. An explicit alias bypasses selector mode entirely.

**No live shadow classifier runs**, including for nonstreaming traffic. Unmatched
model work is `classifier_not_run`/`unresolved`, not an invented default advisory.
`x-slotline-semantic-classifier: not_run` makes this explicit in shadow responses.
Recorded classifier outputs can exercise model-based shadow candidates offline.
Full live decision-model shadow is deferred until guaranteed isolation of rate,
class/physical capacity, deadline and failure/quarantine effects is available.
There is no sampling, speculative generation, automatic promotion or background
classifier task in this increment; unsupported knobs fail validation.

`compatibility_preflight: true` checks the **normalized full request**, not just
extracted text, before paid classifier work. It reuses native Chat/Responses
validators, configured alias/route capabilities and authorized route intersections
for endpoint, tools, inline images, structured formats, streaming and context.
The default is checked first. Classifier candidates exclude known incompatible
or unauthorized targets; a matched denied/incompatible rule fails closed rather
than trying the next rule. The fixed classifier request also gets native
preflight. Nothing is removed from generation requests to manufacture compatibility.

Runtime and offline decision contexts retain the original, unnormalized request.
Compatibility projects a separate copy exactly once per alias with the target
handler's `rewrite_thinking` setting: enabled for managed Chat/Responses aliases,
disabled for Codex. The inspection copy is only for early policy/continuation
validation and text extraction, never an input to compatibility or forwarding.
Nested `extra_body` is not flattened a second time: a remaining wrapper is checked
by the target's authoritative validator. Synthetic classifier bodies follow the
same original-body contract independently of the generation body.

Preflight is not runtime admission. Native validators retain their existing
tokenizer/fallback budgeting contracts; no new token approximation is added.
Offline tools deliberately do not read tokenizer files: when one is configured,
full tokenized context is `context_deferred` (known completion-reserve bounds
still apply). Without one, the existing native conservative fallback check is
used. Native Codex aliases reuse the pure provider Chat translation/validation
and native Responses validation, including validation of translated Chat bodies.
Known-invalid requests (such as Chat `max_tokens`) are incompatible before
classifier admission or candidate construction. Supported requests report
`static_compatible_runtime_deferred`: provider auth/state and availability are
not checked. Other cloud endpoints are checked, but capabilities with no
authoritative pure validator remain `provider_validation_deferred`; names never
imply capabilities. Deferred/unknown does **not** guarantee support or live cloud
provider parity. Health, availability, provider auth state, token template
details and runtime admission remain authoritative in the base handler.

Existing semantic target headers still identify the actual serving selection,
not the advisory. Additional bounded headers are `x-slotline-semantic-mode`,
`x-slotline-semantic-policy` (running policy SHA256), optional
`x-slotline-semantic-version`, and shadow-only
`x-slotline-semantic-advisory-outcome`/`x-slotline-semantic-advisory-target`.
Rejected advisory aliases and rule IDs are not exposed. No prompts, raw output,
credential material or private paths are added. Existing target model and
physical-route identity meanings do not change. Operator status adds a bounded
`profiles` map containing mode, version, preflight and shadow classifier state,
plus the running digest; plain status renders profile mode/version/preflight.

## Offline operator tools and schema v1

These are local operator commands, **not public unauthenticated HTTP endpoints**:

```sh
slotline semantic --help
slotline semantic explain CONFIG.yaml CASE.json
slotline semantic evaluate CONFIG.yaml DATASET.json --require-match
slotline semantic evaluate CONFIG.yaml DATASET.json --min-score 0.95
```

They load and structurally validate the normal operator policy through a shared
static loader. They do not read/decrypt legacy key files, key stores, provider
account files, Hindsight credentials or tokenizer files; they do not construct a
serving runtime, contact providers, run inference, charge rate tokens or mutate
state. Actual key-store grants, expiry, secret-file security, tokenizer integrity
and live availability cannot be validated offline. Metadata-matcher class
membership is deferred only when offline key-store credentials are unavailable;
regex length/compilation and structural validation still run. Inline credential
classes and normal serving retain class-membership checks. Supply **synthetic grants**
explicitly; they never represent successful runtime authentication. The digest
uses the existing policy projection; `policy_sha256_scope` explicitly notes that
runtime key-store grant snapshots were not loaded, so that digest need not equal
the running digest when the runtime materializes key-store grants.

Only local regular files are accepted, never remote policy/dataset URLs. Static
policies are at most 1 MiB; datasets at most 8 MiB, 1000 cases, 64 KiB per line and
64 KiB per serialized case. Explain accepts one case up to 64 KiB. Recorded
classifier envelopes are at most 4096 serialized bytes, with the shared parser's
2048-byte content limit. JSON nesting has serde's normal recursion bound. Use
newlines between records for larger JSON datasets. Errors are strict, nonzero
and sanitized; stdout is nonsecret JSON (except `--help`). Inputs, raw model
outputs, config contents and private paths are not echoed.

A dataset is a single JSON object with **exactly** `schema_version: 1`,
`declared_cases: N`, and `cases: [...]`; version, nonempty count, bounds, unique
IDs and mandatory evaluation labels are checked programmatically. Each case has:

- `schema_version: 1`, bounded identifier `id`, configured `profile`, and
  `endpoint: "chat" | "responses"`.
- `grants: {"aliases": [...], "allowed_routes": [...], "cloud_allowed": false}`;
  routes default to unrestricted configured native routes, as in existing grants.
- `request`: the original request object, with `model` equal to `profile`.
- Optional `recorded_classifier`: the native choices/message envelope, containing
  the JSON content string, **not** a bare `{target, confidence}` object. It uses
  the actual shared decision parser, but cannot certify the real upstream
  adapter's model-identity/protocol checks.
- Alternatively `classifier_failure`: one of `timeout`, `busy`, `rate_limited`,
  `unavailable`, `abstained`, `invalid`. These are explicit synthetic failure
  observations, not a new live classifier wire format.
- `expected: {"state": "resolved" | "rejected" | "unresolved",
  "outcome": REASON_CODE, "target": ALIAS_OR_NULL}`. Evaluation requires it;
  explain does not. Optional `expected_advisory` uses the same label shape.

Unknown schema fields, invalid enums, conflicting recording/failure inputs,
duplicate/unknown synthetic grants,
missing evaluation labels and mismatched declared counts are errors, not skipped
records. Without a recording/failure, required classifier work remains explicitly
**unresolved with no predicted target**. A runtime fallback is predicted only
after an actual recorded failure/low-confidence reply. Shadow output separates
the baseline `prediction` from `advisory`; all predictions remain static plans,
not claims that generation or runtime authorization succeeded.

Per-case output contains labels, predictions and match flags, never request text.
Aggregates include match/mismatch counts, target/nonselection confusion counts,
route selection accuracy (including matching `<none>`), selected-only accuracy,
coverage (`selected / total`), rejected/unresolved counts, outcome counts,
synthetic unauthorized selections and advisory divergences. `match_score` requires
state, outcome, target **and any supplied advisory label** to agree. No option
means report-only (score floor zero); `--require-match` means 1.0, while
`--min-score` requires a finite `[0,1]` score. A missed threshold prints the full
report and exits nonzero. Schema errors print a sanitized error and exit nonzero.
These are **fixture selection metrics**, not answer quality, measured savings,
latency benchmarks or calibrated probabilities.

Repeatable checked-in synthetic examples (intentionally nonexistent key store;
never use this fixture policy to serve production traffic):

```sh
slotline semantic evaluate tests/fixtures/semantic-policy-v1.yaml \
  tests/fixtures/semantic-v1.json --require-match
jq '.cases[5]' tests/fixtures/semantic-v1.json > "$TMPDIR/explain-case.json"
slotline semantic explain tests/fixtures/semantic-policy-v1.yaml \
  "$TMPDIR/explain-case.json"
python3 -B tests/semantic_offline_cli.py "$SLOTLINE_TEST_BINARY"
```

## Restart rollout and rollback

Keep protected existing credential files and a known-good policy backup. In the
operator's existing policy, set `mode: shadow`, the existing `default_target`,
`compatibility_preflight: true` if desired, and `policy_version: rollout-v1`.
Evaluate labeled local fixtures, check grants/compatibility, restart through the
existing service procedure, and inspect baseline/advisory provenance. No live
policy has been edited by these examples. Rule-only shadow evidence is **not**
decision-model quality evidence; use recorded model evaluations separately.

To promote, explicitly change only the intended profile to `mode: active` and
`policy_version: rollout-v2`, validate, and restart. To roll back, restore the
saved shadow configuration (or disable the selector and pin clients explicitly)
and restart. Semantic reload changes are rejected atomically; no hot edits or
automatic promotion are supported. Preserve the documented **explicit provider
alias pinning for stateful continuations**; selectors do not migrate provider
state or turn physical-route affinity into cross-alias conversation stickiness.

Hindsight remains optional and independent: no new memory dependency or bank is
introduced. Cost/quality/latency optimization, provider-state migration, semantic
hot reload, online learning/calibration and isolated live classifier shadow are
explicitly deferred, not represented by placeholder policy knobs.

## Verification commands

`semantic_tests` includes offline rules, extraction, strict-output, configuration,
authority, policy-digest/reload and admission checks. `semantic_tests::http` uses
real loopback HTTP classifiers/backends with matching model identities and checks
the actual endpoint dispatch layer, body preservation, streaming, timeouts and
cancellation. It requires a socket-capable test environment; compile-only checks
are not acceptance passes. Existing Hindsight tests cover its separate admission
gate and same-class model callbacks and must remain in the regression suite.

The executable fixture `tests/semantic_routing_validation.py` additionally starts
the compiled gateway against synthetic local backends. Supply an actually
assigned Tailnet test address via `SLOTLINE_TEST_HOST`, the exact binary via
`SLOTLINE_TEST_BINARY`, and `--result` for durable acceptance evidence. Never point
these fixtures at production providers, installed services or real memory banks.
The cloud-denial cases use cloud-authorized synthetic keys and Copilot targets
with an explicitly absent synthetic OAuth file. Positive controls reach the
fake-account authentication error, proving target eligibility without provider
traffic or real account reads. Rust HTTP tests additionally cover independent
classifier/generation routes and shared-route quarantine. These socket-dependent
checks require parent execution; their compilation alone is not a runtime pass.
