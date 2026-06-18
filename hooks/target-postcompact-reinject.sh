#!/usr/bin/env bash
# PostCompact hook: re-inject plan goal + current phase into context
# Fires after context compaction completes. Outputs additionalContext
# so the model doesn't lose its bearings after compaction.
set -uo pipefail

STATE_FILE=".fno/target-state.md"

# Only reinject when target is actively owned by this session. Stale state
# from a prior session would otherwise inject a dead goal into an unrelated
# compaction event.
GUARD_LIB="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}/scripts/lib/target-guard.sh"
if [[ -f "$GUARD_LIB" ]]; then
    # shellcheck source=../scripts/lib/target-guard.sh
    source "$GUARD_LIB"
    target_is_active "$STATE_FILE" || exit 0
else
    # Fallback: old inline check if the guard lib is somehow unavailable.
    [[ -f "$STATE_FILE" ]] || exit 0
    STATUS=$(grep '^status:' "$STATE_FILE" 2>/dev/null | awk '{print $2}')
    [[ "$STATUS" == "IN_PROGRESS" ]] || exit 0
fi

# Extract key state fields. The manifest is inputs-only post-wedge (ab-d0337fbc):
# no current_phase / iteration / *_passed gate booleans live here anymore
# (ab-88f0854d removed those dead reads). Progress is external now - git HEAD,
# PR/CI, the loop-check verb - surfaced via `fno whoami` / `fno status`.
GOAL=$(grep '^input:' "$STATE_FILE" 2>/dev/null | head -1 | sed 's/^input: *//' | sed 's/^"//' | sed 's/"$//')
PLAN_PATH=$(grep '^plan_path:' "$STATE_FILE" 2>/dev/null | head -1 | sed 's/^plan_path: *//' | tr -d '"')
NODE=$(grep '^graph_node_id:' "$STATE_FILE" 2>/dev/null | head -1 | sed 's/^graph_node_id: *//' | tr -d '"' | tr -d "'")

# Build re-injection context
CONTEXT="## Post-Compaction Context Reminder

**Goal:** $GOAL"
[[ -n "$NODE" && "$NODE" != "null" ]] && CONTEXT="${CONTEXT}
**Backlog node:** $NODE"

# If plan path exists, add task count
if [[ -n "$PLAN_PATH" && -d "$PLAN_PATH" ]]; then
    TOTAL_TASKS=$(grep -c '### Task' "$PLAN_PATH"/*.md 2>/dev/null | awk -F: '{s+=$NF}END{print s+0}')
    CONTEXT="${CONTEXT}
**Plan:** $PLAN_PATH ($TOTAL_TASKS tasks)"
fi

CONTEXT="${CONTEXT}

Progress is not in the manifest. Run \`fno whoami\` then \`fno status\`
for live phase + completion state (git HEAD, PR/CI, review)."

# Output as additionalContext JSON
python3 -c "
import json, sys
print(json.dumps({'additionalContext': sys.argv[1]}))
" "$CONTEXT" 2>/dev/null

exit 0
