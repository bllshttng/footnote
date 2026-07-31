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
# plan-location-guard.sh agree on where a not-yet-created path lives.
_target_directory() {
    fno_resolve_dir "$1"
}

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

# ── plans-dir carve-out (x-5349) ──────────────────────────────────────────────
# The canonical-checkout gate below keys on the SESSION cwd, so a session
# sitting on canonical main was denied every write - including the correct save
# of a plan into the configured plans dir, which is usually reached through the
# internal/ symlink and is not the shared checkout at all. That denial is what
# drove the .fno/drafts-then-mv workaround. A write whose targets all land in
# the configured plans dir is allowed, PROVIDED that directory cannot itself
# reach the shared checkout (fno_plans_dir_carveout_safe decides; a plans dir
# that is an ancestor of the cwd, or a tracked dir inside it, would otherwise
# turn this exemption into a blanket bypass of the gate below).
#
# Gated on a `.md` target first: resolving the plans dir shells a Python CLI
# (~0.5s), and every Edit/Write reaches here. A plan is always markdown, so the
# cheap test keeps that cost off the source-edit path, mirroring the payload
# pre-filter plan-location-guard.sh carries for the same reason.
#
# Requires targets to be known AND resolvable, and requires the WHOLE helper to
# have sourced - bash defines functions as it parses, so a truncated helper
# leaves the first one defined and the rest missing. An unparseable payload or a
# half-sourced helper keeps the old blunt behavior rather than opening a hole.
_has_md_target() {
    local t
    for t in "${TARGETS[@]+"${TARGETS[@]}"}"; do
        [[ "$t" == *.md ]] && return 0
    done
    return 1
}

if [[ ${#TARGETS[@]} -gt 0 ]] && _has_md_target \
   && declare -F fno_plans_dir fno_resolve_dir fno_under_plans_dir \
        fno_plans_dir_carveout_safe >/dev/null 2>&1; then
    # Resolve from the SESSION cwd, not the cwd the harness happened to spawn
    # this hook in: `fno plan path` is repo-anchored, so an ambient cwd names a
    # different project's plans dir and the carve-out silently never fires.
    PLANS_DIR="$(cd "$CWD" 2>/dev/null && fno_plans_dir 2>/dev/null || true)"
    if [[ -n "$PLANS_DIR" ]] && fno_plans_dir_carveout_safe "$PLANS_DIR" "$CWD"; then
        all_in_plans=1
        for t in "${TARGETS[@]}"; do
            fno_under_plans_dir "$PLANS_DIR" "$(_absolute "$t")" || { all_in_plans=0; break; }
        done
        [[ $all_in_plans -eq 1 ]] && _approve
    fi
fi

_block_if_canonical "$CWD"

# A path that resolves to nothing is an anomaly, not a pass: falling through to
# `continue` here would skip the canonical check for that target entirely, which
# is the wrong direction for a guard (the carve-out above fails closed on the
# same resolver failure). Fall back to the session cwd's verdict instead.
for t in "${TARGETS[@]+"${TARGETS[@]}"}"; do
    if target_dir="$(_target_directory "$(_absolute "$t")")" && [[ -n "$target_dir" ]]; then
        _block_if_canonical "$target_dir"
    else
        _block_if_canonical "$CWD"
    fi
done

_approve
