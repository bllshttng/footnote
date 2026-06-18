#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "$0")/../.." && pwd)"
CHECKPOINT_DIR="$ROOT_DIR/.fno/checkpoints"
mkdir -p "$CHECKPOINT_DIR"

STAMP="$(date +%Y%m%d-%H%M%S)"
OUT="$CHECKPOINT_DIR/pre-compact-${STAMP}.md"

{
  echo "# Pre-compact checkpoint"
  echo
  echo "timestamp: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "cwd: $ROOT_DIR"
  echo
  if [[ -f "$ROOT_DIR/.fno/STATE.md" ]]; then
    echo "## STATE.md"
    tail -n 60 "$ROOT_DIR/.fno/STATE.md"
    echo
  fi
  if [[ -f "$ROOT_DIR/.fno/SUMMARY.md" ]]; then
    echo "## SUMMARY.md"
    tail -n 60 "$ROOT_DIR/.fno/SUMMARY.md"
    echo
  fi
  echo "## Git status"
  git -C "$ROOT_DIR" status --short || true
} > "$OUT"

echo "Checkpoint written: $OUT"
