#!/usr/bin/env bash
# WorktreeRemove hook: cleanup with lifecycle awareness
#
# Fires when a worktree is being removed (session exit or subagent finishes).
# Checks for active target sessions before allowing removal.
# Logs lifecycle events for tracking.
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

# Check for active target session in the worktree
if [[ -f "$WORKTREE_PATH/.fno/target-state.md" ]]; then
    STATUS=$(grep '^status:' "$WORKTREE_PATH/.fno/target-state.md" 2>/dev/null | awk '{print $2}')
    if [[ "$STATUS" == "IN_PROGRESS" ]]; then
        echo "Active target session in worktree, preserving" >&2
        # Exit 0 so we don't block CC, but log the skip
        TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
        BRANCH=$(cd "$WORKTREE_PATH" 2>/dev/null && git branch --show-current || echo "unknown")
        echo "{\"ts\":\"$TS\",\"action\":\"skip_remove\",\"reason\":\"active_target\",\"branch\":\"$BRANCH\",\"path\":\"$WORKTREE_PATH\"}" >> "${MAIN_REPO:-.}/.fno/worktree-log.jsonl" 2>/dev/null
        exit 0
    fi
fi

# Log lifecycle event
TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
BRANCH=$(cd "$WORKTREE_PATH" 2>/dev/null && git branch --show-current || echo "unknown")
echo "{\"ts\":\"$TS\",\"action\":\"removed\",\"branch\":\"$BRANCH\",\"path\":\"$WORKTREE_PATH\"}" >> "${MAIN_REPO:-.}/.fno/worktree-log.jsonl" 2>/dev/null

exit 0
