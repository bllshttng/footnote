#!/usr/bin/env bash
# Hook-only event log (typed-blocker phase 01b).
#
# Trust boundary
# ──────────────
# This helper writes to .fno/hook-events.jsonl with mode 0600.
# That file is the trust surface for stop-hook BLOCKED-transition
# verification: the cancel-signal verifier (typed-blocker phase 03)
# reads ONLY this log, never events.jsonl. The split exists so that
# any LLM tool invocation that reaches bash CANNOT forge a cancel
# event - LLM-reachable code only ever writes to the LLM-writable
# scripts/lib/events.sh::emit_event[_raw] target (events.jsonl).
#
# Permitted writers (and ONLY these):
#   - hooks/target-stop-hook.sh
#   - hooks/target-subagent-guard.sh
#   - hooks/session-start.sh
#   - hooks/megawalk-stop-hook.sh
#   - scripts/run-target-loop.sh (outer cancel signaler)
#
# If you find yourself sourcing this from a skill, an agent file, or
# any other LLM-invokable code path, that is a bug. The whole point
# is that the LLM cannot reach the writer.

HOOK_EVENTS_FILE="${HOOK_EVENTS_FILE:-.fno/hook-events.jsonl}"

# Idempotent init: ensure the file exists and is mode 0600. Safe to
# call from every hook spawn; a no-op when already correct.
_hook_events_init() {
    mkdir -p "$(dirname "$HOOK_EVENTS_FILE")" 2>/dev/null || true
    if [[ ! -f "$HOOK_EVENTS_FILE" ]]; then
        : > "$HOOK_EVENTS_FILE" 2>/dev/null || true
    fi
    chmod 0600 "$HOOK_EVENTS_FILE" 2>/dev/null || true
}

# emit_hook_event TYPE [JSON]
#
# Append a single JSONL record with shape {ts, type, data} matching
# emit_event_raw in scripts/lib/events.sh. Silent-safe: any failure
# (jq missing, disk full, permission error) is swallowed so callers
# never crash on telemetry. The cancel verifier treats absence as
# "no signal", which is the correct conservative default.
emit_hook_event() {
    local type="${1:?type required}"
    # Default to empty JSON object {} when no payload is given. Cannot
    # inline the default as `${2:-{}}` - bash parses that as `${2:-{}`
    # (default `{`) followed by a literal `}`, corrupting both the
    # default-case and the arg-case. Assign-then-default avoids the
    # parser ambiguity entirely.
    local json="${2:-}"
    [[ -z "$json" ]] && json='{}'
    _hook_events_init
    command -v jq >/dev/null 2>&1 || {
        # jq missing: log to stderr but never fail the caller. The
        # silent-safe default is continue-the-loop, not crash-the-hook.
        echo "hook-events: WARN: jq unavailable; skipping ${type}" >&2
        return 0
    }
    jq -nc \
        --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        --arg type "$type" \
        --argjson data "$json" \
        '{ts: $ts, type: $type, data: $data}' \
        >> "$HOOK_EVENTS_FILE" 2>/dev/null || true
}

# has_hook_event TYPE SESSION_ID
#
# Return 0 iff the hook-only log contains at least one entry with the
# given type AND data.session_id. Used by the cancel verifier to
# decide whether a BLOCKED status was authored by an external signal
# (legitimate) or forged by the LLM (rejected).
#
# Silent-safe: missing file returns non-zero (no signal), never errors.
has_hook_event() {
    local type="${1:?type required}"
    local session_id="${2:?session_id required}"
    [[ -f "$HOOK_EVENTS_FILE" ]] || return 1
    command -v jq >/dev/null 2>&1 || return 1
    jq -re \
        --arg type "$type" \
        --arg sid "$session_id" \
        'select(.type == $type and .data.session_id == $sid) | .ts' \
        "$HOOK_EVENTS_FILE" 2>/dev/null | head -1 | grep -q .
}
