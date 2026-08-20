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

# Return 0 only if the state file describes a target run owned by a LIVE session.
# Returns non-zero if: no state file, empty-input stub, or the node claim is dead.
#
# Liveness truth is the node CLAIM, not the manifest `status:` field (the writer
# no longer emits it) and NOT `owner_pid` (that is the transient `fno target init`
# wrapper pid, dead ~1s after init returns per claims/session_pid.py — it reads a
# live session as inactive). The claim is acquired with the DURABLE session pid
# (nearest claude ancestor) + TTL, so its liveness is the real signal. We delegate
# to `fno claim status` so this never diverges from the canonical classify().
target_is_active() {
    local state_file="${1:-.fno/target-state.md}"
    [[ -f "$state_file" ]] || return 1

    # `|| true` keeps the documented fail-open behavior under a caller's
    # `set -e` + `set -o pipefail`: target_state_field's grep returns non-zero
    # on an absent field, which would otherwise abort the assignment.
    local input plan_path
    input=$(target_state_field "input" "$state_file" || true)
    plan_path=$(target_state_field "plan_path" "$state_file" || true)
    if _target_guard_is_empty_yaml "$input" && _target_guard_is_empty_yaml "$plan_path"; then
        return 1
    fi

    # Claim liveness. A LIVE or SUSPECT (TTL-protected respawn) claim means the
    # session is active; STALE/free/absent means it is not. Fail open (active)
    # when the signal is unavailable — no claim key on the manifest (free-text /
    # pre-claim legacy), or `fno` unreadable — matching the prior backward-compat
    # stance; the stop hook rewrites a genuinely completed manifest anyway.
    local claim_key
    claim_key=$(target_state_field "target_claim_key" "$state_file" || true)
    if _target_guard_is_empty_yaml "$claim_key"; then
        return 0
    fi
    command -v fno >/dev/null 2>&1 || return 0
    local claim_json
    # --no-roster: this reads `state` and nothing else, and the cross-check's
    # whole cost lands on exactly the free/stale branch this hits in the common
    # case. Paying a harness subprocess here buys a field the caller discards.
    # The || falls back to the unflagged call: --no-roster is new, this runs
    # against the DEPLOYED fno, and an older binary rejects an unknown option
    # with empty stdout - which this gate would read as "no claim".
    claim_json=$(fno claim status "$claim_key" -J --no-roster 2>/dev/null \
        || fno claim status "$claim_key" -J 2>/dev/null || true)
    case "$claim_json" in
        "") return 0 ;;
        *'"state": "live"'* | *'"state": "suspect"'*) return 0 ;;
        *) return 1 ;;
    esac
}

# Strict twin of the claim block above: return 0 ONLY when the manifest records
# a claim that reads live/suspect right now. No claim key, no `fno`, or an
# unreadable claim all return 1, so this NEVER preserves on absence of signal -
# use it where a false "live" would block a legitimate teardown, and
# target_is_active where a false "dead" would be worse.
#
# owner_pid is not a substitute and never was. It is the transient
# `fno target init` wrapper pid, dead within about a second of init returning,
# so `kill -0 owner_pid` is false for EVERY live session; a guard resting on it
# alone reads as protection and never once fires.
target_claim_is_live() {
    local state_file="${1:-.fno/target-state.md}"
    [[ -f "$state_file" ]] || return 1
    local claim_key claim_json
    claim_key=$(target_state_field "target_claim_key" "$state_file" || true)
    _target_guard_is_empty_yaml "$claim_key" && return 1
    command -v fno >/dev/null 2>&1 || return 1
    # No `|| true` here, unlike the fail-open twin above: a nonzero exit means
    # the read did not complete, and a truncated payload can still carry the
    # bytes `"state": "live"`. Strict means an incomplete read is not live.
    # --no-roster for the same reason as the twin above: `state` is all this
    # reads, so the cross-check would be a subprocess bought and thrown away.
    # Same deployed-binary fallback as the twin above. The STRICT contract is
    # unchanged: both calls failing still returns 1, so an incomplete read is
    # never read as live.
    claim_json=$(fno claim status "$claim_key" -J --no-roster 2>/dev/null \
        || fno claim status "$claim_key" -J 2>/dev/null) || return 1
    case "$claim_json" in
        *'"state": "live"'* | *'"state": "suspect"'*) return 0 ;;
        *) return 1 ;;
    esac
}
