#!/usr/bin/env bash

set -uo pipefail

_approve() {
    printf '%s\n' '{}'
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
PATCH_COMMAND=""
if command -v jq >/dev/null 2>&1; then
    CWD="$(printf '%s' "$PAYLOAD" | jq -er '.cwd | select(type == "string" and length > 0)' 2>/dev/null || true)"
    PATCH_COMMAND="$(printf '%s' "$PAYLOAD" | jq -er '.tool_input.command | select(type == "string" and length > 0)' 2>/dev/null || true)"
elif command -v python3 >/dev/null 2>&1; then
    _json_field() {
        printf '%s' "$PAYLOAD" | python3 -c '
import json, sys
try:
    payload = json.load(sys.stdin)
    value = payload.get("cwd") if sys.argv[1] == "cwd" else payload.get("tool_input", {}).get("command")
    if isinstance(value, str) and value:
        print(value)
except Exception:
    pass
' "$1" 2>/dev/null || true
    }
    CWD="$(_json_field cwd)"
    PATCH_COMMAND="$(_json_field command)"
else
    _approve
fi

[[ -n "$CWD" && -d "$CWD" ]] || _approve

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPER="$HOOK_DIR/helpers/check-impl-location.sh"
[[ -f "$HELPER" ]] || _approve

_block_if_canonical() {
    local dir="$1" location verdict branch
    location="$(cd "$dir" && bash "$HELPER" 2>/dev/null)" || return
    verdict="$(printf '%s\n' "$location" | sed -n 's/^verdict=//p' | head -1)"
    [[ "$verdict" == "canonical-protected" ]] || return

    branch="$(printf '%s\n' "$location" | sed -n 's/^branch=//p' | head -1)"
    _block "Canonical ${branch:-checkout} is shared; edit blocked before it lands. For a footnote target, run \`fno target start <node>\`, then continue in a relocated or new Codex session from the \`worktree=\` path in its receipt. Or use Codex Worktree mode or Handoff before retrying."
}

_target_directory() {
    local target="$1" link parent hops=0
    while [[ -L "$target" ]]; do
        hops=$((hops + 1))
        [[ $hops -le 40 ]] || return 1
        link="$(readlink "$target" 2>/dev/null)" || return 1
        if [[ "$link" == /* ]]; then
            target="$link"
        else
            target="$(dirname "$target")/$link"
        fi
    done

    [[ -d "$target" ]] || target="$(dirname "$target")"
    while [[ ! -d "$target" ]]; do
        parent="$(dirname "$target")"
        [[ "$parent" != "$target" ]] || return 1
        target="$parent"
    done
    cd -P "$target" 2>/dev/null && pwd -P
}

_block_if_canonical "$CWD"
[[ -n "$PATCH_COMMAND" ]] || _approve

while IFS= read -r line; do
    case "$line" in
        "*** Add File: "*|"*** Update File: "*|"*** Delete File: "*|"*** Move to: "*)
            path="${line#*: }"
            path="${path%$'\r'}"
            [[ -n "$path" ]] || continue

            if [[ "$path" == /* ]]; then
                target="$path"
            else
                target="$CWD/$path"
            fi
            target_dir="$(_target_directory "$target")" || continue
            _block_if_canonical "$target_dir"
            ;;
    esac
done <<< "$PATCH_COMMAND"

_approve
