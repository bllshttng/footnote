#!/usr/bin/env bash
# abi-watch: per-project inbox-drain daemon. Launchd-managed.
# Usage: abi-watch.sh <project_name> <repo_root>
set -euo pipefail

PROJECT="${1:?usage: abi-watch.sh <project> <repo_root>}"
REPO_ROOT="${2:?usage: abi-watch.sh <project> <repo_root>}"
# Source paths.sh for typed path variables (STATE_DIR is used below for the
# session/prompt files and the no-vault fallback).
if command -v fno >/dev/null 2>&1; then
    _PATHS_SH="$(fno paths shell-stub 2>/dev/null || true)"
    [[ -f "$_PATHS_SH" ]] && source "$_PATHS_SH" 2>/dev/null || true
    unset _PATHS_SH
fi
# Resolve the watch target: the thread store's per-project inbox DIRECTORY,
# inbox_root_for(project) - the SAME path `fno mail drain` reads. It is
# vault-aware: config.paths.inbox_dir override, else <vault>/internal/agents/
# <project>/inbox when Obsidian is enabled, else state_dir()/inbox/agents/
# <project>/inbox. The store writes inbox/<date>-<slug>.md files into it.
#
# The old primary branch watched paths_inbox_thread "$PROJECT/inbox.md" - a flat
# file under $REPO_ROOT/.fno/inbox the thread store never writes - so the
# daemon never woke to drain real messages (ab-d3e7da36). Resolve via Python
# (the single source of truth), mirroring _detect_state's uv/python3 fallback;
# drop to the neutral no-vault default only when that resolution fails.
_resolve_inbox_path() {
  local resolver='import os
from fno.paths import inbox_root_for
print(inbox_root_for(os.environ["WATCH_PROJECT"]))'
  local p=""
  if [[ -n "$CLI_DIR" ]]; then
    p="$(WATCH_PROJECT="$PROJECT" uv run --project "$CLI_DIR" python3 -c "$resolver" 2>/dev/null || true)"
  else
    p="$(WATCH_PROJECT="$PROJECT" python3 -c "$resolver" 2>/dev/null || true)"
  fi
  if [[ -n "$p" ]]; then
    printf '%s\n' "$p"
  else
    printf '%s\n' "${STATE_DIR:-$HOME/.fno}/inbox/agents/$PROJECT/inbox"
  fi
}
DEBOUNCE_SECS="${TARGET_WATCH_DEBOUNCE_SECONDS:-5}"
LOG="$REPO_ROOT/.fno/abi-watch.log"
SESSION_FILE="${STATE_DIR:-$HOME/.fno}/${PROJECT}-watch-session.json"
PROMPT_FILE="${STATE_DIR:-$HOME/.fno}/inbox-drain-prompt.md"
CLI_DIR="$(cd "$(dirname "$0")/../cli" 2>/dev/null && pwd)" || CLI_DIR=""

# Anchor cwd to the repo root before anything else. launchd starts daemons
# from the user's home dir by default; the LLM-side `fno mail drain` and
# our own `mkdir -p .fno` rely on cwd to locate the project context.
cd "$REPO_ROOT" || { echo "abi-watch: cannot cd to REPO_ROOT='$REPO_ROOT'" >&2; exit 1; }

mkdir -p "$REPO_ROOT/.fno"

# Resolve the watch target now that cwd is anchored (inbox_root_for honors a
# {project_root}-relative config.paths.inbox_dir override). Ensure it exists so
# fswatch can attach before the first message lands - a missing path makes
# fswatch error out and the daemon exits.
INBOX_PATH="$(_resolve_inbox_path)"
mkdir -p "$INBOX_PATH"

_log() {
  printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >> "$LOG"
}

_detect_state() {
  # REPO_ROOT is passed via env var (not shell-interpolated into the Python string
  # literal) so paths containing single-quotes do not cause a SyntaxError.
  if [[ -n "$CLI_DIR" ]]; then
    REPO_ROOT="$REPO_ROOT" uv run --project "$CLI_DIR" python3 -c "
import os
from pathlib import Path
from fno.wake.detect import detect_session_state
print(detect_session_state(Path(os.environ['REPO_ROOT'])).value)
" 2>/dev/null || echo "idle"
  else
    REPO_ROOT="$REPO_ROOT" python3 -c "
import os
from pathlib import Path
from fno.wake.detect import detect_session_state
print(detect_session_state(Path(os.environ['REPO_ROOT'])).value)
" 2>/dev/null || echo "idle"
  fi
}

_spawn_drain() {
  local resume=()
  if [[ -f "$SESSION_FILE" ]]; then
    local sid
    sid=$(jq -r .session_id "$SESSION_FILE" 2>/dev/null || true)
    [[ -n "$sid" && "$sid" != "null" ]] && resume=(--resume "$sid")
  fi

  _log "spawn drain (project=$PROJECT)"
  local out
  out=$(claude -p \
    --append-system-prompt-file "$PROMPT_FILE" \
    --allowedTools "Bash(fno *),Read" \
    --output-format json \
    --max-turns 12 \
    --bare \
    "${resume[@]}" 2>&1) || {
    _log "drain failed (rc=$?): $out"
    return
  }
  local sid
  sid=$(echo "$out" | jq -r .session_id 2>/dev/null || true)
  if [[ -n "$sid" && "$sid" != "null" ]]; then
    echo "{\"session_id\": \"$sid\"}" > "$SESSION_FILE"
  fi
  _log "drain complete (sid=$sid)"
}

_on_change() {
  local state
  state=$(_detect_state)
  case "$state" in
    target_active|interactive_active)
      _log "bypassed: $state"
      ;;
    idle)
      _spawn_drain
      ;;
    *)
      _log "unknown state: $state (treating as idle)"
      _spawn_drain
      ;;
  esac
}

# Debounced fswatch loop
fswatch -o "$INBOX_PATH" --batch-marker --latency "$DEBOUNCE_SECS" | while read -r _; do
  _on_change
done
