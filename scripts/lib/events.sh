#!/usr/bin/env bash
# Event log library for target observability
# Appends structured JSONL events to .fno/events.jsonl
# Usage: source this file, then call emit_event

EVENTS_FILE="${EVENTS_FILE:-.fno/events.jsonl}"

emit_event() {
    local source="${1:?source required}"
    local type="${2:?type required}"
    local data
    data="${3}"
    [[ -z "$data" ]] && data='{}'

    mkdir -p "$(dirname "$EVENTS_FILE")" 2>/dev/null || true

    # Use jq for safe JSON construction (handles special chars)
    jq -nc \
        --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        --arg src "$source" \
        --arg type "$type" \
        --argjson data "$data" \
        '{timestamp: $ts, source: $src, type: $type, data: $data}' \
        >> "$EVENTS_FILE" 2>/dev/null || true
}

# emit_event_raw TYPE JSON
#
# Gate-provenance events (phase_init, phase_transition, phase_rolled_back)
# use this form: the caller passes a top-level `type` and a JSON data
# payload, the helper adds the timestamp. Distinct from emit_event so the
# stop hook's cross-check can filter by `.type` without nested parsing.
emit_event_raw() {
    local type="${1:?type required}"
    # Default to empty JSON object {} when no payload is given. Cannot
    # inline the default as `${2:-{}}` - bash parses that as `${2:-{}`
    # (default `{`) followed by a literal `}`, corrupting both the
    # default-case and the arg-case. Assign-then-default avoids the
    # parser ambiguity entirely.
    local json="${2:-}"
    [[ -z "$json" ]] && json='{}'
    local events_path="${EVENTS_FILE:-.fno/events.jsonl}"
    mkdir -p "$(dirname "$events_path")" 2>/dev/null || true
    jq -nc \
        --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        --arg type "$type" \
        --argjson data "$json" \
        '{ts: $ts, type: $type, data: $data}' \
        >> "$events_path" 2>/dev/null || true
}

# emit_polling_external_review key=value [key=value ...]
#
# Emits the polling_external_review event used by /pr check to register
# external-review polling as progress for the thrash detector. The fifth
# thrash-fingerprint signal counts these lines; the 30-minute exemption
# window in check_no_progress_thrash reads the most-recent next_check_at.
#
# Required keys: pr_number, reviewer_bot, wait_kind (cron|inline), session_id.
# Optional keys: next_check_at, nonce.
#
# rc=0  emitted (line appended to EVENTS_FILE)
# rc=1  validation failure (missing required, unknown wait_kind, schema reject)
# rc=2  substrate failure (jq/schema unavailable)
#
# stderr explains the failure; stdout is empty on rc=0.
emit_polling_external_review() {
    local source_id="${EMIT_SOURCE_ID:-target}"
    local pr_number="" reviewer_bot="" wait_kind="" session_id=""
    local next_check_at="" nonce=""
    local arg key val
    for arg in "$@"; do
        key="${arg%%=*}"
        val="${arg#*=}"
        case "$key" in
            pr_number)      pr_number="$val" ;;
            reviewer_bot)   reviewer_bot="$val" ;;
            wait_kind)      wait_kind="$val" ;;
            session_id)     session_id="$val" ;;
            next_check_at)  next_check_at="$val" ;;
            nonce)          nonce="$val" ;;
            source)         source_id="$val" ;;
            *)
                printf 'emit_polling_external_review: unknown key %s\n' "$key" >&2
                return 1
                ;;
        esac
    done

    if [[ -z "$pr_number" ]]; then
        printf 'emit_polling_external_review: missing pr_number\n' >&2
        return 1
    fi
    if [[ -z "$reviewer_bot" ]]; then
        printf 'emit_polling_external_review: missing reviewer_bot\n' >&2
        return 1
    fi
    if [[ "$wait_kind" != "cron" && "$wait_kind" != "inline" ]]; then
        printf 'emit_polling_external_review: invalid wait_kind=%s (allowed: cron|inline)\n' "$wait_kind" >&2
        return 1
    fi
    if [[ -z "$session_id" ]]; then
        printf 'emit_polling_external_review: missing session_id\n' >&2
        return 1
    fi

    if ! command -v jq >/dev/null 2>&1; then
        printf 'emit_polling_external_review: jq missing\n' >&2
        return 2
    fi

    local data event events_path
    data=$(jq -nc \
        --arg pr "$pr_number" \
        --arg reviewer "$reviewer_bot" \
        --arg wk "$wait_kind" \
        --arg sid "$session_id" \
        --arg nca "$next_check_at" \
        --arg nonce "$nonce" \
        '{
          pr_number: ($pr | tonumber? // $pr),
          reviewer_bot: $reviewer,
          wait_kind: $wk,
          session_id: $sid
        }
        + (if $nca == "" then {} else {next_check_at: $nca} end)
        + (if $nonce == "" then {} else {nonce: $nonce} end)')

    if [[ -z "$data" ]]; then
        printf 'emit_polling_external_review: jq build failed\n' >&2
        return 2
    fi

    event=$(jq -nc \
        --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        --arg src "$source_id" \
        --argjson data "$data" \
        '{ts: $ts, type: "polling_external_review", source: $src, data: $data}')

    # Best-effort schema validation. We source the validator if available
    # and the validator file is found. validate_event returns rc=2 on
    # substrate failure (schema missing) which we treat as a soft pass
    # for callers in environments without yq/python3-yaml; the event still
    # writes. rc=1 is hard reject.
    if declare -F validate_event >/dev/null 2>&1; then
        local _vrc=0
        validate_event polling_external_review "$event" 2>/dev/null || _vrc=$?
        if [[ "$_vrc" == "1" ]]; then
            printf 'emit_polling_external_review: validator rejected event\n' >&2
            return 1
        fi
    fi

    events_path="${EVENTS_FILE:-.fno/events.jsonl}"
    mkdir -p "$(dirname "$events_path")" 2>/dev/null || true
    if ! printf '%s\n' "$event" >> "$events_path" 2>/dev/null; then
        printf 'emit_polling_external_review: append to %s failed\n' "$events_path" >&2
        return 2
    fi
    return 0
}
