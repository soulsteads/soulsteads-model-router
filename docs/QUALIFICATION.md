# Observed compatibility

These are bounded development observations, not certification of every model or
client. Recheck the exact deployment using [setup](SETUP.md) and
[development checks](DEVELOPMENT.md). The gateway never treats this table as an
authorization grant or enables a backend automatically.

| Client / backend | Observed result |
| --- | --- |
| Python standard-library HTTP / local Qwen `qwen38`, vLLM `0.30.0-5a4b0775` | Authenticated catalog, actual text completion, complete SSE, alias and capability denials, capacity rejection, restrictive reload, restart, deterministic rules and an actual local decision-model selection passed |
| OpenCode `1.19.5`, OpenAI-compatible provider / same Qwen | Actual Bash tool invocation, successful Python script output and tool-result continuation passed through managed Chat Completions |
| Codex CLI `0.162.1` / managed Responses to local Qwen | Historical checks exercised ordinary aliases and a known-model alias that supplied developer tool declarations and namespaced custom tools. The backend remained local Qwen; the alias does not imply cloud-model equivalence. Other local models, hosted tools, and broad task-quality parity remain unqualified. |
| Codex CLI `0.162.1` / native Codex provider with CLI auth reuse | Actual provider completion and Bash/Python tool execution with successful tool-result continuation; native assistant phases preserved |
| Native Copilot provider adapter | Unit/protocol checks only in this qualification; no live-provider claim |
| Other clients, image/embedding/schema routes, sustained workload ceilings | Require separate exact-deployment qualification |

The test deployment used explicit authenticated loopback mode, one configured
local route, a fresh operator credential, and no cloud grant or fallback. Missing
auth returned401, an unauthorized alias403, unsupported tools on a text-only alias
400, and an overlapping request above the credential's in-flight bound429.
Tool qualification used a separate restarted policy explicitly advertising tools;
passing a text request alone did not establish that capability. Admission limits
are process-local; these observations do not establish global GPU capacity.

Reducing a Qwen alias context ceiling was accepted by the restrictive reload
interface. Listener changes were refused, and a clean stop/start restored real
inference. Reload remains restricted to reviewed local Qwen routes; other model
families require restart. This qualification does not broaden that authority.

## Repeat a bounded client check

Start an isolated policy as in [setup](SETUP.md), using your real backend and a
fresh credential. Use `listener_mode: loopback` with `bind: 127.0.0.1` for same-host
clients. Keep unrelated services and native client configuration unchanged.

For the observed OpenCode path, select an OpenAI-compatible provider whose
`baseURL` is the router's `/v1` endpoint and whose `apiKey` is supplied through an
environment reference. Select an authorized alias as the model. Declare `tools`
on the alias and route only after separately testing the backend's tool support.
Use an isolated working directory and a harmless local script with a calculated
output; ask the client to execute it and report the actual output. Check the
native tool event, exit status, final answer and gateway request audit together.
A returned marker without a corresponding tool event is insufficient evidence.

Do not treat any client-compatibility refusal as a reason to silently strip
unknown client fields, switch providers, enable cloud grants, or use passthrough.
[Explicit passthrough](full-passthrough.md) has a different enforcement contract
and is not qualified by these managed-mode observations. Native CLI auth reuse in
the optional Codex provider is distinct from using Codex CLI as a gateway client.

The native Codex route is a separate, explicitly authorized cloud deployment:
provider enabled, concrete alias grant, `allowed_routes: [codex]` and
`cloud_allowed: true` are all required. The check read the existing native CLI
login through `auth_source: codex_cli` without copying credentials or writing its
configuration. It is not a fallback for a local request. The native adapter
accepts assistant `phase: commentary` and `phase: final_answer` on message items,
validates them separately, and preserves them in forwarded tool continuations.
Managed-local tool projection is described separately in `ROUTING.md`.

## Limits to verify before switching traffic

The observations above do not establish reliable streaming on every backend.
An earlier managed Responses session failed stream validation before a tool ran;
later successful sessions did not establish its cause. The public
[development checks](DEVELOPMENT.md) exercise separate synthetic failure cases.

Policy session pins are held in memory and scoped to an explicit client identity,
credential, and policy alias. Restart or expiry can reject continuation rather
than transfer tool or reasoning state to another provider. See the
[routing guide](ROUTING.md) for the supported continuation contract. Test with a
local-only key first. A native cloud route requires its own explicit grants.
