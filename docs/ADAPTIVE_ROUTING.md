# Adaptive routing: outcome qualification

The first adaptive-routing building block is an **offline outcome scorecard**.
It evaluates operator-supplied, reviewed results for exact model/backend versions.
It does not train a model, change a running router, grant cloud access, replay
requests or generate new inference traffic.

## Why start here?

A useful learning loop needs an outcome signal independent of the routing judge.
HTTP success, plausible prose, judge confidence and user silence are not quality
labels. Suitable evidence includes executable tests, validated extraction against
an expected result, and explicit human review.

The evaluator checks the structure and consistency of supplied evidence metadata.
It does **not** fetch an evidence reference or prove that the claimed test or
review occurred. Admission of real feedback remains an operator responsibility;
references must not be presented as independently verified merely because a JSON
file parsed successfully.

## Run the offline evaluator

```sh
slotline routing evaluate dataset.json > qualification.json
```

This command runs before gateway configuration, authentication stores, tracing
setup or provider initialization. It does not start a listener. Failures return a
nonzero exit status and a fixed diagnostic without echoing input or file paths.
Successful output is JSON on stdout. No configuration or report file is written
unless the caller redirects stdout.

A **synthetic format example**, not real model qualification evidence:

```json
{
  "schema_version": 1,
  "outcomes": [
    {
      "scope": "example-scope",
      "workload": "example-workload",
      "task_family": "extraction",
      "case_id": "fixture-001",
      "target_alias": "local-example",
      "model": {
        "model_id": "example-model",
        "model_revision": "fixture-v1",
        "adapter_version": "fixture-adapter-v1"
      },
      "outcome": "quality_pass",
      "evidence_kind": "human_review",
      "evidence_ref": "synthetic-review-001",
      "latency_ms": 12
    }
  ]
}
```

The default criteria require at least 20 reviewed cases and a success rate of
9500 basis points. This one-case example therefore has insufficient samples.
Optional `settings` can set `minimum_samples` (1–10000) and
`minimum_success_bps` (1–10000). These are caller-selected empirical criteria,
not confidence bounds; lowering them does not establish model reliability.

Outcomes are `quality_pass`, `quality_fail`, `infrastructure_failure` or `unknown`.
Quality outcomes require both `evidence_ref` and one of `executable_test`,
`schema_validation` or `human_review` as `evidence_kind`. A schema-validation
label must represent the task's actual acceptance criteria; structural JSON
validity alone does not establish that extracted values are correct. Unknown
and infrastructure outcomes must omit evidence fields or set them to null.

The v1 format rejects unknown and duplicate JSON fields, duplicate cases within
a full model/target group, and unsupported versions. Inputs are limited to
4 MiB and 10000 outcomes. Identifiers have bounded UTF-8 byte lengths and may not
contain ASCII control characters. Latency is a required positive integer in
milliseconds, bounded to one day. Optional `cost_microusd` is an actual measured
cost in millionths of a US dollar, bounded to 1000000000000 per record; missing
cost is unknown, not zero.

The report includes per-group counts, quality success basis points, empirical
status, all-record average latency, known-cost totals and coverage counts. It
marks reviewed-case coverage comparability separately from individual threshold
status. A qualified group can still have incomparable coverage. A single group
or matching empty sets do not establish a useful model comparison. The report
never chooses a preferred target, even when coverage matches; every
`preferred_target` is null and `advisory_only` is true.

## Isolation and counting

- Keep scope, workload and task family separate. Work data must not teach a
  personal workload implicitly, or vice versa.
- Keep target alias, model identifier, model revision and adapter version separate.
  New versions need new evidence rather than inheriting an older model's scores.
- Count reviewed quality passes and failures in the quality denominator.
  Infrastructure failures and unknown outcomes are visible but do not become
  quality failures or successes.
- Reject duplicate case/model identities instead of inflating sample counts.
  The same case may be evaluated on alternative models for a paired comparison.
- Report inadequate sample counts explicitly. A caller-selected sample minimum
  and pass-rate threshold are empirical criteria, not a statistical guarantee.
- Compare models only on matching reviewed-case coverage. A model that saw only
  easy cases must not become the apparent winner over a model tested on harder
  cases. Even matching case identifiers require a trustworthy corpus and grader.
- Report measured latency and supplied actual cost without inventing missing
  values or interpreting provider token usage as a dollar price.

Use opaque identifiers, not prompts, answers, personal names, private paths or
customer content. This is data minimization by contract, not an automatic PII
scrubber: operators remain responsible for the contents of imported strings.
The evaluator reads the explicitly supplied dataset and writes its JSON report
to stdout; redirect that output only to an appropriately scoped destination.

## Safety boundary

The report is advisory. Its qualified set is not a new authorization source and
must not be connected directly to `cloud_allowed`, credential aliases, native
route grants, spend limits or tool permissions. It does not override current
capability/context checks or make an otherwise unsupported request eligible.

Do not run a user's tool-bearing request on several providers just to create
training data. Such exploration can duplicate effects, expose private content
and create unapproved cost. Use sanitized, explicitly approved evaluation cases.

## Subsequent phases—not implemented by the scorecard

1. Admit outcome evidence through a scoped, authenticated feedback path and bind
   it to the original routing decision, model/version and current policy.
2. Evaluate proposed preferences on held-out cases, keeping evaluation coverage,
   grading criteria, evidence origin and version changes explicit.
3. Publish versioned, reversible policy proposals for operator review. Preserve
   the prior accepted policy and the evidence behind each proposed change.
4. Consider narrow automatic promotion only after its privacy, cost, quality,
   freshness and rollback limits have been explicitly approved and qualified.

Operational adaptation (capacity, availability and latency) remains distinct from
quality learning. A busy local server or expired cloud credential does not teach
that a model is semantically incapable. Cross-provider session pinning and native
provider request-feasibility prefiltering remain separate work, not implied by
this scorecard. Managed-local decision candidates have a static request-fit
prefilter in the routing path; it does not consume scorecard results.
