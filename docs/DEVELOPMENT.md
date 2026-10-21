# Development and release checks

## Default checks

```sh
cargo test --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s tests -p 'test_*.py'
```

The Rust suite uses isolated local mocked upstreams. Four audit checks are
explicitly ignored by default: two loopback-HTTP checks and two synthetic
overhead measurements. An ignored test is not a pass. Python parser/report
tests use the standard library.

Tokenizer-dependent Rust tests return early when `SLOTLINE_TEST_TOKENIZER` is
absent; the test runner can show them as passed without exercising tokenizer
assertions. The optional private-request diagnostic also requires
`SLOTLINE_TEST_REQUEST`. Do not claim tokenizer qualification from the default
summary. Supply an independently verified, model-compatible tokenizer to run
those assertions; the [optional downloader](INTEGRATIONS.md) has an access
limitation. Keep any real request fixture outside Git.

Unix control-socket tests need short temporary paths. If `TMPDIR` points into a
long report directory, choose a shorter disposable test directory rather than
changing the socket contract.

## Real HTTP fixture processes

Build the binary first. Set `SLOTLINE_TEST_HOST` to a Tailnet IPv4 address
actually assigned to your test host; the default `100.64.0.1` is illustrative,
not an address assumed to exist on your network. The gateway's current listener
restriction applies to fixtures too. Do not change production networking to run
tests. Use an isolated test host if your machine lacks a Tailnet address.

Fixtures use fixed local backend and gateway ports; run them **sequentially**,
and confirm these ports are free before starting. Their policies and credentials
are disposable and separate from an installed service.

```sh
cargo build --release --locked
export SLOTLINE_TEST_HOST=100.64.0.1  # replace with your own assigned address
export SLOTLINE_TEST_BINARY="$PWD/target/release/slotline"
PYTHONDONTWRITEBYTECODE=1 python3 tests/mock_validation.py
PYTHONDONTWRITEBYTECODE=1 python3 tests/key_store_http.py
PYTHONDONTWRITEBYTECODE=1 python3 tests/policy_reload_validation.py
PYTHONDONTWRITEBYTECODE=1 python3 tests/timeout_budget_validation.py
PYTHONDONTWRITEBYTECODE=1 python3 tests/stream_deadline_validation.py
PYTHONDONTWRITEBYTECODE=1 python3 tests/stream_progress_validation.py \
  --binary "$SLOTLINE_TEST_BINARY" --result /tmp/slotline-stream-progress-result.json
```

`mock_validation.py` exercises actual HTTP authentication, alias filtering,
capabilities, embeddings, SSE, retries, cancellation, shared physical admission,
affinity, and status CLI behavior. The other fixtures isolate key lifecycle,
restricted policy reload, request deadlines, and stream-progress semantics.

The Headroom failure fixture additionally needs an independently installed Rust
sidecar. It is not built by this repository. Set `SLOTLINE_HEADROOM_PROXY` to
its executable path, then run `python3 tests/headroom_failure_validation.py` with
the same host and binary settings. Missing this dependency is a blocked optional
integration check, not a reason to substitute simulated output.

## Explicit audit checks and synthetic benchmarks

On a host permitting loopback listeners, opt into the two audit HTTP checks:

```sh
cargo test --locked audit_attempt_timeline_successful_recovery_and_metrics -- --ignored --nocapture
cargo test --locked audit_passthrough_observes_actual_send_without_interpreting_body -- --ignored --nocapture
```

Run the overhead exercises separately in release mode, without parallel test
workers:

```sh
cargo test --locked --release audit_sink_release_overhead_measurement -- --ignored --nocapture --test-threads=1
cargo test --locked --release audit_inprocess_release_overhead_measurement -- --ignored --nocapture --test-threads=1
```

They print seven synthetic samples per mode, warmup/sample counts, source
identity, and sink counters. Comparisons cover formatting/enqueue to a null
writer and in-process admission/lifecycle with logging on/off. They exclude
inference, HTTP networking, and journal durability: they are not production
overhead guarantees or comparisons with historical source. Saturation has
separate tests. Use `--offline` only when dependencies are already cached.

## Known integration-test limitations

During public-release preparation, `mock_validation.py` failed its initial wait
for all routes to become serving, and `headroom_failure_validation.py` failed
its initial qualification wait. Both failures also reproduced with the original
fixture code. These checks are **not a green integration gate** merely because
the default Rust/Python suites pass. Reproduce and investigate them on the
intended test host before claiming release acceptance; do not weaken assertions
to hide missing qualification. The optional tokenizer download also returned
HTTP 401; see [optional integrations](INTEGRATIONS.md).

## Source boundaries

- `src/config.rs`: policy schema, credential loading and validation.
- `src/routing.rs`, `src/state.rs`: selection, shared capacity and route state.
- `src/handlers.rs`, `src/request_validation.rs`, `src/validation.rs`: managed APIs.
- `src/sse.rs`, `src/upstream.rs`: streaming and upstream lifecycle.
- `src/passthrough.rs`: explicitly enabled passthrough mode.
- `src/key_manager.rs`, `src/auth_control.rs`: verifier store and local control.
- `src/backend_telemetry.rs`, `src/status*.rs`, `src/audit*.rs`: operator evidence.
- `tests/`: Python HTTP fixtures and report/parser tests.
- `integrations/`: optional external-project patches, not automatic setup.

Keep behavior changes separate from public-release redaction. Do not silently
relax bind, authentication, capability, capacity, retry, or reload restrictions
to make a walkthrough appear easier.

## Before publishing

1. Retain the [Apache License 2.0](../LICENSE) and package license metadata.
   Review third-party dependency and external-patch notices before distribution;
   this repository's license does not relicense external projects.
2. Review all tracked files for credentials, personal data, private topology,
   and host-specific paths. Scan both the current tree and the proposed public
   Git history with a maintained secret scanner. Fixture keys are deliberately
   fake and must never be deployed.
3. Choose whether to publish a fresh sanitized history or rewrite a separate
   release copy. Existing commits and author metadata are not cleaned by a new
   branch. Do not force-push or rewrite the private repository without authority.
4. Check Markdown links, SVG assets, example policy validation, and the setup
   commands against the exact release candidate. Qualification claims belong
   to the tested backend/version, not every compatible-looking implementation.
5. Run the default checks and applicable HTTP fixtures. Record skipped,
   ignored, blocked, and pre-existing failures separately.
6. Review the final diff independently. Do not publish private runtime config,
   raw keys, verifier stores, reports containing caller data, or build artifacts.
