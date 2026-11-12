# Rules and decision-model routing

Routing policies add **virtual model aliases** in front of existing managed local,
Codex, and Copilot aliases. Existing aliases are unchanged when
`routing_policies` is omitted. Policies do not host models or grant permissions.

## Selection order

For authenticated Chat Completions and Responses requests:

1. Evaluate rules in their configured order. The first matching rule wins.
2. If no rule matches and `decision_model` is configured, ask that local model
   to choose from `decision_targets` that the caller is authorized to use on
   this endpoint.
3. Otherwise use `default_target`, if configured.
4. If nothing selects a target, reject the request.

Predicates within a rule are AND-combined. Lists of credential classes or
endpoints match any listed value. Omitted predicates do not constrain matching.
Rules can inspect the authenticated credential class, endpoint (`chat` or
`responses`), `stream`, nonempty `tools`, image content, and minimum/maximum
**full inbound JSON body bytes**. Byte bounds are inclusive; they are not token
counts. Caller headers cannot change the authenticated class or cloud permission.

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

## Decision-model contract

The judge is a managed **local** chat alias, including a qualified local Headroom
adapter; native cloud, passthrough and embedding judges are not accepted. Its
request is nonstreaming and tool-less, with an output cap of 128 tokens. It
receives a fixed selector instruction, the optional operator guidance and the
serialized original request as untrusted task data. No caller headers or client
credentials are included. The original request must fit within 32 KiB; larger
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

Judging consumes one physical route permit at a time under the caller's existing
class permit; it releases the judge permit before target admission. Routing does
not debit a second client rate token or reacquire class capacity. Cancellation
releases owned permits. Decision failures never trigger an implicit cloud
fallback.

## Visibility and operations

`/v1/models` includes a virtual alias only for authorized callers with a usable
configured target (and an authorized judge for decision-only policies). Responses
use the **concrete selected alias** in model fields, following that target's
existing adapter contract; the virtual alias is not rewritten into SSE payloads.
Routing logs contain bounded policy/selection identifiers, not prompts or judge
content. The policy fingerprint includes ordered rules, decision settings and a
hash of operator guidance. Changing routing policy authority requires a restart;
the restricted live-reload path does not admit it.

This is source/test support, not live model qualification. Synthetic upstream
tests exercise routing and denial behavior; configure and qualify your actual
judge and providers before an explicitly authorized deployment. Merely declaring
a model does not prove useful routing quality, latency or provider entitlement.

## Choosing cloud versus local: recommended qualification

Treat local/cloud as a placement choice, not a quality ranking. A particular
local model may outperform a particular cloud model on a narrow task. Qualify
exact model and backend versions against representative, sanitized tasks.

1. **Enforce hard constraints first.** Keep sensitive workloads local through
   trusted credential classes and grants. Check endpoint, capabilities, context
   and any provider-specific continuation requirements. Neither prompt content
   nor the judge may override a cloud denial. The current selector prefilters
   grants/endpoints; complete request capability/context validation remains at
   the concrete target. A permitted target can still reject an unsupported
   request, without trying another provider.
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

Prefer routing at a task or conversation boundary for tool-heavy agents.
Per-request policies currently reevaluate every request: they do **not** pin a
virtual alias to a provider across turns. Physical local-route affinity is not
cross-provider conversation pinning. Keep opaque reasoning, tool continuation,
context-transfer semantics and cache reuse in mind before enabling automatic
per-turn switching. A future session-pinning or measured-profile feature needs
its own implementation and qualification; this guidance does not claim either
already exists.
