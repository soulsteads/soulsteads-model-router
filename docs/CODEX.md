# Native Codex provider

## Preferred authentication: read-only reuse of Codex CLI login

With Codex enabled, `auth_source: auto` is the default. If legacy `account_file`
is absent, Slotline reads the current file-backed Codex CLI ChatGPT session from
`$CODEX_HOME/auth.json`, or `$HOME/.codex/auth.json` when `CODEX_HOME` is unset.
Set `cli_auth_file` to override that location. If `account_file` is present,
auto preserves the existing Slotline-managed source; source selection never
falls back based on file availability. Use `auth_source: codex_cli` or
`auth_source: managed` to state the source explicitly. Ambiguous combinations
are rejected.

The CLI source accepts only `auth_mode: chatgpt` with a usable access token.
Slotline reads the file on each request, validates a bounded descriptor
snapshot, and never copies, refreshes, rotates, deletes, locks, or otherwise
writes the system auth file. The Codex CLI remains responsible for token
refresh. If the access token expires, status reports it unavailable and directs
the operator to run `codex login` locally. Keyring-only CLI auth is not read by
this file-backed source; no keyring dependency is added. Native Slotline OAuth
login is not started for this source.

For a safe readiness probe, configure `enabled: true` and
`auth_source: codex_cli` (omit `cli_auth_file` to use the system default), then
run `slotline codex status CONFIG.yaml`. Status reads the selected local file
only; it does not call a provider or change authentication state.

This is interoperability with an undocumented backend, **not live-provider
acceptance**. Do not enable real traffic without a separately authorized live
test.

## Configuration and default denial

Add this block to a normal Slotline configuration. Its alias must not also
appear in `aliases`; `codex` is reserved and must not be a local route name.
Local routes remain unchanged. Provider authority changes require a restart,
not policy reload.

```yaml
codex:
  enabled: true
  alias: codex-chat
  model: YOUR_ENTITLED_CODEX_MODEL
  auth_source: auto
  # Optional absolute override; default is CODEX_HOME/auth.json or HOME/.codex/auth.json.
  # cli_auth_file: /absolute/path/to/auth.json
```

### Optional Slotline-managed OAuth qualification (compatibility path)

Existing device authorization, token refresh, and Slotline-owned private account
storage remain available for explicit qualification. They are separate from the
preferred CLI source and should be selected explicitly:

```yaml
codex:
  auth_source: managed
  account_file: /var/lib/slotline-codex/account.json
```

The historical managed-login and refresh evidence below remains applicable only
to this managed source. Do not use `slotline codex login` for the CLI source.

The backend defaults to disabled. Every workload/key policy defaults to
`cloud_allowed: false`. In addition to normal key/verifier, name, class and rate
settings, an authenticated workload needs **all three** permissions:

```yaml
aliases: [codex-chat]
allowed_routes: [codex]
cloud_allowed: true
```

Empty `allowed_routes` does not grant Codex access. This checkout has no separate
pre-existing cloud-mode switch: provider enabled, explicit cloud permission,
model permission and explicit `codex` route permission form that boundary.
Caller headers cannot opt in. Backend model names do not create public aliases.
There is no implicit local/cloud fallback, direct-model escape hatch or provider
retry. Denied/disabled cloud models are absent from `/v1/models`.

## Operator authentication

Provision a dedicated service-owned `0700` directory outside every Git worktree.
Do not reuse another application's account file. Account/lock files must be
regular, singly linked, service-owned `0600` files. Parent directories must be
root/service-owned and not group/world writable (root-owned sticky ancestors
such as `/tmp` are permitted). Symlinks in every component, hardlinks, unsafe
ownership/permissions and `..` traversal are rejected, not silently repaired.

```sh
slotline codex login CONFIG.yaml
slotline codex status CONFIG.yaml
```

These commands authenticate only; they do not run inference. Login displays a
verification URL and one-time code on stderr. Use the code only for sign-in you
initiated. The native flow requests a user code, polls (403/404 are pending,
minimum five-second intervals, fifteen-minute deadline), exchanges the returned
authorization code/verifier, then extracts the ID-token account claim. Claim
extraction trusts the TLS-authenticated OAuth issuer; it is not a general JWT
signature verifier. Client account headers never establish identity.

Status is local-only and never refreshes. It emits only `enabled`, `state` and,
when usable, `expires_at`, never account IDs, emails or tokens. Expired state
requires refresh before inference; missing/malformed/rejected state requires
local sign-in. No provider error bodies, tokens or prompts are logged.

Refresh starts within sixty seconds of expiry. A no-follow private file lock
serializes tasks/processes; waiters reread after acquiring it. A durable
refresh-in-progress marker makes cancellation or uncertain token rotation fail
closed instead of replaying a consumed refresh token. Success persists rotated
tokens. Omitted replacement refresh/ID tokens retain prior values; missing
access tokens, invalid expiry/claims and account changes fail. Storage uses
directory-FD-relative no-follow opens, exclusive same-directory temp creation,
file fsync, atomic rename and directory fsync. Keep backups/crash dumps private.
Provider HTTP 401 also invalidates the matching account token without replay;
a stale in-flight failure cannot invalidate a newer refreshed token.

## API contract and limitations

- Both existing production HTTP handlers use authenticated rate buckets, class
  concurrency and body limits. A separate fixed eight-request Codex ceiling
  applies. Permits last through body completion/cancellation, including buffered
  nonstream results. No GPU/KV reservations, local selection, health qualification,
  model load or unload is used for Codex.
- Upstream requests always stream Responses with `store: false`, instructions
  and `include: [reasoning.encrypted_content]`. Account bearer and account ID,
  originator, user agent and content headers are internally constructed. Caller
  authorization, cookies, API keys and all other headers are never forwarded.
- Native text, function definitions/calls/results, reasoning summaries and
  encrypted reasoning items are retained. Replay calls before their
  `function_call_output` items: this implementation is stateless. Completed item
  events recover output omitted by the terminal envelope. Nonstream results
  require a validated terminal; incomplete responses remain incomplete.
- Chat messages/tools/results are explicitly translated, never sent unchanged.
  System/developer roles, text/refusal/reasoning-summary deltas, incremental
  function arguments, tool-only output, finish reason and usage are converted.
  Chat `reasoning_effort` maps to native reasoning effort. `[DONE]` follows only
  validated completion. Usage chunks require `stream_options.include_usage`.
  Encrypted reasoning round trips require native Responses; Chat exposes summaries.
- Unsupported semantics return HTTP 400 rather than silently dropping controls:
  images/audio, legacy functions, structured output, stateful/previous-response
  references, background/store-true requests, automatic truncation, arbitrary
  extras, sampling controls and parallel-tool controls. Requested output caps
  are also unsupported by the pinned transport: omit Chat `max_tokens` /
  `max_completion_tokens` and native `max_output_tokens`. Conservative context
  admission uses 131072 units, not an entitlement guarantee.
- Malformed/truncated streams, missing terminals, empty completed output and
  provider failures fail closed. Streaming errors abort the body; nonstream
  errors return sanitized gateway errors. No replay after possible tool effects.
  Limits: 1 MiB/SSE frame, 16 MiB/upstream stream, 128 KiB/OAuth body or account
  file. Error bodies are discarded without buffering. Class attempt/total/
  first-output/idle budgets apply; without a class budget the absolute ceiling
  is `min(300, retry_policy.total_seconds)` seconds.

## Metrics

Observed native inference HTTP, transport and protocol failures increment the existing `slotline_upstream_errors_total` counter once per request. This includes body errors observed while relaying an SSE response after its 200 headers. Successful requests, client cancellation, policy/input refusals and missing-account preflight failures do not increment this inference counter. The existing operator-only metrics authorization remains unchanged.

## Network and verification boundary

Defaults are `https://auth.openai.com` and
`https://chatgpt.com/backend-api/codex/responses`, verified TLS on port 443.
Overrides `auth_base_url` / `responses_url` allow only those production endpoints
or explicit **numeric loopback HTTP fixtures**. Remote HTTP, other HTTPS origins,
userinfo, query strings and fragments are rejected. All redirects are disabled;
ambient proxy configuration is ignored. Never use real credentials in fixtures.

Protocol/auth unit tests are network-free. `codex_integration_tests` starts
synthetic loopback OAuth/provider servers and exercises production Axum HTTP
handlers. Run `cargo test --locked codex_` on a host permitting loopback and
private temporary storage. Sandbox bind denials or protected `.git` ancestors
block those fixtures; blocked tests are not passes. Real OAuth, account
entitlement and live provider acceptance remain untested.

## Attribution

Device/token/header/Responses behavior is translated from the non-enterprise
LiteLLM ChatGPT provider at immutable revision
`978b4d1e4e91cdaaab2e16fbd02d855439e143c1`:
`litellm/llms/chatgpt/{authenticator.py,common_utils.py,responses/transformation.py}`.
These files are MIT-licensed; the complete notice is retained in
`third_party/LiteLLM-LICENSE.txt`. No enterprise code or runtime is included.
The Slotline implementation is otherwise governed by the repository license.
