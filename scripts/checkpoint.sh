#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "$0")/../../.." && pwd)"
OUT_DIR="$ROOT_DIR/.fno/checkpoints"
mkdir -p "$OUT_DIR"

STAMP="$(date +%Y%m%d-%H%M%S)"
OUT="$OUT_DIR/abilities-${STAMP}.md"

{
  echo "# Abilities checkpoint"
  echo
  echo "timestamp: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo
  if [[ -f "$ROOT_DIR/.fno/STATE.md" ]]; then
    echo "## STATE"
    tail -n 80 "$ROOT_DIR/.fno/STATE.md"
    echo
  fi
  echo "## Git status"
  git -C "$ROOT_DIR" status --short || true
} > "$OUT"

echo "Wrote checkpoint: $OUT"
