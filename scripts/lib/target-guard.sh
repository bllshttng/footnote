#!/usr/bin/env bash
# target-guard.sh - Shared helpers for detecting active, session-owned target runs.
#
# Source from hooks that read .fno/target-state.md:
#   source "${CLAUDE_PLUGIN_ROOT}/scripts/lib/target-guard.sh"
#
# The goal: before PR #136, any stale target-state.md in any project would
# activate six separate hooks (cache-keepalive, postcompact-reinject,
# subagent-guard, etc.) in unrelated sessions. PR #136 closed the recreation
# vector; this library closes the consumption vector by gating every read on
# a liveness check.
#
# Functions exposed:
#   target_is_active [state_file]        — 0 if active in a live session, else 1
#   target_state_field <field> [file]    — emit a YAML field value (strips quotes)
#
# No side effects. Pure reads. Stop-hook is the only thing that should archive
# stale state.

# Return 0 if the named field value is YAML-null / empty.
_target_guard_is_empty_yaml() {
    local v="$1"
    [[ -z "$v" || "$v" == "null" || "$v" == "~" ]]
}

# Read a top-level YAML field. Strips surrounding quotes and trailing whitespace.
# Returns empty string if field not present or file missing.
target_state_field() {
    local field="$1"
    local state_file="${2:-.fno/target-state.md}"
    [[ -f "$state_file" ]] || return 0
    # Field name validation: prevent regex/command injection via caller input.
    [[ "$field" =~ ^[a-z_][a-z0-9_]*$ ]] || return 1
    grep -E "^${field}:" "$state_file" 2>/dev/null \
        | head -1 \
        | sed -e "s/^${field}:[[:space:]]*//" -e 's/[[:space:]]*$//' \
        | tr -d '"' | tr -d "'"
}

# Return 0 only if the state file describes a target run owned by a live process.
# Returns non-zero if: no state file, empty-input stub, or owner_pid is dead.
# Liveness truth is owner_pid, not a status field: the manifest writer no longer
# emits `status:`, so gating on it returned "not active" for every live session
# and silently disabled every consumer (x-6044). A legacy manifest that still
# carries `status:` is unaffected — that field is simply not read here anymore.
target_is_active() {
    local state_file="${1:-.fno/target-state.md}"
    [[ -f "$state_file" ]] || return 1

    local input plan_path
    input=$(target_state_field "input" "$state_file")
    plan_path=$(target_state_field "plan_path" "$state_file")
    if _target_guard_is_empty_yaml "$input" && _target_guard_is_empty_yaml "$plan_path"; then
        return 1
    fi

    # Owner liveness. Absent owner_pid means pre-session-owner state (written
    # by a previous abilities version) — treat as active for backward compat.
    # The stop hook will rewrite such states on the next completion anyway.
    local owner_pid
    owner_pid=$(target_state_field "owner_pid" "$state_file")
    if ! _target_guard_is_empty_yaml "$owner_pid" && [[ "$owner_pid" =~ ^[0-9]+$ ]]; then
        kill -0 "$owner_pid" 2>/dev/null || return 1
    fi

    return 0
}
