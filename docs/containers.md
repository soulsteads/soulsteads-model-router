# Run Model Router in a container

Build a Linux image from this repository with Docker or Podman. Run these
commands from the root of a Git checkout that contains `Dockerfile`. You need
a running container engine and network access for the public build dependencies.
No registry account or host language toolchain is required; the build pulls
public base images. Use `engine=podman` in place of `engine=docker` for all
commands below.

```sh
engine=docker                   # or: engine=podman
image=localhost/soulsteads-model-router:local
"$engine" build --tag "$image" .
"$engine" volume create soulsteads-model-router-data
```

The image runs as UID/GID `10001`, without root. A fresh engine-managed volume
at `/data` inherits the image's private directory and avoids bind-mount UID and
SELinux problems. If using a bind mount instead, provision a dedicated directory
writable by the mapped container user, mode `0700`, with suitable SELinux labeling
(e.g. a private `:Z` label on Podman); do not make it world-writable. Don't mount
a host home or the container-engine socket.

## Prepare a private policy and key

Slotline does not run an inference server. Start with
[the example policy](../config/slotline.example.yaml) on your host, then put a
copy with your backend's literal private/loopback IPv4 and exact model ID in a
private `config.yaml` outside the checkout. For the local walkthrough, set
`listener_mode: loopback`, `bind: 127.0.0.1`, and `port: 4011`. Change the
example credential's `key_file` to `/data/operator.key`. Keep the route and
alias context limits within your backend's tested limits. See [Setup](SETUP.md)
for all prerequisites and the supported Tailnet alternative.

Generate a fresh example operator key and install the config in the private
volume. No key is written to the build context or printed:

```sh
# Set SLOTLINE_POLICY to the absolute path to your private config.yaml.
"$engine" run --rm -i --network none -v soulsteads-model-router-data:/data \
  --entrypoint sh "$image" -ec '
    umask 077; set -C
    cat > /data/config.yaml
    head -c 32 /dev/urandom | od -An -tx1 | tr -d " \n" > /data/operator.key
  ' < "$SLOTLINE_POLICY"
"$engine" run --rm --network none -v soulsteads-model-router-data:/data \
  "$image" --check-config /data/config.yaml
```

Run the initialization only once per new volume. `--check-config` checks policy
and credentials without contacting the model backend. Keep state writable by
the container user: the router creates owner-only Unix control sockets beside
the policy or in the verifier store, so a read-only config-directory mount is
not a substitute for the state volume.

## Run the router

Use native Linux host networking for this recipe. Slotline rejects wildcard
and LAN binds, so `-p` on a bridged container does not implement this setup.
`--network host` shares the host's network namespace and makes the host's
loopback backend reachable; do not use it for untrusted workloads. Keep the
listener loopback-only for same-host use. No `-p` is needed or used. For a
Tailnet deployment, bind only the operator's actual permitted Tailnet IPv4 and
maintain the access controls in [Setup](SETUP.md). Slotline does not provide TLS.

```sh
"$engine" run -d --name soulsteads-model-router --network host \
  -v soulsteads-model-router-data:/data "$image" /data/config.yaml
"$engine" logs soulsteads-model-router
"$engine" exec soulsteads-model-router soulsteads-router status /data/config.yaml --json
```

The `status` command can report backend health; it is not proof of inference.
After the backend is ready, follow the [authenticated client check](SETUP.md#4-check-from-another-terminal).
For that check, the operator can copy `/data/operator.key` into a mode-`0700`
host directory outside the checkout using the engine's `cp`, restrict the
copied file to mode `0600`, and set `SLOTLINE_KEY_FILE` to it. Use scoped client
keys for regular traffic, not the operator key.

## Retain state and provenance

Stop the old container before replacing it. Keep a private backup of the
whole volume (including policy, verifier store and keys where configured),
and keep the prior image for rollback. See [verifier-store operations](../KEYS.md)
for rotation and recovery. The image excludes `.git`, so Slotline logs a source
content hash but no embedded Git revision. Tag the local image for the exact
reviewed source revision you build; do not treat a mutable tag as provenance.

To stop and remove just the container (the named volume remains):

```sh
"$engine" stop --time 30 soulsteads-model-router
"$engine" rm soulsteads-model-router
```

Then build the desired source revision and rerun the start command with the same
volume. Reuse a stable image tag or digest and the same deployment settings when
supervising it with your container engine or service manager. First test on a
fresh disposable volume. Run the offline image smoke locally with:

```sh
sh scripts/smoke-container.sh "$engine" "$image"
```
