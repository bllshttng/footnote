#!/usr/bin/env bash
# Read-only SessionStart predicate for recent activity in this worktree.
#
# Normal mode (default): prints the advisory line when a fresh peer is
# observed, else stays silent. Byte-compatible with the pre-machine behavior.
# --machine mode: prints one JSON observation object when a fresh peer is
# observed, else stays silent. The object carries the observer, every fresh
# peer, the worktree/repo Git identity, and the fixed liveness window.
# Both modes share ONE liveness predicate, so prose and structured evidence
# cannot disagree about whether overlap was observed.

worktree_live_dir() {
  local cwd="$1" git_dir
  git_dir="$(unset GIT_DIR GIT_WORK_TREE; git -C "$cwd" rev-parse --absolute-git-dir 2>/dev/null || true)"
  [[ -n "$git_dir" && -d "$git_dir" ]] || return 1
  printf '%s/fno/live\n' "$git_dir"
}

# Minimal JSON string escaper for safe token values (session ids and paths).
json_str() {
  local s="${1-}"
  s="${s//\\/\\\\}"
  s="${s//\"/\\\"}"
  printf '"%s"' "$s"
}

set -uo pipefail

if [[ "${1:-}" == --live-dir ]]; then
  worktree_live_dir "${2:-}" || true
  exit 0
fi

MACHINE=0
[[ "${1:-}" == --machine ]] && MACHINE=1

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

# Resolve Git identity for machine mode once, best-effort. Never blocks the
# predicate: a missing identity yields empty strings in the observation and the
# CLI's normalization handles the rest. Harness identity is intentionally NOT
# resolved here - it is diagnostic-only and must not shape the predicate.
GIT_DIR=""
COMMON_DIR=""
if (( MACHINE )); then
  GIT_DIR="$(unset GIT_DIR GIT_WORK_TREE; git -C "$CWD" rev-parse --absolute-git-dir 2>/dev/null || true)"
  COMMON_DIR="$(unset GIT_DIR GIT_WORK_TREE; git -C "$CWD" rev-parse --git-common-dir 2>/dev/null || true)"
  [[ -n "$COMMON_DIR" && "$COMMON_DIR" != /* ]] && COMMON_DIR="$CWD/$COMMON_DIR"
fi

[[ "$SELF_ID" =~ ^[A-Za-z0-9_-]+$ && -d "$LIVE_DIR" && -r "$LIVE_DIR" ]] || exit 0

now="$(date +%s 2>/dev/null || echo 0)"
(( now > 0 )) || exit 0
shopt -s nullglob
peer_ids=()
for stamp in "$LIVE_DIR"/*; do
  [[ -f "$stamp" && "${stamp##*/}" != "$SELF_ID" ]] || continue
  mtime="$(stat -c %Y "$stamp" 2>/dev/null || stat -f %m "$stamp" 2>/dev/null || echo 0)"
  if (( mtime > 0 && mtime <= now && now - mtime < WINDOW )); then
    peer_ids+=("${stamp##*/}")
  fi
done

# No fresh peer: silent in both modes (matches the advisory-only contract).
(( ${#peer_ids[@]} > 0 )) || exit 0

if (( MACHINE )); then
  # Deterministic peer ordering so the downstream observation id is stable.
  sorted_peers=()
  while IFS= read -r line; do
    [[ -n "$line" ]] && sorted_peers+=("$line")
  done < <(printf '%s\n' "${peer_ids[@]}" | LC_ALL=C sort)
  {
    printf '{"observer_session_id":%s,"peer_session_ids":[' "$(json_str "$SELF_ID")"
    for i in "${!sorted_peers[@]}"; do
      (( i > 0 )) && printf ','
      printf '%s' "$(json_str "${sorted_peers[$i]}")"
    done
    printf '],"worktree_git_dir":%s,"repository_common_dir":%s,"live_window_seconds":%d}\n' \
      "$(json_str "$GIT_DIR")" "$(json_str "$COMMON_DIR")" "$WINDOW"
  }
else
  echo '- Another session is working in this worktree. [fno-overlap-observed]'
fi
exit 0
