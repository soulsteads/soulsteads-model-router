# Set up Soulsteads Model Router

This guide starts a text-only gateway in the foreground. It does not install a
model server, migrate an existing client, or replace another gateway.

## Prerequisites

- Linux with Rust/Cargo supporting edition 2024 and a C toolchain for bundled
  native dependencies. Build with the checked-in `Cargo.lock`.
- An assigned Tailnet IPv4 address in `100.64.0.0/10`, or explicit loopback
  mode for same-host clients (see below). LAN and wildcard binds are rejected;
  port `4001` is reserved.
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

The executable is `target/release/soulsteads-router`. No tokenizer download is needed
for the minimal text example; without a pinned tokenizer Slotline uses a
conservative context estimate.

## 2. Prepare a private policy and credential

Set `SLOTLINE_BIND` to an address actually assigned to this machine. The address
below is illustrative; do not copy it unchanged unless it is yours.

```sh
export SLOTLINE_BIND=100.127.255.254
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
text = text.replace('bind: 100.127.255.254', f'bind: {address}')
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
target/release/soulsteads-router --check-config "$CONFIG"
target/release/soulsteads-router "$CONFIG"
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

## 5. Optional systemd service

Only do this after the foreground checks pass. The installer uses the current
absolute executable and the XDG policy location
`$XDG_CONFIG_HOME/slotline/config.yaml` (or `~/.config/slotline/config.yaml`).
It never parses the policy, enables, starts, or restarts a unit.

For a user service, first copy the reviewed executable somewhere stable on your
`PATH` (for example `~/.local/bin/soulsteads-router`), then run the copy:

```sh
install -d "$HOME/.local/bin"
install -m 755 target/release/soulsteads-router "$HOME/.local/bin/soulsteads-router"
"$HOME/.local/bin/soulsteads-router" service install --user --dry-run
"$HOME/.local/bin/soulsteads-router" service install --user
```

The write refuses an existing or symlinked unit. Review the generated unit at
`$XDG_CONFIG_HOME/systemd/user/soulsteads-router.service` (or
`~/.config/systemd/user/soulsteads-router.service`) before manually choosing
any lifecycle action. The user unit intentionally avoids filesystem-namespace
sandbox directives so it also works where unprivileged user namespaces are
disabled:

```sh
systemctl --user daemon-reload
systemctl --user enable soulsteads-router.service
systemctl --user start soulsteads-router.service
systemctl --user status soulsteads-router.service
```

For a system service, run the installer as an administrator and select an
existing non-root account explicitly. It writes only the fixed
`/etc/systemd/system/soulsteads-router.service` path; there is no directory
override. It derives that account's absolute `~/.config/slotline/config.yaml`
location, runs the service as that account, uses the system-service filesystem
sandbox directives, and installs to `multi-user.target`:

```sh
sudo /absolute/path/to/soulsteads-router service install --system --run-as router --dry-run
sudo /absolute/path/to/soulsteads-router service install --system --run-as router
sudo systemctl daemon-reload
sudo systemctl enable soulsteads-router.service
sudo systemctl start soulsteads-router.service
sudo systemctl status soulsteads-router.service
```

Re-run the real catalog and completion checks after service startup. Use
`journalctl --user -u soulsteads-router.service` for a user service, or
`sudo journalctl -u soulsteads-router.service` for a system service. Starting
at boot without an interactive login depends on your host's user-service/linger
configuration; this guide does not change it.

## Next steps

- Use [operations](OPERATIONS.md) to inspect status, understand deadlines, and update safely.
- Use the [key manager](../KEYS.md) for verifier-backed client credentials.
- Check [compatibility](COMPATIBILITY.md) before adding tools, images, or other endpoints.
- Read [optional integrations](INTEGRATIONS.md) only if you need a tokenizer or sidecar.

## Synthetic address used in examples

`100.127.255.254` is an intentionally synthetic Tailnet/CGNAT fixture address, not a configured deployment endpoint. Replace it with your own Tailnet IPv4 address when following these examples. This walkthrough uses the default Tailnet mode; see below for loopback mode.


## Same-host deployment

To serve only clients on the same machine, explicitly set:

```yaml
listener_mode: loopback
bind: 127.0.0.1
port: 8080
```

The default remains `listener_mode: tailnet`, requiring an explicit Tailnet IPv4
address. Loopback mode accepts numeric loopback addresses only, including `::1`;
LAN, Tailnet and wildcard binds are rejected in that mode. Bearer authentication,
key grants, rate/capacity limits and qualification are identical in both modes.
Changing listener mode/address/port requires a restart, never policy reload.
Port 4001 remains reserved. This mode does not publish the service or migrate clients.
