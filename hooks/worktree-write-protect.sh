#!/usr/bin/env bash

set -uo pipefail

_approve() {
    printf '%s\n' '{"decision":"approve","hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}'
    exit 0
}

_block() {
    local reason="$1"
    if command -v jq >/dev/null 2>&1; then
        jq -nc --arg reason "$reason" '{
            decision: "block",
            reason: $reason,
            hookSpecificOutput: {
                hookEventName: "PreToolUse",
                permissionDecision: "deny",
                permissionDecisionReason: $reason
            }
        }'
    else
        python3 -c 'import json,sys; r=sys.argv[1]; print(json.dumps({"decision":"block","reason":r,"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":r}}))' "$reason"
    fi
    exit 0
}

PAYLOAD="$(cat)"
CWD=""
if command -v jq >/dev/null 2>&1; then
    CWD="$(printf '%s' "$PAYLOAD" | jq -er '.cwd | select(type == "string" and length > 0)' 2>/dev/null || true)"
elif command -v python3 >/dev/null 2>&1; then
    CWD="$(printf '%s' "$PAYLOAD" | python3 -c '
import json, sys
try:
    value = json.load(sys.stdin).get("cwd")
    if isinstance(value, str) and value:
        print(value)
except Exception:
    pass
' 2>/dev/null || true)"
else
    _approve
fi

[[ -n "$CWD" && -d "$CWD" ]] || _approve

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPER="$HOOK_DIR/helpers/check-impl-location.sh"
[[ -f "$HELPER" ]] || _approve

LOCATION="$(cd "$CWD" && bash "$HELPER" 2>/dev/null)" || _approve
VERDICT="$(printf '%s\n' "$LOCATION" | sed -n 's/^verdict=//p' | head -1)"
[[ "$VERDICT" == "canonical-protected" ]] || _approve

BRANCH="$(printf '%s\n' "$LOCATION" | sed -n 's/^branch=//p' | head -1)"
_block "Canonical ${BRANCH:-checkout} is shared; edit blocked before it lands. For a footnote target, run \`fno target start <node>\`, then continue in a relocated or new Codex session from the \`worktree=\` path in its receipt. Or use Codex Worktree mode or Handoff before retrying."
