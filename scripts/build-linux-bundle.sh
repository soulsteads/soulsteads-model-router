#!/usr/bin/env bash
# Build the production GNU/Linux executable and its release notices in scratch.
set -euo pipefail
if (( $# != 1 )); then
  echo "Usage: $0 EXISTING_SCRATCH_DIRECTORY" >&2
  exit 2
fi
RUNNER_TEMP=$(realpath -- "$1")
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
bundle='soulsteads-model-router-linux-x86_64'
mkdir "$RUNNER_TEMP/$bundle"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$RUNNER_TEMP/release-target}"
export CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_PROFILE_RELEASE_STRIP=debuginfo
# Ubuntu 24.04 GNU/Linux build; do not include dev/test executables.
cargo +1.100.0 build --release --locked --target x86_64-unknown-linux-gnu --bin soulsteads-router
# Exercise both unit scopes without contacting systemd or a provider.
PYTHONDONTWRITEBYTECODE=1 python3 tests/service_install_validation.py --binary "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/soulsteads-router"
install -m 0755 "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/soulsteads-router" "$RUNNER_TEMP/$bundle/soulsteads-router"
install -m 0644 LICENSE NOTICE "$RUNNER_TEMP/$bundle/"
install -m 0644 third_party/LiteLLM-LICENSE.txt "$RUNNER_TEMP/$bundle/"
# The router takes a config path, not --help. Validate a disposable fixture without starting it.
(
  umask 077
  smoke=$(mktemp -d "$RUNNER_TEMP/soulsteads-router-smoke.XXXXXXXX")
  openssl rand -hex 32 > "$smoke/smoke.key"
  key_path=$(jq -Rn --arg path "$smoke/smoke.key" '$path')
  cat > "$smoke/smoke.yaml" <<YAML
listener_mode: loopback
bind: 127.0.0.1
port: 4011
credentials:
  - {name: fixture, key_file: $key_path, aliases: [fixture], class: interactive, requests_per_minute: 100, burst: 20, max_in_flight: 2}
routes:
  - {name: fixture, kind: local, base_url: 'http://127.0.0.1:18121/v1', model: fixture, safe_concurrency: 1, context_limit: 2000, adapter_version: fixture, capabilities: [text], timeout_seconds: 1, enabled: true}
aliases:
  fixture: {context_limit: 2000, capabilities: [text], routes: [{route: fixture, priority: 10}]}
YAML
  "$RUNNER_TEMP/$bundle/soulsteads-router" --check-config "$smoke/smoke.yaml"
)
cargo +1.100.0 metadata --format-version 1 --locked --filter-platform x86_64-unknown-linux-gnu > "$RUNNER_TEMP/cargo-metadata.json"
python3 - "$RUNNER_TEMP/cargo-metadata.json" "$RUNNER_TEMP/$bundle" <<'PY'
import json
from pathlib import Path
import re

import sys

metadata = json.loads(Path(sys.argv[1]).read_text())
bundle = Path(sys.argv[2])
notices = bundle / 'dependency-notices'
notices.mkdir()
resolve = metadata['resolve']
nodes = {node['id']: node for node in resolve['nodes']}
packages = {pkg['id']: pkg for pkg in metadata['packages']}
root = resolve['root']
selected = set()
pending = [root]
while pending:
    package = pending.pop()
    if package in selected:
        continue
    selected.add(package)
    pending.extend(dep['pkg'] for dep in nodes[package]['deps']
                   if any(kind['kind'] != 'dev' for kind in dep['dep_kinds']))

rows = []
legal = re.compile(r'(license|licence|copying|copyright|notice|patents|unlicense)(?:$|[._-])', re.I)
for package in sorted(selected - {root}):
    pkg = packages[package]
    source = Path(pkg['manifest_path']).parent.resolve()
    target = notices / (pkg['name'] + '-' + pkg['version'])
    files = {path for path in source.rglob('*')
             if path.is_file() and legal.match(path.name) and path.suffix != '.rs'}
    if pkg['license_file']:
        files.add(source / pkg['license_file'])
    copied = []
    for path in sorted(files):
        relative = path.relative_to(source)
        if '..' in relative.parts or not path.resolve().is_relative_to(source):
            raise SystemExit(f'License file outside crate: {target.name}')
        content = path.read_bytes()
        if not content.strip():
            continue
        dest = target / relative
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(content)
        copied.append(relative.as_posix())
    if not copied:
        raise SystemExit(f'No nonempty license or notice files for {target.name}')
    rows.append(f"{target.name}\t{pkg['license'] or 'See packaged license file'}\t{', '.join(copied)}\n")
(notices / 'INDEX.tsv').write_text('Crate\tDeclared license\tFiles supplied by the crate\n' + ''.join(rows))
PY
