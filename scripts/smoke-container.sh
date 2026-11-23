#!/bin/sh
# Exercise the installed CLI and its state in disposable, network-disabled containers.
set -eu
engine=${1:-docker}
image=${2:-localhost/soulsteads-model-router:smoke}
# Use an explicit name: Podman removes anonymous volumes with --rm.
suffix=$(od -An -N 8 -tx1 /dev/urandom | tr -d " \n")
volume=$("$engine" volume create "soulsteads-smoke-$suffix")
trap '"$engine" volume rm "$volume" >/dev/null' EXIT
run() {
  "$engine" run --rm --network none --mount "type=volume,source=$volume,target=/data" "$@"
}
run --entrypoint sh "$image" -ec 'test "$(id -u)" -eq 10001 && test -w /data'
{
  printf 'listener_mode: loopback\n'
  sed -e 's/^bind:.*/bind: 127.0.0.1/' \
    -e 's|/absolute/path/to/operator.key|/data/operator.key|' config/slotline.example.yaml
} | run --entrypoint sh -i "$image" -ec '
  umask 077; cat > /data/config.yaml
  head -c 32 /dev/urandom | od -An -tx1 | tr -d " \n" > /data/operator.key
'
run "$image" --check-config /data/config.yaml | grep -F 'slotline configuration valid'
