#!/usr/bin/env bash
# Read-only SessionStart predicate for recent activity in this worktree.

set -uo pipefail

INPUT="$(cat 2>/dev/null || true)"
CWD=""
SELF_ID="${CODEX_THREAD_ID:-}"
if [[ -n "$INPUT" ]] && command -v jq >/dev/null 2>&1; then
  CWD="$(printf '%s' "$INPUT" | jq -r '.cwd // empty' 2>/dev/null)"
  [[ -n "$SELF_ID" ]] || SELF_ID="$(printf '%s' "$INPUT" | jq -r 'if (.session_id? | type) == "string" then .session_id else empty end' 2>/dev/null)"
fi
[[ -n "$CWD" ]] || CWD="${CLAUDE_PROJECT_DIR:-$PWD}"

WINDOW="${FNO_WORKTREE_LIVE_WINDOW_SECONDS:-120}"
LIVE_DIR="$CWD/.fno/live"
[[ "$SELF_ID" =~ ^[A-Za-z0-9_-]+$ && "$WINDOW" =~ ^[0-9]+$ \
    && "$WINDOW" -gt 0 && -d "$LIVE_DIR" && -r "$LIVE_DIR" ]] || exit 0

now="$(date +%s 2>/dev/null || echo 0)"
(( now > 0 )) || exit 0
shopt -s nullglob
for stamp in "$LIVE_DIR"/*; do
  [[ -f "$stamp" && "${stamp##*/}" != "$SELF_ID" ]] || continue
  mtime="$(stat -f %m "$stamp" 2>/dev/null || stat -c %Y "$stamp" 2>/dev/null || echo 0)"
  if (( mtime > 0 && mtime <= now && now - mtime < WINDOW )); then
    echo '- Another session is working in this worktree.'
    exit 0
  fi
done
exit 0
