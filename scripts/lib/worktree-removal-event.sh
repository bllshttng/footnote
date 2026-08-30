#!/usr/bin/env bash
# worktree-removal-event.sh - one worktree_removed event row per removal.
#
# Sourced by every removal executor (archive-worktree.sh for the guarded path
# the merged sweep and the post-merge ritual reach; worktree-lifecycle.sh for
# the age sweep's direct remove). Before this, NO removal path emitted
# anything: a live worker losing its tree left no row, and 2239 journal rows
# contained zero removals, so recurrence was unattributable. The row records
# what the guards read at decision time, so a removal that skipped the claim
# check is visible after the fact, not only prevented before it.
#
# JSON string escaping is minimal by design: backslash and double quote, the
# two characters that break a JSON string literal. Path, branch, and evidence
# strings come from git and fno-managed layout, not free text.

_wt_json_escape() {
    printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

# _wt_emit_removal_event <repo-root> <path> <caller> <claim> <reason> [branch] [forced]
#
# Best-effort but LOUD: a failed emit never blocks the removal (a broken
# journal must not strand every worktree on disk), and the failure line names
# the lost row. Resolution prefers the repo that ships this script - its
# venv, then any fno-py on PATH, then the deployed fno - because the event
# schema travels with this repo: a deployed fno one release older refuses the
# new type, and repo-first means rows land the same day the branch does.
_wt_emit_removal_event() {
    local root="$1" path="$2" caller="$3" claim="$4" reason="$5"
    local branch="${6:-}" forced="${7:-false}"
    local data rc=1
    data="$(printf '{"path":"%s","caller":"%s","claim":"%s","reason":"%s","branch":"%s","forced":%s}' \
        "$(_wt_json_escape "$path")" "$(_wt_json_escape "$caller")" \
        "$(_wt_json_escape "$claim")" "$(_wt_json_escape "$reason")" \
        "$(_wt_json_escape "$branch")" "$forced")"
    if [[ -n "$root" && -x "$root/cli/.venv/bin/python" ]]; then
        PYTHONPATH="$root/cli/src" "$root/cli/.venv/bin/python" -m fno.cli doctor event emit \
            -t worktree_removed -s bash -d "$data" >/dev/null 2>&1 && rc=0
    fi
    if [[ "$rc" -ne 0 ]] && command -v fno-py >/dev/null 2>&1; then
        fno-py doctor event emit -t worktree_removed -s bash -d "$data" >/dev/null 2>&1 && rc=0
    fi
    if [[ "$rc" -ne 0 ]] && command -v fno >/dev/null 2>&1; then
        fno doctor event emit -t worktree_removed -s bash -d "$data" >/dev/null 2>&1 && rc=0
    fi
    if [[ "$rc" -ne 0 ]]; then
        echo "worktree-removal-event: row NOT emitted for $path caller=$caller (no usable fno); removal proceeds, row lost" >&2
    fi
    return 0
}
