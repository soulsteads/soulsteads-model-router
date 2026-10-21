# Slotline key manager

`slotline keys` is a local administrator CLI. It has no HTTP route. A Slotline
policy can set `key_store: /absolute/path/to/keys.sqlite3` and omit
`credentials` to authenticate inference requests against an in-memory
verifier snapshot. File-backed credentials remain supported; choose one mode.

The CLI requires a mode `0600` SQLite store. A running store-backed service
also requires its parent directory to be mode `0700`; it binds a mode `0600`
`keys.sqlite3.control.sock` there. The store records immutable credential policy and Argon2id
verifiers, never raw keys. `issue` and `rotate` print a raw key once; direct
that output to the client's protected secret file. `list` shows only policy,
status, lifecycle times, and a non-secret key prefix. Successful lifecycle
audit events record the kernel effective UID, credential ID, action,
timestamp, and result.

```sh
# Use an existing private directory; commands below are lifecycle examples,
# not a sequence to run blindly. The binary must be on PATH.
umask 077
STORE="${XDG_CONFIG_HOME:-$HOME/.config}/slotline/keys.sqlite3"
slotline keys init "$STORE"
slotline keys issue "$STORE" policy.json > client.key
slotline keys list "$STORE"
slotline keys verify "$STORE" < client.key
slotline keys rotate "$STORE" example-client 600 > replacement.key
# Replace KEY_PREFIX with the nonsecret prefix returned by `keys list`.
slotline keys revoke-key "$STORE" KEY_PREFIX
slotline keys revoke-credential "$STORE" example-client
slotline keys backup "$STORE" "${STORE}.backup"
slotline keys restore "${STORE}.backup" "${STORE}.restored"
```

`policy.json` contains non-secret policy, for example:

```json
{
  "id": "example-client",
  "aliases": ["local-chat"],
  "class": "interactive",
  "requests_per_minute": 120,
  "burst": 12,
  "max_in_flight": 8,
  "agent_affinity": true,
  "allowed_routes": [],
  "operator": false
}
```

Rotation caps old active key versions to the requested overlap, at most one
hour. Explicitly revoke an old prefix after client migration. `backup` and
`restore` create new files and refuse to overwrite an existing store. The
inference request path performs bounded Argon2 verification on blocking
workers and never reads SQLite. The process reloads verifier snapshots every
second and immediately on a CLI lifecycle change. The CLI receives `ok` only
after the running service swaps the authorization state, so issue, rotation,
and revocation are effective when a successful command returns. The swap also
allocates rate and class capacity for a newly issued policy. A corrupt store
or invalid new policy closes authentication until corrected. If the control
socket exists but cannot acknowledge, the CLI exits with an error after the database mutation; inspect
the command output and service state before retrying. The listener safely
reclaims an owned stale socket after an unclean stop.

## Route-policy reload is separate

The key-control socket is not the route-policy control socket. With `key_store`,
`slotline reload CONFIG.yaml` uses `<key_store>.policy.sock` in the private store
directory. File-key policies use `CONFIG.yaml.policy.sock` and require a private,
writable policy directory. Neither socket is an HTTP API.

Reload validates and atomically publishes a candidate generation. It rejects
changes to listener/credential authority and every ordered route declaration,
including endpoint, capacity, capabilities, enablement, and upstream credentials.
The supported scope is reviewed local-Qwen alias/catalog, priority, and matcher
edits. See [operations](docs/OPERATIONS.md#reload-policy-or-restart-safely) before
changing a running gateway; key lifecycle acknowledgment does not authorize a
backend or policy migration.
