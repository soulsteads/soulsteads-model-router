# Set up Soulsteads Model Router

This guide starts a text-only gateway in the foreground. It does not install a
model server, migrate an existing client, or replace another gateway.

## Prerequisites

- Linux with Rust/Cargo supporting edition 2024 and a C toolchain for bundled
  native dependencies. Build with the checked-in `Cargo.lock`.
- An assigned Tailnet IPv4 address in `100.64.0.0/10`. The current validator
  rejects loopback, LAN, and wildcard listener binds, and reserves port `4001`.
- A running local OpenAI-compatible backend with `/v1/models` and
  `/v1/chat/completions`. Know its exact model ID and tested context/capacity.
- Python 3 for the configuration preparation and client request examples.

The minimal example uses `qwen38flash` at `http://127.0.0.1:8000/v1`.
Change both values to match your server. Do not point Slotline at itself or
another public provider gateway. Upstream URLs use literal private or loopback
IPv4 addresses, not DNS names. `/v1/models` discovery alone does not prove a
server can handle your workload; Slotline also runs serving probes.

## 1. Build

From the repository root:

```sh
cargo build --release --locked
cargo test --locked
```

The executable is `target/release/slotline`. No tokenizer download is needed
for the minimal text example; without a pinned tokenizer Slotline uses a
conservative context estimate.

## 2. Prepare a private policy and credential

Set `SLOTLINE_BIND` to an address actually assigned to this machine. The address
below is illustrative; do not copy it unchanged unless it is yours.

```sh
export SLOTLINE_BIND=100.64.0.1
```

This script creates `~/.config/slotline/config.yaml` and `operator.key` by default,
or uses `XDG_CONFIG_HOME`. It refuses to overwrite either file. It never prints
the generated key. Use a dedicated private directory; do not commit its contents.

```sh
python3 - <<'PY'
import ipaddress
import os
from pathlib import Path
import secrets

address = ipaddress.IPv4Address(os.environ['SLOTLINE_BIND'])
if address not in ipaddress.IPv4Network('100.64.0.0/10'):
    raise SystemExit('SLOTLINE_BIND must be your assigned Tailnet IPv4 address')
os.umask(0o077)
directory = Path(os.environ.get('XDG_CONFIG_HOME', Path.home() / '.config')) / 'slotline'
directory.mkdir(parents=True, exist_ok=True)
if directory.stat().st_mode & 0o077:
    raise SystemExit('Use a private configuration directory (mode 0700)')
key = directory / 'operator.key'
policy = directory / 'config.yaml'
if key.exists() or policy.exists():
    raise SystemExit('Existing files found; choose a new private directory or edit manually')
with key.open('x') as output:
    output.write(secrets.token_urlsafe(32) + '\n')
text = Path('config/slotline.example.yaml').read_text()
text = text.replace('bind: 100.64.0.1', f'bind: {address}')
# JSON-quoted strings are valid YAML and preserve spaces/quotes in the path.
import json
text = text.replace('/absolute/path/to/operator.key', json.dumps(str(key.resolve())))
with policy.open('x') as output:
    output.write(text)
print(f'Policy created at {policy}; edit its backend before starting')
PY
```

Edit the policy before proceeding:

- Set `base_url` and the exact backend `model` on `local-a`.
- Set `safe_concurrency` to the number of requests that backend can actually
  handle. Do not count aliases or sidecars as additional physical slots.
- Set `context_limit` on the route and alias to verified total context limits.
- Keep `capabilities: [text]` until tools, structured output, or images have
  been separately tested on the exact backend. The gateway's capability enum
  and validation rules live in `src/config.rs`.
- Leave `local-b` disabled unless a second real backend is ready. To use it,
  update its URL, model, limits, and capabilities before enabling it.

The walkthrough uses a file-backed operator credential for simplicity. Give
normal clients separate non-operator keys restricted to their aliases. For
long-lived deployments, use the [verifier-store CLI](../KEYS.md); the policy's
`key_store` and inline `credentials` are mutually exclusive. Runtime key
rotation/revocation acknowledgments require that store mode.

## 3. Validate and start

```sh
CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/slotline/config.yaml"
target/release/slotline --check-config "$CONFIG"
target/release/slotline "$CONFIG"
```

Expected validation output: `slotline configuration valid`. Validation checks
policy structure and credential files; it is not a successful inference test.
The foreground process should log `slotline listening`. Wait for backend health
and serving probes to qualify the route before testing inference. Stop with
Ctrl-C. If no route becomes eligible, inspect its health before sending client traffic.

## 4. Check from another terminal

Set the same `SLOTLINE_BIND` in this terminal. This request reads the credential
from disk without putting it in process command-line arguments or printing it.
Do not enable shell tracing or paste the credential file into chat.

```sh
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/slotline"
export SLOTLINE_BASE_URL="http://${SLOTLINE_BIND}:4011"
export SLOTLINE_KEY_FILE="$CONFIG_DIR/operator.key"
python3 - <<'PY'
import json
import os
from pathlib import Path
import urllib.request

base = os.environ['SLOTLINE_BASE_URL']
key = Path(os.environ['SLOTLINE_KEY_FILE']).read_text().strip()
for endpoint, body in [('/v1/models', None), ('/v1/chat/completions', {
    'model': 'local-chat',
    'messages': [{'role': 'user', 'content': 'Reply with a short greeting.'}],
    'max_tokens': 32,
})]:
    request = urllib.request.Request(
        base + endpoint,
        data=None if body is None else json.dumps(body).encode(),
        headers={'Authorization': 'Bearer ' + key, 'Content-Type': 'application/json'},
    )
    with urllib.request.urlopen(request, timeout=150) as response:
        print(response.status, response.read().decode())
PY
```

The catalog should contain `local-chat`. A completion should use that public
alias in its response and contain actual model output. A successful catalog
request alone does not prove the backend is serving. Confirm a missing or
incorrect bearer is rejected, and inspect authenticated `/healthz` or `/status`
if the completion returns `503`.

## 5. Optional systemd user service

Only do this after the foreground checks pass. Review
[`deploy/slotline-shadow.service`](../deploy/slotline-shadow.service) first.
It expects the binary at `~/.local/bin/slotline` and the policy at
`~/.config/slotline/config.yaml` (edit paths if you used `XDG_CONFIG_HOME`).
The private state directory is writable through the unit's filesystem
restrictions so Unix sockets and a verifier store can work.

Do not overwrite an existing binary or unit without preserving its rollback.
For a new installation:

```sh
install -d "$HOME/.local/bin" "$HOME/.config/systemd/user"
install -m 755 target/release/slotline "$HOME/.local/bin/slotline"
install -m 644 deploy/slotline-shadow.service "$HOME/.config/systemd/user/slotline.service"
systemctl --user daemon-reload
systemctl --user enable --now slotline.service
systemctl --user status slotline.service
```

Re-run the real catalog and completion checks after service startup. Use
`journalctl --user -u slotline.service` for logs. Starting at boot without an
interactive login depends on your host's user-service/linger configuration;
this guide does not change it. These units are examples, not an installer or
proof of systemd hardening on every distribution.

## Next steps

- Use [operations](OPERATIONS.md) to inspect status, understand deadlines, and update safely.
- Use the [key manager](../KEYS.md) for verifier-backed client credentials.
- Check [compatibility](COMPATIBILITY.md) before adding tools, images, or other endpoints.
- Read [optional integrations](INTEGRATIONS.md) only if you need a tokenizer or sidecar.
