# GitHub Copilot (native)

Slotline can send OpenAI-compatible Chat Completions and Responses requests directly to GitHub Copilot. This provider is configured separately from ordinary routes; it does not relax generic route URL policy or depend on LiteLLM.

## Configuration

Add this provider section to the Slotline configuration:

```yaml
copilot:
  enabled: true
  auth_source: managed
  models:
    - alias: public-haiku
      model: claude-haiku-5.5
      endpoint: chat_completions
    - alias: public-luna
      model: gpt-6-luna
      endpoint: responses
```

`endpoint` is explicit per model. `chat_completions` routes `/v1/chat/completions` to Copilot `/chat/completions`; `responses` routes `/v1/responses` to Copilot `/responses`. Requests retain stream and tool fields while Slotline replaces the public alias with the configured upstream model name. `/models` lists configured aliases, not an upstream-discovered catalog. Configure only models and endpoint protocols that your account supports.

The caller's credential must include the matching alias, `cloud_allowed: true`, and `allowed_routes: [copilot]`. Alias collisions with ordinary aliases or Codex are rejected. Copilot model aliases do not become generic route aliases.

Native Copilot dispatch consumes one workload token-bucket admission after those grants and the configured endpoint are checked, before provider authentication or inference. A depleted bucket returns `429 rate_limit_exceeded`; ordinary pinned passthrough aliases retain their intentional exemption.

Native inference has a 120-second attempt and total deadline by default. `class_timeout_budgets` overrides both for the authenticated workload class. Authentication/session refresh, response headers and body consumption count against the budget; the single unauthorized refresh/retry gets only the remaining total time and respects `min_retry_seconds`. For streaming requests, configured `first_output_seconds` includes authentication/header waiting and ignores heartbeats/usage-only events; `stream_idle_seconds` starts after meaningful text, reasoning, refusal or tool-argument output. Downstream pauses suspend progress clocks, never hard attempt/total deadlines. Pre-header timeouts return a redacted `504`; post-header timeouts terminate the body with a stable local error and release class capacity.

## Authentication

Managed authentication uses GitHub's device flow:

```text
slotline copilot login CONFIG.yaml
slotline copilot status CONFIG.yaml
slotline copilot logout CONFIG.yaml
```

The authorization code is displayed only for interactive sign-in. Slotline stores the GitHub OAuth access token in its own secure managed file. At inference time, Slotline exchanges that OAuth token for the separate, expiring Copilot session token and discovers the Copilot API endpoint. Session tokens are cached only in memory, refreshed before expiry, and invalidated/retried once after an upstream unauthorized response. Managed writes occur atomically after device authorization completes; no storage lock is held while the user authorizes the device.

A read-only source can instead be selected explicitly:

```yaml
copilot:
  enabled: true
  auth_source: oauth_token_file
  oauth_token_file: /absolute/path/to/secure/github-oauth-token
  models:
    - alias: public-haiku
      model: claude-haiku-5.5
      endpoint: chat_completions
```

The source file contains only a GitHub OAuth token, must pass Slotline's secure-file and trusted-ancestor checks, and is never modified. Do not place the Copilot inference session token in this file. `login` and `logout` are refused for this read-only source.

## Protocol and limits

- Requests go directly to the provider's session-discovered HTTPS API host. Only provider-owned `api.*.githubcopilot.com` endpoints are accepted; arbitrary configured destinations, redirects, userinfo, nonstandard ports, and URL suffixes are rejected.
- GitHub OAuth access tokens and Copilot inference session tokens are distinct. Slotline uses GitHub's documented device-flow client ID and `read:user` scope, then exchanges the OAuth token through the Copilot session endpoint.
- HTTP Chat Completions and Responses bodies, streaming events, and supported tool fields pass through without Codex-specific protocol rewriting.
- Responses WebSocket is explicitly unsupported and returns `501 unsupported_endpoint`, even though some Copilot models advertise it. `/v1/messages` is not exposed by this OpenAI-compatible router.
- Provider errors are redacted to stable Slotline errors; raw upstream bodies and authentication values are not returned.

No live model generation is performed by the implementation test suite. Protocol and auth tests use synthetic values and loopback mocks only.
