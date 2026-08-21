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

_deny_canonical() {
    _block "Canonical ${1:-checkout} is shared; edit blocked before it lands. For a footnote target, run \`fno do target start <node>\`, then continue in a relocated or new Codex session from the \`worktree=\` path in its receipt. Or use Codex Worktree mode or Handoff before retrying."
}

_location_of() { (cd "$1" && bash "$HELPER" 2>/dev/null); }
_field() { printf '%s\n' "$2" | sed -n "s/^$1=//p" | head -1; }

_block_if_canonical() {
    local location
    location="$(_location_of "$1")" || return
    [[ "$(_field verdict "$location")" == "canonical-protected" ]] || return
    _deny_canonical "$(_field branch "$location")"
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

# A payload that names no object cannot be judged by object: the session cwd is
# the only thing left, and a protected one fails closed exactly as before.
if [[ ${#TARGETS[@]} -eq 0 ]]; then
    _block_if_canonical "$CWD"
    _approve
fi

# ── one protected root, judged per object ─────────────────────────────────────
# The guard owns exactly one checkout: the canonical worktree of the session's
# own git common dir (`worktree list` prints it first). Asking each TARGET's
# directory whether IT is canonical over-blocks a Git-ignored path inside this
# project - `.fno/what-if/` inherits `canonical-protected` from the repo root -
# and over-reaches into an unrelated project's checkout, which this hook is not
# responsible for. Resolving one root instead also costs one helper call rather
# than one per target.
#
# NAMING the root is cheap (one `git worktree list`); asking whether it is
# PROTECTED is not - that verdict resolves the project's worktree policy through
# a Python CLI, and this hook runs on every Edit and Write. So the object
# question is asked first and the verdict only when a target actually looks
# unsafe: a session editing inside its own worktree never pays for it. Failing
# to name the root is not an answer of "not protected" either - a repo whose
# HEAD is corrupt fails `worktree list` while the location helper still reads it
# as canonical-protected off its own `pwd` fallback - so that case hands the
# payload back to the cwd gate rather than approving it.
_undeterminable_root() { _block_if_canonical "$CWD"; _approve; }
_root="$(git -C "$CWD" worktree list --porcelain 2>/dev/null | sed -n 's/^worktree //p' | head -1)"
[[ -n "$_root" && -d "$_root" ]] || _undeterminable_root
PROTECTED_ROOT="$(cd -P "$_root" 2>/dev/null && pwd -P)"
[[ -n "$PROTECTED_ROOT" ]] || _undeterminable_root

# An unsafe target only matters while the root is actually protected.
_unsafe_target() { _block_if_canonical "$PROTECTED_ROOT"; _approve; }

# The inode question is relevant only for existing multiply linked files. Keep
# the normal ignored-path fast path cheap, then compare the rare case against
# every tracked path without losing NUL-delimited filenames.
_link_count() {
    local count
    count="$(stat -c '%h' "$1" 2>/dev/null)" \
        || count="$(stat -f '%l' "$1" 2>/dev/null)" \
        || return 1
    case "$count" in
        ''|*[!0-9]*) return 1 ;;
    esac
    printf '%s\n' "$count"
}

_shares_inode_with_tracked() {
    local phys="$1" statuses
    git -C "$PROTECTED_ROOT" ls-files -z --cached 2>/dev/null | (
        local tracked found=1
        while IFS= read -r -d '' tracked; do
            [[ "$PROTECTED_ROOT/$tracked" -ef "$phys" ]] && found=0
        done
        exit "$found"
    )
    statuses=("${PIPESTATUS[@]}")
    [[ ${statuses[0]} -eq 0 ]] || return 2
    return "${statuses[1]}"
}

# _target_safe PHYSICAL_PATH -> 0 when this write cannot change protected content.
#
# Containment is decided on the PHYSICAL path, so an `internal/` symlink out to
# the vault is external and an ignored symlink back onto a tracked file is not.
# Inside the root, only git may grant the exception: no literal directory name
# is trusted, so a renamed ignored dir keeps working and a tracked file under an
# ignored-looking name does not. `check-ignore` runs WITHOUT `--no-index`
# precisely because that flag reports the ignore-pattern match for a force-added
# TRACKED file, which would hand out the exception for shared content. Exit 1
# (not ignored) and exit 128 (unanswerable) are both refusals.
_target_safe() {
    local phys="$1"
    # A `..` that survived resolution sat under a directory that does not exist,
    # so nothing could fold it away. It is textually under one prefix and points
    # at another: `check-ignore` reads `scratch/nope/../src/app.py` as ignored by
    # `scratch/` while the write lands on tracked `src/app.py`. Refuse.
    case "$phys" in
        */../*|*/..) return 1 ;;
    esac
    case "$phys/" in
        "$PROTECTED_ROOT/"*) ;;
        *) return 0 ;;
    esac
    [[ "$phys" != "$PROTECTED_ROOT" ]] || return 1
    git -C "$PROTECTED_ROOT" check-ignore -q -- "$phys" 2>/dev/null || return 1
    [[ -f "$phys" ]] || return 0
    local links alias_status
    links="$(_link_count "$phys")" || return 1
    (( links > 1 )) || return 0
    _shares_inode_with_tracked "$phys"
    alias_status=$?
    [[ $alias_status -eq 1 ]]
}

# One blocked target denies the whole call: safe siblings cannot launder an
# unsafe one. A target that will not resolve physically is unknown, not safe -
# including when the shared resolver failed to source, which leaves the guard
# with no way to see through a symlink.
declare -F fno_physical_path >/dev/null 2>&1 || _unsafe_target
for t in "${TARGETS[@]}"; do
    phys="$(fno_physical_path "$(_absolute "$t")")" || _unsafe_target
    [[ -n "$phys" ]] || _unsafe_target
    _target_safe "$phys" || _unsafe_target
done

_approve
