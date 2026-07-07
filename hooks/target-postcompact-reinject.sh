#!/usr/bin/env bash
# PostCompact hook: re-inject plan goal + current phase into context
# Fires after context compaction completes. Outputs additionalContext
# so the model doesn't lose its bearings after compaction.
set -uo pipefail

STATE_FILE=".fno/target-state.md"
FNO_DIR=".fno"

# Guard (c) re-surface: if a handoff was armed pre-compaction (by
# arm-handoff-precompact.sh), nudge the agent to run it at the next wave
# boundary. Computed BEFORE the reinject gate below because the armed marker is
# self-gated (the arm hook already checked liveness + pressure) and
# session-scoped, so it must surface even when target_is_active is false.
GUARD_LIB="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}/scripts/lib/target-guard.sh"
HANDOFF_NUDGE=""
emit_context() {
    python3 -c "
import json, sys
print(json.dumps({'additionalContext': sys.argv[1]}))
" "$1" 2>/dev/null
}

# Compute the armed-handoff nudge (if any) and apply the reinject gate in one
# guard-lib block - the goal reminder only fires for a live session, but an
# armed nudge surfaces regardless.
if [[ -f "$GUARD_LIB" ]]; then
    # shellcheck source=../scripts/lib/target-guard.sh
    source "$GUARD_LIB"
    if [[ -f "$STATE_FILE" ]]; then
        _SID="$(target_state_field session_id "$STATE_FILE" 2>/dev/null || true)"
        if [[ -n "$_SID" && -f "$FNO_DIR/.handoff-armed-$_SID" ]]; then
            _NODE="$(target_state_field graph_node_id "$STATE_FILE" 2>/dev/null || true)"
            HANDOFF_NUDGE="**Handoff armed:** you are past the context-handoff threshold with outstanding work on ${_NODE:-this node}. Run handoff.sh (skills/target/scripts/handoff.sh --boundary wave) at the NEXT wave boundary to hand off to a fresh-context successor - never mid-wave. The marker clears once handoff.sh runs."
        fi
    fi
    # Only reinject the goal when target is actively owned by this session. Stale
    # state from a prior session would otherwise inject a dead goal into an
    # unrelated compaction event.
    if ! target_is_active "$STATE_FILE"; then
        [[ -n "$HANDOFF_NUDGE" ]] && emit_context "$HANDOFF_NUDGE"
        exit 0
    fi
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

[[ -n "$HANDOFF_NUDGE" ]] && CONTEXT="${CONTEXT}

${HANDOFF_NUDGE}"

# Output as additionalContext JSON
emit_context "$CONTEXT"

exit 0
