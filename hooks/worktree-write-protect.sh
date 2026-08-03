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
FILE_PATH=""
if command -v jq >/dev/null 2>&1; then
    CWD="$(printf '%s' "$PAYLOAD" | jq -er '.cwd | select(type == "string" and length > 0)' 2>/dev/null || true)"
    PATCH_COMMAND="$(printf '%s' "$PAYLOAD" | jq -er '.tool_input.command | select(type == "string" and length > 0)' 2>/dev/null || true)"
    FILE_PATH="$(printf '%s' "$PAYLOAD" | jq -er '.tool_input.file_path | select(type == "string" and length > 0)' 2>/dev/null || true)"
elif command -v python3 >/dev/null 2>&1; then
    _json_field() {
        printf '%s' "$PAYLOAD" | python3 -c '
import json, sys
try:
    payload = json.load(sys.stdin)
    key = sys.argv[1]
    value = payload.get("cwd") if key == "cwd" else payload.get("tool_input", {}).get(key)
    if isinstance(value, str) and value:
        print(value)
except Exception:
    pass
' "$1" 2>/dev/null || true
    }
    CWD="$(_json_field cwd)"
    PATCH_COMMAND="$(_json_field command)"
    FILE_PATH="$(_json_field file_path)"
else
    _approve
fi

[[ -n "$CWD" && -d "$CWD" ]] || _approve

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPER="$HOOK_DIR/helpers/check-impl-location.sh"
[[ -f "$HELPER" ]] || _approve
# shellcheck source=helpers/plans-dir.sh
source "$HOOK_DIR/helpers/plans-dir.sh" 2>/dev/null || true

_block_if_canonical() {
    local dir="$1" location verdict branch
    location="$(cd "$dir" && bash "$HELPER" 2>/dev/null)" || return
    verdict="$(printf '%s\n' "$location" | sed -n 's/^verdict=//p' | head -1)"
    [[ "$verdict" == "canonical-protected" ]] || return

    branch="$(printf '%s\n' "$location" | sed -n 's/^branch=//p' | head -1)"
    _block "Canonical ${branch:-checkout} is shared; edit blocked before it lands. For a footnote target, run \`fno target start <node>\`, then continue in a relocated or new Codex session from the \`worktree=\` path in its receipt. Or use Codex Worktree mode or Handoff before retrying."
}

# _target_directory delegates to the shared resolver so this guard and
# plan-location-guard.sh agree on where a not-yet-created path lives. The
# fallback matters: the source above is swallowed, and without it a missing
# helper would make every per-target check exit 127 and skip - leaving the
# degraded mode WEAKER than the guard was before the helper existed.
if declare -F fno_resolve_dir >/dev/null 2>&1; then
    _target_directory() { fno_resolve_dir "$1"; }
else
    _target_directory() { dirname "$1"; }
fi

# _absolute PATH -> PATH anchored against the session cwd when relative.
_absolute() {
    case "$1" in
        /*) printf '%s\n' "$1" ;;
        *)  printf '%s\n' "$CWD/$1" ;;
    esac
}

# Collect every path this call writes: a file_path payload contributes one, an
# apply_patch command contributes its header paths.
TARGETS=()
[[ -n "$FILE_PATH" ]] && TARGETS+=("$FILE_PATH")
if [[ -n "$PATCH_COMMAND" ]]; then
    while IFS= read -r line; do
        case "$line" in
            "*** Add File: "*|"*** Update File: "*|"*** Delete File: "*|"*** Move to: "*)
                path="${line#*: }"
                path="${path%$'\r'}"
                [[ -n "$path" ]] && TARGETS+=("$path")
                ;;
        esac
    done <<< "$PATCH_COMMAND"
fi

# The session cwd is authoritative only when the payload does not identify an
# object. When targets are present, each target's own directory is the complete
# safety question; checking the session first would reject a valid worktree
# write solely because the conversation began on canonical main.
if [[ ${#TARGETS[@]} -eq 0 ]]; then
    _block_if_canonical "$CWD"
fi

# Known ceiling: a target that resolves to nothing is skipped. The `dirname`
# fallback above keeps a missing resolver from routing every target down this
# branch; a payload with no targets still takes the fail-closed cwd path above.
for t in "${TARGETS[@]+"${TARGETS[@]}"}"; do
    target_dir="$(_target_directory "$(_absolute "$t")")" || continue
    [[ -n "$target_dir" ]] || continue
    _block_if_canonical "$target_dir"
done

_approve
