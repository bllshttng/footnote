#!/usr/bin/env bash
# WorktreeRemove hook: cleanup with lifecycle awareness
#
# Contract (Claude Code delegation): when this hook is configured, the harness
# does NOT remove the worktree itself - it expects THIS hook to remove it, then
# verifies the path is gone. A log-only hook strands every hook-created
# worktree as an unremovable bg job ("WorktreeRemove hook did not remove
# worktree"). So: preserve active target sessions, refuse the main checkout,
# otherwise actually remove.
set -uo pipefail

INPUT=$(cat)
WORKTREE_PATH=$(printf '%s' "$INPUT" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    print(data.get('worktree_path', ''))
except Exception:
    print('')
" 2>/dev/null || echo "")

[[ -n "$WORKTREE_PATH" ]] || exit 0

MAIN_REPO=$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null | sed 's/\/.git$//')

log_event() {
    local action="$1" extra="${2:-}"
    local ts branch
    ts=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    branch=$(cd "$WORKTREE_PATH" 2>/dev/null && git branch --show-current || echo "unknown")
    echo "{\"ts\":\"$ts\",\"action\":\"$action\",${extra:+$extra,}\"branch\":\"$branch\",\"path\":\"$WORKTREE_PATH\"}" >> "${MAIN_REPO:-.}/.fno/worktree-log.jsonl" 2>/dev/null
}

# Never remove the main checkout, no matter what the job state claims.
if [[ -n "$MAIN_REPO" && "$WORKTREE_PATH" -ef "$MAIN_REPO" ]]; then
    echo "Refusing to remove the main checkout: $WORKTREE_PATH" >&2
    log_event "refuse_remove" "\"reason\":\"main_checkout\""
    exit 1
fi

# Already gone: prune the stale git record and report success.
if [[ ! -d "$WORKTREE_PATH" ]]; then
    [[ -n "$MAIN_REPO" ]] && git -C "$MAIN_REPO" worktree prune 2>/dev/null
    log_event "already_removed"
    exit 0
fi

# Check for an active target session in the worktree. Legacy manifests carried
# status: IN_PROGRESS; the modern immutable manifest has no status field and
# signals liveness with a live owner_pid instead - so a status-only guard would
# fall through and `git worktree remove` a running claimed target's cwd. Mirror
# _wt_live / archive-worktree.sh: preserve on IN_PROGRESS OR a live owner_pid.
ST="$WORKTREE_PATH/.fno/target-state.md"
if [[ -f "$ST" ]]; then
    PRESERVE=""
    grep -qE '^status:[[:space:]]*IN_PROGRESS' "$ST" 2>/dev/null && PRESERVE="active_target"
    if [[ -z "$PRESERVE" ]]; then
        OWNER_PID="$(sed -nE '/^owner_pid:[[:space:]]*[0-9]+/{s/^owner_pid:[[:space:]]*//;p;q;}' "$ST" 2>/dev/null)"
        [[ -n "$OWNER_PID" ]] && kill -0 "$OWNER_PID" 2>/dev/null && PRESERVE="live_owner_pid"
    fi
    if [[ -n "$PRESERVE" ]]; then
        echo "Active target session in worktree, preserving" >&2
        log_event "skip_remove" "\"reason\":\"$PRESERVE\""
        exit 0
    fi
fi

if [[ -n "$MAIN_REPO" ]] && git -C "$MAIN_REPO" worktree remove "$WORKTREE_PATH" 2>/dev/null; then
    git -C "$MAIN_REPO" worktree prune 2>/dev/null
    log_event "removed"
    exit 0
fi

# Not a registered worktree (or removal refused, e.g. dirty). If it is a bare
# leftover dir with no git metadata of its own, clear it; otherwise refuse so
# dirty work is never silently destroyed.
if [[ ! -e "$WORKTREE_PATH/.git" ]]; then
    rm -rf "$WORKTREE_PATH"
    log_event "removed" "\"reason\":\"unregistered_dir\""
    exit 0
fi

echo "Could not remove worktree (dirty or locked): $WORKTREE_PATH" >&2
log_event "remove_failed"
exit 1
