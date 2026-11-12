# Documentation

- [Native Codex provider](CODEX.md): opt-in authentication, permissions and offline-test boundary.

Start with the [project README](../README.md) for the overview. The public guides
below describe the current source contract, not the state of a particular
installation. The gateway still builds as `slotline`.

## First-time users

1. [Compatibility and limits](COMPATIBILITY.md): decide whether the gateway fits
   your host, backend, and client request shapes.
2. [Setup](SETUP.md): build it, prepare a private policy/key, and verify a request.
3. [Operations](OPERATIONS.md): inspect live status and update safely.

## Operators

- [Rules and decision-model routing](ROUTING.md): explicit local/cloud selection,
  bounded local judges, permissions and failure behavior.
- [Offline outcome qualification](ADAPTIVE_ROUTING.md): advisory scorecards for
  reviewed model-version results; no automatic live learning or policy changes.

- [Key management](../KEYS.md): verifier store, issuance, rotation, revocation,
  acknowledgments, and backup/restore.
- [Request audit](request-audit.md): fields, metrics, measurement boundaries,
  privacy, and offline reports.
- [Request correlation](request-correlation.md): server-generated request IDs
  across routing, retries, and streaming.
- [Full authenticated passthrough](full-passthrough.md): opt-in forwarding that
  bypasses managed admission, limits, deadlines, and validation. Read its risks
  before changing an alias.
- [Optional integrations](INTEGRATIONS.md): tokenizer, external Headroom sidecar,
  and reference patches. None is required for the minimal text setup.

## Contributors and maintainers

[Development and release checks](DEVELOPMENT.md) covers source boundaries,
tests, known fixture failures, licensing, and publication/privacy gates.

For an existing client, use the [migration guide](../MIGRATION.md) to inventory
request shapes, qualify a parallel gateway, canary one workload, and retain a
rollback path.
