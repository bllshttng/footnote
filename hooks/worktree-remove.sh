#!/usr/bin/env bash
# WorktreeRemove hook: cleanup with lifecycle awareness
#
# Contract (Claude Code delegation): when this hook is configured, the harness
# does NOT remove the worktree itself - it expects THIS hook to remove it. A
# log-only hook strands every hook-created worktree as an unremovable bg job
# ("WorktreeRemove hook did not remove worktree"). So: preserve active target
# sessions, refuse the main checkout, otherwise actually remove.
#
# EXIT CODE IS THE WHOLE SIGNAL. The harness never stats the path afterward; it
# reads exit 0 as "removed" and deletes the job record. So exit 0 ONLY when the
# worktree is actually gone - a preserve or a refusal must exit non-zero, or the
# job record is deleted out from under a worktree that still exists and nothing
# points at it anymore.
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

# Check for an active target session in the worktree, or `git worktree remove`
# takes a running claimed target's cwd out from under it. Three signals, in
# descending order of trustworthiness: a legacy `status: IN_PROGRESS`, a LIVE
# NODE CLAIM (session-pid anchored + TTL - the only durable one), and a live
# owner_pid. owner_pid is kept last and only as a positive signal: it is the
# transient `fno target init` wrapper pid, dead about a second after init
# returns, so before the claim check this guard preserved essentially nothing.
ST="$WORKTREE_PATH/.fno/target-state.md"
if [[ -f "$ST" ]]; then
    PRESERVE=""
    grep -qE '^status:[[:space:]]*IN_PROGRESS' "$ST" 2>/dev/null && PRESERVE="active_target"
    if [[ -z "$PRESERVE" ]]; then
        GUARD_LIB="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}/scripts/lib/target-guard.sh"
        # shellcheck source=../scripts/lib/target-guard.sh
        [[ -f "$GUARD_LIB" ]] && source "$GUARD_LIB" 2>/dev/null \
            && target_claim_is_live "$ST" && PRESERVE="live_claim"
    fi
    if [[ -z "$PRESERVE" ]]; then
        OWNER_PID="$(sed -nE '/^owner_pid:[[:space:]]*[0-9]+/{s/^owner_pid:[[:space:]]*//;p;q;}' "$ST" 2>/dev/null)"
        [[ -n "$OWNER_PID" ]] && kill -0 "$OWNER_PID" 2>/dev/null && PRESERVE="live_owner_pid"
    fi
    if [[ -n "$PRESERVE" ]]; then
        echo "Active target session in worktree, preserving: $WORKTREE_PATH" >&2
        log_event "skip_remove" "\"reason\":\"$PRESERVE\""
        exit 1
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
    # A worktree is the largest thing fno deletes. Trash-moving it reclaims
    # nothing. command -p rm + /bin/rm, never bare rm. See
    # docs/architecture/disposable-deletes.md.
    command -p rm -rf "$WORKTREE_PATH" 2>/dev/null || /bin/rm -rf "$WORKTREE_PATH"
    log_event "removed" "\"reason\":\"unregistered_dir\""
    exit 0
fi

echo "Could not remove worktree (dirty or locked): $WORKTREE_PATH" >&2
log_event "remove_failed"
exit 1
