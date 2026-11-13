# Operate the gateway

Use this guide after the [setup walkthrough](SETUP.md) succeeds. Commands assume
`slotline` is on `PATH`; otherwise use `target/release/slotline` from the checkout.
Set `CONFIG` to the policy used by the **running** listener, not an example or an
old deployment copy.

```sh
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/slotline/config.yaml"
```

## Inspect live status

```sh
slotline status "$CONFIG"
slotline status "$CONFIG" --json
```

The local CLI reads the running gateway through an owner-only Unix control
socket. It does not need a bearer key, but it must run as an account able to
access that socket and load the policy. File-backed policies require their
policy directory to be private and writable. Verifier-store policies put the
socket in the private store directory.

Without an explicit policy path, `slotline status` uses `SLOTLINE_CONFIG`, then
`$XDG_CONFIG_HOME/slotline/config.yaml` or `~/.config/slotline/config.yaml`.
Remote clients can use operator-authenticated HTTP `/status` instead. See the
[setup request example](SETUP.md#4-check-from-another-terminal) for reading a key
from disk without putting its value into command-line arguments.

Status includes route readiness, admission counts, configured credential access,
eligible alias mappings, advisory affinity leases, and upstream attempt shares.
Optional semantic routing appears separately: enabled/disabled state, sorted
configured selector IDs, live decision-admission limit/usage/availability and
closed state. Credential `semantic_selectors` lists selector grants separately
from physical-route `aliases`; these grants do not confer classifier or target
authority. Disabled configured selectors remain visible but cannot dispatch.
Unrelated policy reloads retain the same decision gate; semantic policy and its
limit require restart. See [semantic routing](SEMANTIC-ROUTING.md#operator-status).
It is operational metadata: do not publish snapshots containing private endpoint
addresses or identifying credential/route names.

## Interpret health and traffic

- A backend advertising a model is not yet a serving backend. Slotline also
  checks readiness and successful serving qualification. Capability declarations
  still require your own exact-backend testing.
- Only eligible managed routes receive new work. Busy or unhealthy backends can
  cause `503`; the configured catalog alone does not prove available capacity.
- Physical admission is shared across adapters for one backend. Passthrough is
  an exception: its requests bypass admission and do not appear as active slots.
- Traffic share means **upstream attempts**, not unique requests, token share,
  or successful completions. The five-minute view is bounded by a 10,000-entry
  recent-attempt buffer; process-lifetime counters reset on restart.
- Affinity is a capacity-tie preference. A lease does not reserve a slot,
  guarantee a warm KV cache, or load/unload a model.

Probe diagnostics show discovery/readiness/serving stage, outcome, age, duration,
and HTTP status when available. They retain the most recent failed observation
even after a successful check and reset on process restart. These checks do not
automatically discover and requalify every modality after a backend upgrade.

## Backend telemetry

Enabled direct local routes are sampled alongside the health poll. Slotline reads
backend `/metrics` with a five-second timeout and a 1 MiB response cap; adapters
are not scraped again. These measurements are **display-only**, not routing
weights, health evidence, qualification, or additional capacity.

- vLLM and Halogen can provide aggregate prompt/generation counter rates and
  speculative acceptance when suitable series exist. Halogen's draft counters
  can combine draft-head and prompt-lookup proposals; do not label them MTP-only.
- Server-reported throughput is distinct from gateway-computed counter rates
  and is not per-request speed.
- If llama.cpp returns HTTP 501 for `/metrics`, `/slots` can supply slot/activity
  data, not token throughput or speculative acceptance.
- Missing/invalid/reset counters produce unavailable values, not guessed zeros.
  The first sample after restart or scrape failure has no interval rate. With
  no proposed drafts, acceptance is unavailable even in an idle window.
- Samples become stale after three poll intervals. JSON uses `null` for
  unavailable current values; Prometheus omits those values while retaining
  availability/support gauges.

Counters cannot reliably detect every backend restart: if there is no exported
epoch and new counters exceed the prior sample, the restart may be invisible.
There is no persistent telemetry history store. See `src/backend_telemetry.rs`
for supported series and the [request audit](request-audit.md) for per-request
measurement semantics.

## Request deadlines

Managed requests have attempt and total budgets. The default route timeout is
120 seconds; `retry_policy` defaults to `max_attempts: 8` and
`total_seconds: 1800`. Accepted retry bounds are 1–16 attempts and 1–1800 seconds.
Each eligible route is attempted at most once.

`class_timeout_budgets` overrides the attempt and total budget for an
**authenticated credential class**. This example requires a credential whose
class is `interactive`; it is a policy fragment, not a complete configuration:

```yaml
class_timeout_budgets:
  interactive:
    attempt_seconds: 300
    total_seconds: 390
    min_retry_seconds: 120
    first_output_seconds: 120
    stream_idle_seconds: 60
```

Attempt/total limits are hard caps and include response-body consumption. The
optional retry floor stops later attempts when too little total time remains;
it defaults to zero and cannot exceed the attempt budget. Total budget must be
at least the attempt budget and no more than 1800 seconds.

First-output and idle timers are opt-in, positive, and no larger than the attempt
budget. Omitted/null fields disable them. First-output timing begins with the
attempt, including header waiting. Idle timing begins only after validated,
nonempty generated text, reasoning, refusal, or tool-argument progress.
Heartbeats, partial framing, role/name/usage-only events, and empty events are
not progress. Valid terminals can still finish without nonempty output.

Progress timers pause while the downstream consumer suspends delivery; hard
limits do not. Enforcement is pull-driven: a hard deadline reached during
suspension is noticed on the next poll, not by an independent slow-client
watchdog. Before SSE handoff, eligible failures may retry under the remaining
budget. After handoff, failure closes the body with an error, never replays the
request, and cannot change committed HTTP 200 headers. Nonstream requests and
[passthrough aliases](full-passthrough.md) do not use these progress timers.

Choose thresholds from your measured workload. A longer deadline does not make
generation faster. Set client deadlines deliberately, validate configuration,
and use an authorized safe update rather than copying another deployment's
workload budgets.

## Reload policy or restart safely

```sh
slotline --check-config "$CONFIG"
slotline reload "$CONFIG"
```

`--check-config` validates policy and credential material, not live inference.
`reload` asks the running gateway to validate and atomically activate a candidate;
an invalid candidate leaves its active generation unchanged. In-flight requests
retain their policy snapshot and share the same physical admission pool with
new requests.

The supported reload scope is deliberately narrow: reviewed local-Qwen
alias/catalog, priority, and matcher edits. Listener address/port, credential
authority, every ordered route declaration, and upstream credential bytes must
stay unchanged. Reload cannot add, remove, resize, reorder, enable, disable, or
retarget physical routes. It is not a general-purpose backend swap command.
Credential lifecycle changes use the separate [key manager](../KEYS.md).

For changes outside that scope:

1. Preserve the exact policy and binary needed for rollback outside Git.
2. Validate the new policy and check the affected client/backend contract.
3. Drain active work, or get explicit authority for an interrupting restart.
   Do not use a zero slot count as evidence of no passthrough traffic.
4. Replace/restart only the owning service. For the setup guide's optional unit,
   use `systemctl --user restart slotline.service`.
5. Verify the intended binary/policy, authenticated catalog, route health, and a
   real completion. Verify the restore procedure before migrating more clients.

For the example user service, inspect `systemctl --user status slotline.service`
and `journalctl --user -u slotline.service`. These commands do not establish the
identity or health of an independently installed service with another name.
