#!/usr/bin/env bash
# Read-only SessionStart predicate for recent activity in this worktree.

worktree_live_dir() {
  local cwd="$1" git_dir
  git_dir="$(git -C "$cwd" rev-parse --absolute-git-dir 2>/dev/null || true)"
  [[ -n "$git_dir" && -d "$git_dir" ]] || return 1
  printf '%s/fno/live\n' "$git_dir"
}

set -uo pipefail

if [[ "${1:-}" == --live-dir ]]; then
  worktree_live_dir "${2:-}" || true
  exit 0
fi

INPUT="$(cat 2>/dev/null || true)"
CWD=""
SELF_ID="${CODEX_THREAD_ID:-}"
if [[ -n "$INPUT" ]] && command -v jq >/dev/null 2>&1; then
  CWD="$(printf '%s' "$INPUT" | jq -r '.cwd // empty' 2>/dev/null)"
  [[ -n "$SELF_ID" ]] || SELF_ID="$(printf '%s' "$INPUT" | jq -r 'if (.session_id? | type) == "string" then .session_id else empty end' 2>/dev/null)"
fi
[[ -n "$CWD" ]] || CWD="${CLAUDE_PROJECT_DIR:-$PWD}"

WINDOW=120
LIVE_DIR="$(worktree_live_dir "$CWD" 2>/dev/null || true)"
[[ "$SELF_ID" =~ ^[A-Za-z0-9_-]+$ && -d "$LIVE_DIR" && -r "$LIVE_DIR" ]] || exit 0

now="$(date +%s 2>/dev/null || echo 0)"
(( now > 0 )) || exit 0
shopt -s nullglob
for stamp in "$LIVE_DIR"/*; do
  [[ -f "$stamp" && "${stamp##*/}" != "$SELF_ID" ]] || continue
  mtime="$(stat -f %m "$stamp" 2>/dev/null || stat -c %Y "$stamp" 2>/dev/null || echo 0)"
  if (( mtime > 0 && mtime <= now && now - mtime < WINDOW )); then
    echo '- Another session is working in this worktree. [fno-overlap-observed]'
    exit 0
  fi
done
exit 0
