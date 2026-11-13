# Optional integrations

These integrations are not required for the [minimal setup](SETUP.md). They are
not installed automatically, and their external binaries, model assets, and
project versions need separate verification.

- [Hindsight](HINDSIGHT.md) is a disabled-by-default connection to an existing
  external memory server, with explicit authenticated retain/recall/reflect and
  server-owned bank mappings. It never enriches or retains ordinary inference.

- `scripts/install_qwen_tokenizer.sh` attempts to fetch a checksum-pinned Qwen
  tokenizer. The pinned external URL returned HTTP 401 during public-release
  preparation; do not assume anonymous download access. The minimal setup does
  not require it. Do not substitute another tokenizer without validating model
  compatibility and pinning its bytes. Set `TOKENIZER_DEST` to an absolute
  destination to override the XDG default.
  It replaces that destination; preserve an existing tokenizer first. Add the
  resulting absolute file and SHA-256 under the alias's `tokenizer` setting.
- Headroom is an external optional sidecar, not built or distributed here.
  The example unit fixes its upstream, binds the adapter to loopback, and
  disables compression. Generate `~/.config/slotline/headroom.sha256` from an
  independently verified binary. Do not treat the retained historical digest
  as verification of an arbitrary download. Matching `physical_route` and
  safe concurrency are required so adapter/direct traffic shares admission.
- Integration patches are reference material for specific external revisions,
  not automatically installed dependencies. Review and test them against the
  version you actually use.
