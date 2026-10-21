#!/usr/bin/env bash
set -euo pipefail

TOKENIZER_REVISION=925d7be6c14c6c9442ef83e8f05b5a3c39304f69
TOKENIZER_SHA256=0997f410c57a1f4e53b09e4be8f4a172d90edd9564368fb0847030937229b9f3
TOKENIZER_DEST=${TOKENIZER_DEST:-${XDG_CONFIG_HOME:-$HOME/.config}/slotline/qwen38-tokenizer.json}
(umask 077; mkdir -p "$(dirname "$TOKENIZER_DEST")")
TOKENIZER_TEMP=$(mktemp)
trap 'rm -f "$TOKENIZER_TEMP"' EXIT

curl --fail --location --silent --show-error --connect-timeout 5 --max-time 60 \
  "https://huggingface.co/Mia-AiLab/Qwen3.8-Flash-Next-NVFP4/resolve/${TOKENIZER_REVISION}/tokenizer.json" \
  --output "$TOKENIZER_TEMP"
printf '%s  %s\n' "$TOKENIZER_SHA256" "$TOKENIZER_TEMP" | sha256sum --check --status
install -m 644 "$TOKENIZER_TEMP" "$TOKENIZER_DEST"
printf 'Installed pinned Qwen tokenizer: %s\n' "$TOKENIZER_SHA256"
