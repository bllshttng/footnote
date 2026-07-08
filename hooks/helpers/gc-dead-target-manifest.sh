#!/usr/bin/env bash
# Archive a DEFUNCT .fno/target-state.md at session start (x-4af4: a dead
# manifest once auto-locked attended /think for ~10 days). Shell the ONE
# claim-first predicate (`fno target status`); never re-implement it in bash.
# Advisory: never blocks a session, degrades silently on any failure.
set -uo pipefail

STATE_FILE="${1:-.fno/target-state.md}"
[[ -f "$STATE_FILE" ]] || exit 0
command -v fno >/dev/null 2>&1 || exit 0

# jq is a hard dep of session-start.sh (it exits early when jq is absent), so
# parse the JSON robustly rather than with a format-fragile grep.
ml="$(fno target status --json 2>/dev/null | jq -r '."manifest-live"' 2>/dev/null || true)"
[[ "$ml" == dead* ]] || exit 0

if fno state archive --path "$STATE_FILE" --type target >/dev/null 2>&1; then
    echo "[fno] archived dead target manifest ($STATE_FILE); prior session is gone." >&2
fi
exit 0
