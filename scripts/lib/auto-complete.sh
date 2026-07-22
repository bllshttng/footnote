#!/usr/bin/env bash
# auto-complete.sh - scan events.jsonl for a fresh session_satisfied
# event so the stop hook can take the auto-complete path instead of
# waiting for the LLM to emit <promise>.
#
# Lifted from hooks/target-stop-hook.sh (Phase 2 of stop-hook refactor).
# Behavior is identical to the inline definitions; the helper's only
# consumer is the promise-branch entry condition in the hook.
#
# Constrained sources (check_pr, pr_merge, ci_watcher, fno_gate_manual)
# emit session_satisfied events; the LLM cannot forge one because the
# stop hook checks the event's gate_state_hash against the current
# state-file md5 (staleness check). Auto-complete is an alternative
# entry to <promise>-tag emission; the existing gate audit still runs
# unchanged downstream.
#
# Requires (set by caller):
#   STATE_FILE - path to target-state.md
#   STATE_DIR  - directory containing target-state.md (typically .fno/)
#   log()      - logging function from the hook
#   read_state_field KEY - single-arg form (uses global STATE_FILE);
#                          defined in the hook below the source block.
#
# Side effects:
#   AUTO_COMPLETE_TRIGGER - global; set to the matched event's
#                           data.source value on rc 0. The hook reads
#                           this in the promise branch to discriminate
#                           tag-based completion from event-based.

# compute_md5 FILE
#   Portable across Linux (md5sum), BSD/macOS (md5 -q), and python3
#   fallback. Prints lowercase hex digest on stdout, rc=0 on success.
#   Returns rc=1 if no hashing tool is available or the file does not
#   exist - callers must treat empty stdout as failure.
compute_md5() {
    local file="$1"
    [[ -f "$file" ]] || return 1
    if command -v md5sum >/dev/null 2>&1; then
        md5sum "$file" 2>/dev/null | awk '{print $1}'
    elif command -v md5 >/dev/null 2>&1; then
        md5 -q "$file" 2>/dev/null
    elif command -v python3 >/dev/null 2>&1; then
        python3 -c 'import hashlib,sys; print(hashlib.md5(open(sys.argv[1],"rb").read()).hexdigest())' "$file" 2>/dev/null
    else
        return 1
    fi
}

# check_session_satisfied
#   Look for the most recent session_satisfied event whose session_id
#   matches the current target session AND whose gate_state_hash STILL
#   matches the current state-file md5. Returns rc 0 when one is
#   found and sets AUTO_COMPLETE_TRIGGER to the event's data.source.
#   Returns rc 1 otherwise (no events file, no jq, session_id missing,
#   md5 unavailable, no matching event, or the matching event has gone
#   stale).
check_session_satisfied() {
    local events_file="${STATE_DIR}/events.jsonl"
    if [[ ! -f "$events_file" ]]; then
        # No events file at all is the common case for fresh sessions; log
        # at debug-ish level so the file isn't spammed every iteration.
        return 1
    fi
    if ! command -v jq >/dev/null 2>&1; then
        log "check_session_satisfied: jq not installed - auto-complete disabled"
        return 1
    fi

    local sid
    sid=$(read_state_field "session_id" 2>/dev/null)
    if [[ -z "$sid" || "$sid" == "null" ]]; then
        # `null` is the literal init value before a real session_id is
        # assigned; treat it the same as missing. The Python CLI's
        # complete_session command applies the same guard.
        log "check_session_satisfied: session_id not present (or null) in $STATE_FILE - skipping"
        return 1
    fi

    local current_hash
    current_hash=$(compute_md5 "$STATE_FILE")
    if [[ -z "$current_hash" ]]; then
        log "check_session_satisfied: md5 hashing unavailable (no md5sum/md5/python3) - auto-complete disabled"
        return 1
    fi

    # Pick the most recent session_satisfied event for THIS session whose
    # gate_state_hash STILL matches the current state-file md5. Filtering
    # on the hash inside the jq select() guarantees `tail -1` returns the
    # latest VALID event (not the latest emission that may have gone
    # stale). A stale-then-fresh emission sequence resolves to the fresh
    # one; a fresh-then-stale sequence correctly returns no match.
    local event_json jq_rc
    # grep -F is a cheap pre-filter; the jq select() also matches on .type
    # to avoid false positives if another event type's payload happens to
    # contain the literal string (Gemini PR #286 review). The `|| true`
    # wrapper isolates grep's exit code from the rest of the pipeline:
    # the caller runs under `set -o pipefail`, and grep returning 1 for
    # "no matches" is the common case for fresh sessions. Without the
    # wrapper, the pipeline rc would be 1 on every empty-events
    # iteration, triggering the "possibly corrupt entries" log spuriously.
    event_json=$( (grep -F '"type":"session_satisfied"' "$events_file" 2>/dev/null || true) \
        | jq -c --arg sid "$sid" --arg hash "$current_hash" \
            'select(.type == "session_satisfied" and .data.session_id == $sid and .data.gate_state_hash == $hash)' \
            2>/dev/null \
        | tail -1)
    jq_rc=$?
    if [[ $jq_rc -ne 0 ]]; then
        log "check_session_satisfied: jq pipeline rc=$jq_rc on $events_file (possibly corrupt entries); continuing with whatever it returned"
    fi
    if [[ -z "$event_json" ]]; then
        # Common case: no event yet emitted for this session OR all
        # emitted events are stale. Don't log every iteration; the
        # hook fires often.
        return 1
    fi

    local event_source
    # printf '%s\n' is safer than echo for jq input: avoids backslash
    # interpretation and leading-dash issues in some shells (Gemini review).
    event_source=$(printf '%s\n' "$event_json" | jq -r '.data.source // empty' 2>/dev/null)
    if [[ -z "$event_source" ]]; then
        log "check_session_satisfied: matching event for $sid is missing data.source - ignoring"
        return 1
    fi

    AUTO_COMPLETE_TRIGGER="$event_source"
    log "Found fresh session_satisfied event (source=$event_source) for session $sid - taking auto-complete path"
    return 0
}
