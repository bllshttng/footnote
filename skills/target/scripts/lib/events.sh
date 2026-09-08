#!/usr/bin/env bash
# Event log library for target observability
# Appends structured JSONL events to .fno/events.jsonl
# Usage: source this file, then call emit_event

_events_lib_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=events-lock.sh
source "$_events_lib_dir/events-lock.sh" || return 1 2>/dev/null || exit 1
unset _events_lib_dir

if [[ -z "${EVENTS_FILE:-}" ]]; then
    # FNO_EVENTS_PATH is the journal pin, the same var paths.project_events_json()
    # reads on the Python side. It stands in for a repo root nobody can sandbox:
    # fno.hermetic.neutralise leaves FNO_REPO_ROOT unset on purpose, so without
    # the pin a shell test running inside the checkout appends a real row to the
    # journal the operator needs panel folds. An explicit EVENTS_FILE still wins,
    # exactly as an explicit events_path= wins in Python.
    if [[ -n "${FNO_EVENTS_PATH:-}" ]]; then
        EVENTS_FILE="$FNO_EVENTS_PATH"
    else
        # Resolver first: the project journal lives in the space, and a raw
        # checkout path is retired once a repo migrates. The Rust CLI answers
        # the same path every Rust reader uses; the checkout file remains the
        # degrade for contexts with no fno-agents on PATH (its setup symlink,
        # when one was linked, still follows to the canonical journal).
        _events_repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || _events_repo_root="$PWD"
        EVENTS_FILE=""
        if command -v fno-agents >/dev/null 2>&1; then
            EVENTS_FILE=$(cd "$_events_repo_root" 2>/dev/null && fno-agents state path events 2>/dev/null)
        fi
        if [[ -z "$EVENTS_FILE" ]]; then
            EVENTS_FILE="$_events_repo_root/.fno/events.jsonl"
        fi
        unset _events_repo_root
    fi
fi

# Resolve only an existing symlink leaf. Already-running shells retain the old
# path until their next setup migration, while newly sourced writers share the
# canonical GC marker and mutex neighbourhood immediately.
if [[ -L "$EVENTS_FILE" ]]; then
    EVENTS_FILE=$(_resolve_event_symlink "$EVENTS_FILE") || return 1 2>/dev/null || exit 1
fi
EVENTS_ATOMIC_LINE_MAX_BYTES=4000

_wait_for_event_gc() {
    local events_path="${1:?events path required}"
    local marker="${events_path}.gc.d"
    local attempts=0
    local max_attempts="${EVENTS_GC_WAIT_ATTEMPTS:-20}"
    [[ "$max_attempts" =~ ^[0-9]+$ ]] || max_attempts=20
    while [[ -d "$marker" && "$attempts" -lt "$max_attempts" ]]; do
        if _steal_stale_event_dir "$marker"; then
            continue
        fi
        sleep 0.05
        attempts=$((attempts + 1))
    done
    [[ ! -d "$marker" ]]
}

_begin_shell_event_append() {
    local events_path="${1:?events path required}"
    local writer_pid="${2:?writer pid required}"
    local active_dir="${events_path}.shell-writers.d"
    local token identity
    while true; do
        _wait_for_event_gc "$events_path" || return 1
        mkdir -p "$active_dir" 2>/dev/null || return 1
        token="$active_dir/${writer_pid}.${RANDOM}"
        if ! mkdir "$token" 2>/dev/null; then
            continue
        fi
        identity=$(_event_process_identity "$writer_pid")
        if [[ -z "$identity" ]] || ! printf '%s' "$identity" > "$token/owner"; then
            command -p rm -f "$token/owner" 2>/dev/null || true
            rmdir "$token" 2>/dev/null || true
            return 1
        fi
        if [[ ! -d "${events_path}.gc.d" ]]; then
            printf '%s' "$token"
            return 0
        fi
        _end_shell_event_append "$token"
    done
}

_end_shell_event_append() {
    local token="${1:?writer token required}"
    command -p rm -f "$token/owner" 2>/dev/null || true
    rmdir "$token" 2>/dev/null || true
    rmdir "$(dirname "$token")" 2>/dev/null || true
}

# Roots a hermetic shell process may write a journal into. The Python half is
# fno.events._hermetic_allowed_roots; keep the two in step. TMPDIR covers both
# the neutralise sandbox and a test's own mktemp file, and the parent of an
# ABSOLUTE FNO_EVENTS_PATH covers a caller that declared its own journal. A
# relative pin is not a root, exactly as on the Python side.
_shell_hermetic_roots() {
    local tmp="${TMPDIR:-/tmp}"
    printf '%s\n' "${tmp%/}"
    printf '%s\n' "$(_shell_physical_dir "${tmp%/}")"
    if [[ "${FNO_EVENTS_PATH:-}" == /* ]]; then
        local pin_dir="${FNO_EVENTS_PATH%/*}"
        printf '%s\n' "$pin_dir"
        # Physical, not `cd`: the pin's own parent is often the directory the
        # first append creates. Judging the write physically and the root
        # lexically makes the two disagree, and the appender then refuses the
        # very journal FNO_EVENTS_PATH names.
        printf '%s\n' "$(_shell_physical_dir "$pin_dir")"
    fi
}

# Physical form of a directory that may not exist yet.
#
# `cd` reaches only an existing directory, and the journal's parent is often
# the very directory the append is about to create. Falling back to the raw
# string there leaves a symlinked ANCESTOR unresolved, and a sandbox path
# pointing at a live tree then reads as inside the sandbox. So walk up to the
# deepest existing ancestor, resolve THAT, and re-append the missing tail. The
# Python half gets this from `os.path.realpath`, which resolves a nonexistent
# path already.
_shell_physical_dir() {
    local dir="${1:?dir required}"
    local tail="" real
    while [[ ! -d "$dir" && "$dir" == */* ]]; do
        tail="/${dir##*/}$tail"
        dir="${dir%/*}"
        [[ -n "$dir" ]] || dir="/"
    done
    real=$(cd "$dir" 2>/dev/null && pwd -P) || real="$dir"
    printf '%s%s' "${real%/}" "$tail"
}

# Refuse a journal write that would leave a test's sandbox.
#
# The Python appender has refused this since the events fence shipped; the
# shell appender did not, and three hooks hand it a `${REPO_ROOT}/.fno/...`
# path they build themselves, so no pin can reach them. In a worktree that
# file is a symlink to the canonical journal, which is how 33 fixture rows
# reached the operator's live question queue on 2026-08-20. Judged on the
# physical directory, because the symlink is the mechanism.
_refuse_shell_hermetic_escape() {
    local events_path="${1:?events path required}"
    local label="${2:-events}"
    [[ "${FNO_TEST_HERMETIC:-}" == "1" ]] || return 0
    local real root
    real=$(_shell_physical_dir "${events_path%/*}")
    while IFS= read -r root; do
        [[ -n "$root" ]] || continue
        if [[ "$real" == "$root" || "$real" == "$root"/* ]]; then
            return 0
        fi
    done < <(_shell_hermetic_roots)
    printf '%s: refused a journal write outside the test sandbox: %s\n' \
        "$label" "$events_path" >&2
    printf 'A hermetic run must not touch a live events.jsonl. Pass a path under TMPDIR, or set FNO_EVENTS_PATH.\n' >&2
    return 1
}

# Whether this append may create the directory it writes into.
#
# A project `.fno/` marks a project that opted in. Creating one as a side
# effect of an event append put a `.fno/` and an events.jsonl in every repo a
# hook touched, whether or not that project ever used footnote.
#
# The rule names the directory, not a state root. `STATE_DIR` cannot be read
# here: `scripts/lib/paths.sh` exports it as the machine-wide root, and
# `hooks/capture-plan-mode.sh` sets it to `<repo>/.fno`, so the second meaning
# turns this guard off in exactly the repos it protects. `~/.fno` is named as
# itself, and the spaces under it have parents that are not `.fno`, so both
# stay creatable.
_shell_events_may_create_parent() {
    local events_path="${1:?events path required}"
    local parent="${events_path%/*}"
    [[ -d "$parent" ]] && return 0
    # FNO_HOME is the state-root override the rest of the hook tree reads as
    # `${FNO_HOME:-$HOME/.fno}`. Compared physically as well as by spelling,
    # so a symlinked $HOME does not make the same root pass one way and fail
    # the other.
    local state_root="${FNO_HOME:-${HOME%/}/.fno}"
    state_root="${state_root%/}"
    [[ "$parent" == "$state_root" ]] && return 0
    [[ "$(_shell_physical_dir "$parent")" == "$(_shell_physical_dir "$state_root")" ]] && return 0
    [[ "${parent##*/}" == ".fno" ]] && return 1
    return 0
}

_append_bounded_event() {
    local label="${1:?label required}"
    local event="${2:?event required}"
    local requested_path="${3:?events path required}"
    local events_path current_path writer_token
    local event_bytes
    event_bytes=$(printf '%s\n' "$event" | wc -c | tr -d '[:space:]')
    if (( event_bytes > EVENTS_ATOMIC_LINE_MAX_BYTES )); then
        printf '%s: serialized event is %s bytes and exceeds the %s-byte append cap\n' \
            "$label" "$event_bytes" "$EVENTS_ATOMIC_LINE_MAX_BYTES" >&2
        return 1
    fi
    local writer_pid="${BASHPID:-$$}"
    while true; do
        events_path="$requested_path"
        if [[ -L "$events_path" ]]; then
            events_path=$(_resolve_event_symlink "$events_path") || return 1
        fi
        # Both guards run BEFORE the mutex: _begin_shell_event_append mkdir -p's
        # a writer dir beside the journal, so acquiring the lock itself creates
        # the `.fno/` this refuses to create, in the file this refuses to touch.
        #
        # A skipped append returns 3, never 0. A caller reading 0 as "the line
        # is on disk" is the shape this whole guard exists to refuse, and the
        # thrash detector counts appended lines. Silent, because guard-mark
        # fires per tool call and a note per call is noise in exactly the repos
        # this leaves alone.
        _refuse_shell_hermetic_escape "$events_path" "$label" || return 1
        _shell_events_may_create_parent "$events_path" || return 3
        writer_token=$(_begin_shell_event_append "$events_path" "$writer_pid") || return 1
        current_path="$requested_path"
        if [[ -L "$current_path" ]]; then
            current_path=$(_resolve_event_symlink "$current_path") || {
                _end_shell_event_append "$writer_token"
                return 1
            }
        fi
        if [[ "$current_path" == "$events_path" ]]; then
            break
        fi
        _end_shell_event_append "$writer_token"
    done
    if ! mkdir -p "$(dirname "$events_path")" 2>/dev/null; then
        _end_shell_event_append "$writer_token"
        return 1
    fi
    local append_rc=0
    printf '%s\n' "$event" >> "$events_path" 2>/dev/null || append_rc=$?
    _end_shell_event_append "$writer_token"
    return "$append_rc"
}

emit_event() {
    local source="${1:?source required}"
    local type="${2:?type required}"
    local data
    data="${3}"
    [[ -z "$data" ]] && data='{}'

    # Use jq for safe JSON construction (handles special chars)
    local event
    event=$(jq -nc \
        --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        --arg src "$source" \
        --arg type "$type" \
        --argjson data "$data" \
        '{timestamp: $ts, source: $src, type: $type, data: $data}' 2>/dev/null) || return 0
    _append_bounded_event emit_event "$event" "$EVENTS_FILE" || true
}

# emit_event_raw TYPE JSON [SOURCE]
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
    local source="${3:-}"
    [[ -z "$json" ]] && json='{}'
    local events_path="${EVENTS_FILE:-.fno/events.jsonl}"
    local event
    event=$(jq -nc \
        --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        --arg type "$type" \
        --arg source "$source" \
        --argjson data "$json" \
        '{ts: $ts, type: $type, data: $data}
        + (if $source == "" then {} else {source: $source} end)' 2>/dev/null) || return 0
    _append_bounded_event emit_event_raw "$event" "$events_path" || true
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
# rc=3  skipped: the project journal's `.fno/` does not exist, so this repo
#       never opted in. Nothing is wrong; nothing was written either.
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
    local append_rc=0
    _append_bounded_event emit_polling_external_review "$event" "$events_path" || append_rc=$?
    # A deliberate skip is not a broken substrate. Telling a caller its jq is
    # missing when the repo simply never opted in sends it to fix the wrong
    # thing.
    if (( append_rc == 3 )); then
        return 3
    fi
    if (( append_rc != 0 )); then
        printf 'emit_polling_external_review: append to %s failed\n' "$events_path" >&2
        return 2
    fi
    return 0
}
