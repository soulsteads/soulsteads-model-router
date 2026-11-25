# Rules and decision-model routing

Routing policies add **virtual model aliases** in front of existing managed local,
Codex, and Copilot aliases. Existing aliases are unchanged when
`routing_policies` is omitted. Policies do not host models or grant permissions.

## Selection order

For authenticated Chat Completions and Responses requests:

1. Evaluate rules in their configured order. The first matching rule wins.
2. If no rule matches and `decision_model` is configured, ask that local model
   to choose from `decision_targets` that the caller is authorized to use on
   this endpoint. Managed-local candidates must also fit the normalized request
   and at least one enabled, authorized physical route's capabilities and
   context limit before they are shown to the judge.
3. Otherwise use `default_target`, if configured.
4. If nothing selects a target, reject the request.

Predicates within a rule are AND-combined. Lists of credential classes or
endpoints match any listed value. Omitted predicates do not constrain matching.
Rules can inspect the authenticated credential class, endpoint (`chat` or
`responses`), `stream`, nonempty `tools`, image content, and minimum/maximum
**full inbound JSON body bytes**. Byte bounds are inclusive; they are not token
counts. Optional `user_text` conditions inspect only the latest user turn.
Caller headers cannot change the authenticated class or cloud permission.

Targets must be concrete configured chat aliases. Nested policies, embedding
aliases and passthrough targets are rejected. The selected target still validates
its own request, context and capabilities and enforces its normal admission,
health, capacity and provider restrictions. A rule is not proof that a backend
supports the request. A policy selects once: a failed target does not cause
another policy choice or local-to-cloud escalation. Normal retries within that
concrete target retain their existing contract.

## Example

This fragment extends a policy that already defines `local-small`, `local-judge`
and the native cloud alias `cloud-large`:

```yaml
routing_policies:
  smart:
    mode: active             # optional; active is the default
    version: example-v1       # optional operator label, not model qualification
    rules:
      - id: interactive-small
        target: local-small
        when:
          credential_classes: [interactive]
          endpoints: [chat]
          tools: false
          image: false
          max_input_bytes: 2048
    decision_model: local-judge
    decision_targets: [local-small, cloud-large]
    decision_timeout_seconds: 10
    decision_prompt: >-
      Prefer local-small for straightforward questions and short edits.
      Choose cloud-large for difficult reasoning or complex code changes.
```

For deterministic routing without a decision model, omit all decision settings
and add `default_target: local-small`, or use only rules and reject unmatched
requests. A configured default is **not** a fallback for an unsuccessful decision.

The workload must independently authorize every alias it may use:

```yaml
aliases: [smart, local-judge, local-small, cloud-large]
allowed_routes: [local-small-route, judge-route, codex]
cloud_allowed: true
```

Use your actual physical route names. Replace `codex` with `copilot` for a native
Copilot target. A virtual alias alone does not grant access to its targets or
judge. Cloud still requires provider enablement, the concrete model alias,
`cloud_allowed: true`, and the explicit native route grant. Empty `allowed_routes`
never grants cloud access. This applies to file credentials and verifier-store
policies alike. No existing key is modified automatically.

## Text conditions

Add text matching inside the existing `when`; it AND-combines with the other
predicates and retains configured rule order:

```yaml
when:
  credential_classes: [batch]
  user_text:
    keywords: [debug, Rust]   # any literal substring; no word-boundary inference
    case_sensitive: false   # default; Unicode case-insensitive regex matching
```

Specify either nonempty `keywords` or a nonempty `regex`, never both. Keywords
are escaped as literals. There are at most 16 keywords of 1–128 UTF-8 bytes each,
or one pattern of 1–512 bytes. The existing Rust regex engine validates and
compiles the condition once when configuration is deserialized, with 64 KiB
compiled-program and DFA cache limits. `case_sensitive` sets the engine's initial
case mode; supported inline regex flags can override it within a pattern.

For Chat, extraction uses the last `role: user` message. For Responses, a string
`input` is user text; array input uses the last user message (`type: message` or
omitted). String content is used directly; `text`/`input_text` parts are joined
with newlines. Images and other parts in that turn are ignored for text matching.
Earlier user turns, system/assistant/tool content, function outputs and opaque
reasoning never supply matching text. A latest user turn without textual content
is a predicate non-match, even if an earlier turn had text. It does not reject a
request globally or remove supported history. Existing full-body shape and byte
conditions still inspect the full normalized inspection shape and original
inbound byte count respectively.

## Shadow mode

`mode: shadow` requires an explicit `default_target`. It serves that independently
authorized baseline through normal target validation, availability and admission.
Only the first deterministic rule supplies possible advice. Advice is omitted
when that target lacks a grant or fails known request checks. Shadow never invokes
the judge, generates an advisory answer, takes another class permit or rate debit,
or falls back to the advisory target. A denied or incompatible baseline fails.
Its actual upstream attempts follow that baseline's existing retry contract.

```yaml
routing_policies:
  trial:
    mode: shadow
    version: example-v1
    default_target: local-small
    rules:
      - id: batch-advice
        target: cloud-large
        when:
          credential_classes: [batch]
```

Active mode retains rule → judge → default selection. An active caller need not
have a grant for an unused default; a configured judge failure never uses it.

## Decision-model contract

Managed-local decision candidates reuse the concrete Chat/Responses validators
on the normalized request, with each candidate's alias, tokenizer and thinking
settings. They must have an enabled, credential-authorized physical route that
meets the required context, tools, structured-output, supported modality and
streaming contract. An unauthorized capable route cannot rescue an incompatible
permitted route. This is a static request-fit check, not a health/capacity probe
or a reservation; final target admission and validation still run normally.

Native Codex candidates additionally run the same pure Chat/Responses projection
and validators as native dispatch, including stream typing and exactly one
`extra_body` merge without a managed thinking rewrite. These checks use the
original request, not the already normalized inspection copy. Copilot retains
its permission/endpoint checks and original-body forwarding contract; account,
network and provider capability checks remain at concrete dispatch. Rules and
active default-target selection retain their concrete-handler validation. If filtering removes every candidate, the request fails before
judge inference; the existing `model_not_allowed` error covers that empty set.
A judge choice outside the filtered list is invalid, even if the alias is
otherwise authorized. A single remaining candidate still goes through the
configured judge; failures do not become a fallback or bypass.

The judge is a managed **local** chat alias, including a qualified local Headroom
adapter; native cloud, passthrough and embedding judges are not accepted. Its
request is nonstreaming and tool-less, with an output cap of 128 tokens. It
receives a fixed selector instruction, the optional operator guidance and the
serialized **normalized inspection copy** as untrusted task data. This copy merges
one level of `extra_body` and applies the managed thinking rewrite. Generation
dispatch preserves the original body except for the selected model, then applies
the selected provider's existing projection once. No caller headers or client
credentials are included. The serialized inspection copy must fit within 32 KiB; larger
unmatched requests are rejected rather than silently truncated or exported.

The judge must return exactly one JSON object as its assistant content:

```json
{"target":"local-small"}
```

Unknown or duplicate fields, unconfigured/unauthorized targets, truncated output,
tool calls, invalid JSON, unavailable capacity and judge failure reject the
request before target inference. Decision input is not a source of authority:
only deterministic checks can admit the selected target. Guidance is at most
4096 bytes, target lists are bounded and unique, and the judge timeout defaults
to 10 seconds (allowed range 1–30). A separate bounded judge step precedes the
selected target's existing inference deadlines; it does not replace them.

A request that already carries continuation state is never judged. State means Chat
`tool`/`function` messages, `tool_calls`, `tool_call_id`, `function_call` or
`reasoning_content`; Responses `previous_response_id`/`conversation`,
`encrypted_content`, and any input item whose type is not `message`, including
`function_call(_output)`, `custom_tool_call(_output)` (Codex `apply_patch`),
`local_shell_call(_output)`, `mcp_*`, `tool_search_*`, `web_search_call`,
`file_search_call`, `computer_call(_output)`, `code_interpreter_call`,
`image_generation_call`, `reasoning`, `compaction` and `item_reference`. Unknown
future item types fail closed. Such a request is never judged when the authorized candidates span
more than one provider kind (local, native Codex, Copilot). It fails with HTTP 409
`continuation_pinned` before any judge inference or target dispatch, so a decision
model cannot move a tool or stateful continuation between providers. Rules and
defaults are deterministic operator configuration and are unchanged. The router
keeps no session: a client that explicitly names a different concrete alias in a
later request is making its own choice, still limited by that credential's grants;
the router cannot prove which provider produced earlier history.

Judging consumes one physical route permit at a time under the caller's existing
class permit; it releases the judge permit before target admission. Routing does
not debit a second client rate token or reacquire class capacity. Cancellation
releases owned permits. Decision failures never trigger an implicit cloud
fallback.

## Session pins (memory only)

For an `active` policy the router remembers, in process memory only, which target it
placed a conversation on, **but only for conversations that carry an explicit client
session identity**: `prompt_cache_key`, else `client_metadata.session_id`/`thread_id`
(the Codex CLI sends these). Identity is never derived from conversation content, so
two conversations share a pin only if the client says they are the same. The pin is
scoped to the authenticated credential and the requested policy alias, bounded (4096
pins in total, at most 512 per credential; a credential at its cap evicts only its own
oldest pin) and expires 4 hours after last use. Nothing is written to disk and a restart
loses every pin. The first placement wins: a pin is never overwritten by a later
decision for the same identity until it expires, is evicted, or the router restarts.

Order of handling for a request to a policy alias:

1. Eligibility is computed first (alias/route/`cloud_allowed` grants, capability,
   context fit). A pin never adds access.
2. If a pin exists for the identity, the request goes to the pinned target (reason
   `pin`), with no rule evaluation and no judge call, whether or not this turn carries
   tool state. If the pinned target is no longer one of the policy's current,
   authorized targets or no longer fits, the request is refused with HTTP 409
   `pin_unavailable`.
3. Without a pin, an `active` policy refuses every request carrying tool or
   continuation state (see above) with HTTP 409 `continuation_pinned`, before rule
   evaluation or a judge call. This covers a restart, eviction, expiry, a conversation
   this router never placed, and **every client that sends no session identity (for
   example OpenCode today): such clients get no pinning and their tool continuations
   are refused.** This remains fail-closed even when every eligible target has the same
   provider kind: distinct local routes/models can still have incompatible continuation
   state. No judge or upstream call is made on a refusal. Shadow policies deliberately
   retain their baseline-only, no-pin advisory behavior and are not changed by this
   active-policy fence.
4. Otherwise rules, defaults or the decision model place the request, and, if the
   request has a session identity, the placement becomes that conversation's pin.

Conversations without a session identity and without tool state (plain text turns) are
decided afresh each turn and may be placed on different providers turn to turn,
carrying their history with them, subject to eligibility; a local-only credential
still never reaches cloud. Shadow policies do not pin. The pin never replays work; a
failed pinned target returns its own error. Known limit: a client that names a
different concrete alias itself is making its own choice.

## Decision records (internal, unstable)

Each routed request emits one `routing policy selected` event (or `routing policy
refused`) in the existing JSON audit channel, joined to the request's other audit
events by `request_id`. Fields may change without notice and are not a public API:
`policy`, `target`, `reason` (`rule`, `default`, `decision`, `pin`, `shadow`),
`rule`, `mode`, `provider_kind` (`local`, `codex`, `copilot`), `candidates` and
`candidate_kinds` (authorized route/endpoint candidates before per-request
tool/context fit checks; dispatch still requires those checks), `continuation_state`, `pin_hit`, `session` (short non-reversible hash),
`decision_ms` (time spent deciding, including any judge inference), `advisory_target`/
`advisory_kind` in shadow mode, and on refusal `refused` and `pin_present`. Queue/retry
time, elapsed time, first-output time and reported tokens are on the same request's
terminal `request body completed` event. `scripts/decision_summary.py` is a minimal offline summariser that joins the two events
by `request_id` and reports request and reported-token share by provider kind and
decision overhead; token share covers only requests that reported usage. Prompts, tool
arguments and raw session identifiers are never logged.
No response header or API exposes them.

When concurrent first requests for one explicit session race a decision-model call,
only the first placement is retained. A conflicting later decision returns HTTP 409
`pin_conflict` before target dispatch; retry uses the recorded placement. Pins remain
memory-only, scoped to credential/policy and rechecked against current grants and fit.

## Local Responses compatibility for Codex CLI

On managed local routes the Responses path validates, bounds and consumes Codex
bookkeeping instead of rejecting it: `client_metadata` (at most 32 short string
entries), `prompt_cache_key` (at most 512 bytes) and `include:
["reasoning.encrypted_content"]` are removed before the local backend sees the request
and are never forwarded or shown to a decision model. Opaque `reasoning` items are
dropped (they are provider-bound) and assistant `phase` is removed. Tool catalogs
may arrive in top-level `tools` or developer `additional_tools` input records.
The latter are declarations, not continuation state; actual calls/results still
trigger continuation fencing.

A single namespace level containing function/custom tools is projected to unique,
bounded backend names. Custom tools use a single string `input` parameter locally.
Responses restore the original namespace/name and custom freeform input, including
SSE and terminal output records; JSON-escaped custom input fragments are withheld
until the complete bounded argument is available. Native clients retain execution
and permission enforcement. The adapter creates no tools or authority of its own.
Catalogs are limited to 128 tools. Conflicting declarations, nested namespaces,
invalid custom arguments and unknown calls during projected-tool output are refused.
Only backend function calls can become native tool calls: unsolicited native/custom
items or native tool-input events are refused before exposure, and namespaces are
restored solely from the request catalog. Tool-based routing predicates include
developer catalogs as well as top-level declarations.
Repeated identical declarations are allowed. Tool description truncation preserves
Unicode boundaries.

Hosted tools (`web_search`, etc.) are dropped from the local request and listed in
the internal compatibility record; choosing one by `tool_choice` is refused.
Unsupported execution-history items (`local_shell_call`, `mcp_call`,
`item_reference`, etc.) return HTTP 400 `unsupported_input_item`. Native Codex
provider requests bypass this managed-local adapter. A familiar client model slug
is still only a configured alias: it does not change the actual backend/model or
establish cloud-model quality equivalence.

## Visibility and operations

`/v1/models` includes a virtual alias only for authorized callers with a usable
configured target (and an authorized judge for decision-only policies). Responses
use the **concrete selected alias** in model fields, following that target's
existing adapter contract; the virtual alias is not rewritten into SSE payloads.
Routing logs contain bounded policy/selection identifiers, not prompts or judge
content. Shadow catalog visibility depends on its baseline grant, rather than an
unused judge or advisory target. Catalog `model_info`, protected operator audit
status and the status CLI include routing mode, optional version and policy hash.
Changing mode, version, text conditions or other routing authority requires a
restart; the restricted live-reload path does not admit it.

`version` is an optional 1–64 byte ASCII alphanumeric/dot/dash/underscore label.
It is not a model version or certification. The existing audit `policy_sha256`
now uses schema 5 and retains the Hindsight and prior identity fields while adding mode, version
and text authority. See [request audit](request-audit.md) for its wider scope.

Responses selected by a virtual policy carry these server-owned headers:

- `x-soulsteads-router-routing-policy-sha256`: SHA-256 of canonical JSON containing
  schema 1, the virtual alias and its complete ordered policy descriptor (mode,
  version, rules/conditions/targets, default, decision model/targets/timeout and
  SHA-256 of operator guidance).
- `x-soulsteads-router-routing-config-sha256` and `x-soulsteads-router-routing-generation`: the
  request's effective existing audit policy identity and runtime generation.
- `x-soulsteads-router-routing-mode`, `x-soulsteads-router-routing-reason`, and optional
  `x-soulsteads-router-routing-version`.
- Optional `x-soulsteads-router-routing-rule-sha256` hashes the matched rule ID;
  `x-soulsteads-router-routing-advisory-sha256` hashes independently permitted shadow advice.

Raw rule IDs are never put in headers, so existing Unicode IDs remain accepted.
Values are bounded ASCII labels, decimal generations or hexadecimal hashes.
Upstream `x-soulsteads-router-routing-*` values cannot supply this provenance on virtual,
ordinary managed, native provider or passthrough routes. Denied advice is absent.
The per-policy hash excludes concrete route/alias configuration and grants; the
wider config hash includes its documented projection. Neither identifies live
capacity/health, tokenizer state, secret rotation or provider account state.

This is source/test support, not live model qualification. Synthetic upstream
tests exercise routing and denial behavior; configure and qualify your actual
judge and providers before an explicitly authorized deployment. Merely declaring
a model does not prove useful routing quality, latency or provider entitlement.

## Offline selection explanation and fixtures

```sh
soulsteads-router routing explain CONFIG.yaml CASE.json
soulsteads-router routing evaluate-selection CONFIG.yaml DATASET.json --require-match
soulsteads-router routing evaluate-selection CONFIG.yaml DATASET.json --min-score 0.95
```

These commands branch before runtime creation. They read only the two specified,
regular input files, each bounded to 8 MiB, with sanitized errors. They never call
`config::load`, read keys, verifier stores, tokenizer/account/secret files or
application caches, create a runtime or use network access. Provider configuration
and identity validation is pure: it checks declared values and path syntax, without
probing account paths or any ancestor `.git` entries. Serving configuration loading
retains account-storage filesystem guards and credential/tokenizer readers; those
storage checks are deferred offline. Static routing and route-reference validation
does not replace the live `--check-config` command.
The shared pure planner, request projection, authorization checks and target-only
judge parser also serve HTTP dispatch. The separate reviewed outcome-quality
command remains **`routing evaluate DATASET.json`**, with its existing schema.
Selection fixture agreement is not answer quality.

Each case is strict schema 1 and supplies an explicit synthetic credential class,
alias/physical-route/cloud grants, the exact raw inbound JSON body string and its
UTF-8 byte count. There is no assumed interactive class, implicit alias grant or
reserialization-based byte estimate. For example (the byte count includes exactly
the string shown):

```json
{
  "schema_version": 1,
  "id": "synthetic-rule",
  "endpoint": "chat",
  "grants": {
    "synthetic": true,
    "credential_class": "interactive",
    "aliases": ["smart", "local-small", "local-judge", "cloud-large"],
    "allowed_routes": ["local-small-route", "judge-route", "codex"],
    "cloud_allowed": false
  },
  "raw_body": "{\"model\":\"smart\",\"messages\":[{\"role\":\"user\",\"content\":\"Rust\"}]}",
  "input_bytes": 63,
  "expected": {"state": "deferred", "outcome": "rule", "target": "local-small"}
}
```

`target` is the planned logical target, not evidence that inference can run.
Results explicitly defer `runtime_availability`, configured `tokenizer_context`,
provider account/network facts and unavailable Copilot request capabilities.
With a configured tokenizer, only prompt sizing is deferred. The parsed completion
reserve (including the default) must still fit the declared alias context and at
least one enabled, authorized physical route with the required capabilities. An
unauthorized larger route cannot satisfy that bound. Offline does not read the
tokenizer or use a prompt estimate to reject or certify that request. Without a
configured tokenizer, the shared serving estimate applies. A retained target is
still subject to prompt sizing, runtime and provider checks during serving.
If judging is needed, no supplied result yields `judge_not_run`, no target and no
fallback. Optional `judge_response` supplies the complete synthetic Chat response
envelope; it must satisfy the same single-choice, stopped, tool-less, target-only
JSON parser and filtered candidate constraints as serving. It never proves the
judge could actually run. Unused judge recordings are rejected.

A dataset is `{"schema_version":1,"declared_cases":N,"cases":[...]}`. It requires
1–4096 uniquely named cases with `expected` state/outcome/target. Unknown fields,
versions, duplicate grants/IDs, missing explicit facts and inconsistent byte or
case counts fail. Output is deterministic and includes per-case predictions,
deferred facts, candidate lists, authorization assertions, exact manifest counts,
outcome totals, expectation match rate and planned-target coverage. CI options
fail on mismatches or a finite 0–1 minimum match rate. They measure selection
contract agreement; they do not generate answers or score answer quality.

Compiled acceptance fixtures are `tests/routing_offline_cli.py` (including
positive SIGSYS and strace socket controls) and `tests/routing_http_acceptance.py`.
They require an explicit `SOULSTEADS_ROUTER_TEST_BINARY`; HTTP also requires the assigned
`SOULSTEADS_ROUTER_TEST_HOST`. Supply `--manifest` and a private `TMPDIR`. Restricted socket,
seccomp or ptrace environments produce blocked evidence, never runtime passes.

## Choosing cloud versus local: recommended qualification

The [offline outcome evaluator](ADAPTIVE_ROUTING.md) can summarize reviewed
model-version evidence. It is advisory and does not feed scores into live
selection or change these routing policies automatically.

Treat local/cloud as a placement choice, not a quality ranking. A particular
local model may outperform a particular cloud model on a narrow task. Qualify
exact model and backend versions against representative, sanitized tasks.

1. **Enforce hard constraints first.** Keep sensitive workloads local through
   trusted credential classes and grants. Check endpoint, capabilities, context
   and any provider-specific continuation requirements. Neither prompt content
   nor the judge may override a cloud denial. The selector prefilters grants
   and endpoints for all candidates and request capability/context fit for
   managed-local decision candidates, with known pure Codex checks before judging.
   Provider account/network checks and final local validation/admission remain at
   the concrete target. A permitted
   target can still fail without trying another provider.
2. **Route known cases deterministically.** Use explicit rules for well-qualified
   task classes. Full request size helps enforce limits; it is not a reliable
   estimate of reasoning difficulty. A short question can need complex reasoning,
   while a large extraction task may be well suited to a local model.
3. **Judge only the ambiguous cases.** Give the local judge clear operator
   guidance about each target's demonstrated strengths. Alias names alone do
   not encode performance. The current guidance/target-list contract supports
   this manually; it does not supply measured model profiles, calibrated
   confidence, cost optimization or automatic model learning.
4. **Measure useful outcomes, not the judge's self-confidence.** Compare the same
   admitted task on eligible targets using executable tests, schema/extraction
   correctness or blinded human scoring. Record quality, unsupported-request
   rate, end-to-end latency (including judging and queueing), and actual provider
   cost. Keep a held-out set; tune rules/guidance on a separate set.
5. **Start with explicit canaries after authorization.** Compare local-only,
   cloud-only and rule/judge policies on approved fixtures. Count unnecessary
   cloud choices separately from local choices that fail the quality bar.
   No private prompt retention or live cross-provider evaluation is implied.

Cost controls are limited to grants (`cloud_allowed`, `allowed_routes`, alias
lists), per-credential `requests_per_minute`/`burst`, per-class `max_in_flight`,
per-route `safe_concurrency` and request deadlines. There is no monetary budget,
token quota or spend limit, and no queue: capacity exhaustion returns 429 or 503
immediately. Cost is reported only as usage in responses and audit records.

Prefer routing at a task or conversation boundary for tool-heavy agents.
Explicit client session identities retain their first active-policy placement in
bounded memory; subsequent requests recheck authorization and fit. Without a pin,
mixed-provider tool/continuation history is refused with HTTP 409 `continuation_pinned`.
Plain conversations without an identity are evaluated afresh, and a client that
names a different concrete alias is choosing that route itself, subject to grants.
Physical local-route affinity is separate from policy session pinning. Durable pins
and broad measured quality/latency profiles remain unqualified; current pins are
lost on restart or expiry.
