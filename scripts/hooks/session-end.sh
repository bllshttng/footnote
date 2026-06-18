#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "$0")/../.." && pwd)"
LOG_DIR="$ROOT_DIR/.fno/checkpoints"
mkdir -p "$LOG_DIR"

STAMP="$(date +%Y%m%d-%H%M%S)"
OUT="$LOG_DIR/session-end-${STAMP}.md"

{
  echo "# Session end summary"
  echo
  echo "timestamp: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo
  if [[ -f "$ROOT_DIR/.fno/SUMMARY.md" ]]; then
    echo "## SUMMARY.md"
    tail -n 80 "$ROOT_DIR/.fno/SUMMARY.md"
    echo
  fi
  echo "## Git status"
  git -C "$ROOT_DIR" status --short || true
} > "$OUT"

echo "Session summary written: $OUT"
