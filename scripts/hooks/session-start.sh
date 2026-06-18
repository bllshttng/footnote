#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "$0")/../.." && pwd)"
STATE_FILE="$ROOT_DIR/.fno/STATE.md"
RSTATE_FILE="$ROOT_DIR/.fno/target-state.md"

if [[ -f "$STATE_FILE" ]]; then
  echo "Loaded .fno/STATE.md"
  tail -n 30 "$STATE_FILE"
else
  echo "No .fno/STATE.md found; starting with empty task state."
fi

if [[ -f "$RSTATE_FILE" ]]; then
  echo
  echo "Loaded .fno/target-state.md"
  tail -n 20 "$RSTATE_FILE"
fi
