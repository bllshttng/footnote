#!/usr/bin/env bash
# SubagentStart/SubagentStop hook: git checkpoints around subagent execution
# SubagentStart: stash uncommitted changes as a recovery point
# SubagentStop: log completion, optionally verify build
set -uo pipefail

STATE_FILE=".fno/target-state.md"

# Only act during a live, session-owned target. Stale state from a prior
# session would otherwise create noisy stashes around unrelated subagent runs.
GUARD_LIB="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}/scripts/lib/target-guard.sh"
if [[ -f "$GUARD_LIB" ]]; then
    # shellcheck source=../scripts/lib/target-guard.sh
    source "$GUARD_LIB"
    target_is_active "$STATE_FILE" || exit 0
else
    [[ -f "$STATE_FILE" ]] || exit 0
    STATUS=$(grep '^status:' "$STATE_FILE" 2>/dev/null | awk '{print $2}')
    [[ "$STATUS" == "IN_PROGRESS" ]] || exit 0
fi

# REPO_ROOT-anchored (not bare cwd-relative) events.jsonl, per the placement
# rule (ab-f063 Wave 2) - avoids the nested ~/.fno/.fno accident class.
REPO_ROOT=$(git rev-parse --show-toplevel 2>/dev/null || echo "$PWD")
EVENTS_LOG="${REPO_ROOT}/.fno/events.jsonl"

INPUT=$(cat)

EVENT=$(echo "$INPUT" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    print(data.get('hook_event_name', ''))
except Exception:
    print('')
" 2>/dev/null || echo "")

TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)

case "$EVENT" in
    SubagentStart)
        AGENT_NAME=$(echo "$INPUT" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    name = data.get('agent_name', data.get('description', 'unknown'))
    print(name[:40])
except Exception:
    print('unknown')
" 2>/dev/null || echo "unknown")

        # Only create checkpoint if there are uncommitted changes
        if git diff --quiet 2>/dev/null && git diff --cached --quiet 2>/dev/null; then
            exit 0
        fi

        # Use git stash create + store to create a recoverable ref
        # WITHOUT modifying the working tree (stash push would remove files)
        STASH_MSG="abilities-checkpoint-before-${AGENT_NAME// /-}"
        STASH_SHA=$(git stash create "$STASH_MSG" 2>/dev/null)
        if [[ -n "$STASH_SHA" ]]; then
            git stash store -m "$STASH_MSG" "$STASH_SHA" 2>/dev/null || true
        fi

        printf '{"ts":"%s","type":"subagent_stash","agent":"%s","stash":"%s"}\n' "$TS" "$AGENT_NAME" "$STASH_MSG" >> "$EVENTS_LOG" 2>/dev/null
        ;;

    SubagentStop)
        AGENT_NAME=$(echo "$INPUT" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    name = data.get('agent_name', data.get('description', 'unknown'))
    print(name[:40])
except Exception:
    print('unknown')
" 2>/dev/null || echo "unknown")

        echo "{\"ts\":\"$TS\",\"type\":\"subagent_done\",\"agent\":\"$AGENT_NAME\"}" >> "$EVENTS_LOG" 2>/dev/null

        # Pop stash if one was created for this agent
        STASH_MSG="abilities-checkpoint-before-${AGENT_NAME// /-}"
        STASH_REF=$(git stash list 2>/dev/null | grep "$STASH_MSG" | head -1 | cut -d: -f1)
        if [[ -n "$STASH_REF" ]]; then
            # Don't pop - just log that a recovery point exists.
            # Popping could conflict with the agent's changes.
            echo "{\"ts\":\"$TS\",\"type\":\"subagent_stash_available\",\"agent\":\"$AGENT_NAME\",\"ref\":\"$STASH_REF\"}" >> "$EVENTS_LOG" 2>/dev/null
        fi
        ;;
esac

exit 0
