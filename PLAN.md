# Semantic routing expansion

## Configurable hardening increment — November 13, 2026

Assigned worktree: `feat/semantic-routing`, increment base
`6acd6d559fbe6f67d571181cb5a4189d84c33e4d`. The original plan is preserved as
`PLAN.original.md` in the assigned `slotline-semantic-hardening-20261113T154746Z`
report directory. The historical delivery/parent certifications below describe
the **base**, not certification of this new candidate.

- Shared deterministic/result parsing powers serving and local operator explain/evaluate.
- Active remains the compatible default; per-profile shadow serves only the existing
  default target, with rules-only advisory and explicit classifier-not-run provenance.
- Optional normalized full-request preflight reuses native validators and capability
  contracts, including pure Codex request-shape checks. Offline configured-tokenizer
  context, unsupported cloud capability checks and runtime health/admission remain
  explicitly deferred, never guaranteed by names.
- Static tools skip all credential/key-store/account/tokenizer reads, network,
  inference, rate charging and mutation. Versioned bounded synthetic fixtures and
  CI thresholds report label agreement, not answer quality or savings.
- Optional bounded version labels, mode and preflight affect the existing digest;
  semantic reload remains atomically restart-only. Rollout/rollback are manual
  restart procedures, with provider-state pinning preserved.
- Hindsight is independent. Live classifier shadow without guaranteed isolation,
  cost optimization, provider-state migration, hot reload and online learning
  remain deferred. This increment adds no dependencies or production activation.

New validation evidence is recorded separately in the assigned report directory:
initial option-support assertions fail RED; a stronger network-killing seccomp
test also fails RED on unconditional Tokio startup and drives offline startup
separation. GREEN logs cover pure routing/CLI checks, strict clippy, compilation,
offline CLI, Hindsight CLI and Python regressions. Checked-in fixtures declare
24 synthetic labeled cases and verify their count and outcomes programmatically.
Full Rust and compiled-gateway socket acceptance are parent certification gates:
the implementation sandbox denies TCP bind and bwrap namespaces. Compilation is
not runtime acceptance; final execution receipts belong in the delivery report.

All builds and temporary fixtures use the assigned scratch root with four Cargo
jobs and mode-0700 fixtures. Source and unique evidence are retained; the parent
owns final inactive build/fixture deletion. The exact commands and results are
in the report. Independent full/ignored Rust tests, socket acceptance, specification
and security reviews gate publication; the delivery report records their outcomes
and the activity-checked cleanup measurements.

## Objective and delivery boundary

Add optional deterministic and decision-model routing without bypassing Slotline's authorization, provider validation or capacity controls.

Delivery target: `feat/semantic-routing`, based on the previously delivered Hindsight commit `5f3b462e52ab4ae95224279f88017c73049e4b4f`. This branch includes that integration. Main-branch merge, production configuration, model installation and deployment are outside this task.

## Implemented contract

- Disabled by default. Explicit selector aliases activate semantic routing; ordinary explicit-model requests do not depend on a classifier.
- Priority-ordered keyword/regex rules inspect bounded latest-user text. Ordering, case handling, ties and defaults are documented in `docs/SEMANTIC-ROUTING.md`.
- An optional existing managed Chat Completions model chooses among authorized configured targets. Strict JSON target/confidence validation, input/output bounds, deadlines and separate decision admission constrain the classifier.
- A selector grants no classifier, target-alias, physical-route or cloud privileges. Missing grants fail closed. Unsupported `cloud_allowed`/`local_only` request-policy hints, including normalized `extra_body` forms, are rejected before model work.
- Explicit default/error behavior handles invalid decisions, low confidence, timeouts and unavailable classifiers. Fallback remains an authorized attempt, not a guarantee that a failed shared physical backend becomes usable.
- Actual model calls retain normal admission and provider limits. No outer inference permit is held while awaiting internal classification. Classifiers cannot recursively select semantic profiles.
- Original generation arguments, supported tools/schema checks and streaming remain intact. Unsupported `metadata` is not silently removed to make managed Chat succeed.
- Supported stateless Responses inputs are validated as complete self-contained text messages before classification. Omitted/null type discriminators cannot hide references; partial, camouflaged and provider-specific continuation shapes are rejected. Legitimate type-less text messages remain supported.
- Safe response headers identify profile, outcome, applicable rule and selected alias. Operator status and plain CLI expose bounded semantic feature/admission state and separate selector grants without changing status authorization or physical-route maps.
- Semantic policy changes require restart. Unrelated reloads cannot reset the decision gate. Hindsight bank authorization and separate memory admission remain intact.

## Executed validation

The parent ran socket-capable checks using trusted private fixture roots, without weakening credential readers or listener restrictions:

- Formatting, strict all-target clippy, build and full test compilation passed.
- The ordinary Rust suite passed **307 tests**, with **4 normally ignored tests** subsequently run explicitly and passing. The overhead exercises used a debug test binary; these are not release-performance benchmarks.
- The actual compiled gateway passed **126 HTTP acceptance checks**. Declared, collected and unique check counts are verified.
- All **5 Hindsight CLI regression tests** passed against the same new gateway build.
- A synthetic live test used a real existing local decision model, selected the coding alias and obtained a response from a different real local generation model. Its routing outcome was `model`, not a deterministic rule or default fallback. Only synthetic text was used; this is not an accuracy, confidence-calibration or global-concurrency benchmark.
- Review findings were repaired rather than waived: invalid preservation fixtures, unhealthy shared-backend fallback assumptions, request-policy denial handling, implicit Responses references and incomplete operator status. Temporary fixtures inherit the assigned `TMPDIR`.

Unique execution logs, manifests, earlier candidates and review evidence remain outside the source repository. No shared service was deployed or reconfigured.

## Review and publication gates

- Require fresh independent specification and security reviews of the final frozen source before committing and pushing.
- Verify committed source against the tested/reviewed candidate and read back the exact remote branch SHA after publication.
- Remove only this task's inactive regenerable build/fixture outputs after fresh process, mapped-file, working-directory and open-file checks. Preserve source, unique evidence, existing services and rollback artifacts.

## Explicit limits

- Production selectors and a decision model still require operator configuration; no default production model was chosen.
- Native Codex/Copilot classifiers are not supported by the fixed classifier contract. Authorized native cloud generation targets retain their existing provider paths and permissions.
- Stored/provider-specific continuation, non-text Responses content and raw passthrough are not automatic cross-provider handoff features. Follow the documented endpoint and pinning restrictions.
- Model quality, confidence calibration, release overhead, production cutover and positive live cloud-provider behavior were not certified by the synthetic local acceptance test.
