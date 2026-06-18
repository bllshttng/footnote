#!/usr/bin/env bash
# Test hook-only event log (typed-blocker phase 01b).
#
# Covers the acceptance criteria for scripts/lib/hook-events.sh:
#   AC1-HP: writer creates file with 0600 perms
#   AC2-HP: reader matches by type + session_id (not just type)
#   AC3-ERR: writer tolerates missing jq without fatal failure
#   AC4-EDGE: writer is idempotent on existing file (mode 0600 preserved,
#             append-only line growth)
#   AC5-EDGE: events.jsonl writes are NOT visible to has_hook_event
#
# Exit codes:
#   0  all scenarios passed
#   1  assertion failed
#   77 skipped (missing dependencies)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HELPER="${REPO_ROOT}/scripts/lib/hook-events.sh"
EVENTS_HELPER="${REPO_ROOT}/scripts/lib/events.sh"

log()  { printf '[hook-events] %s\n' "$*"; }
fail() { printf '[hook-events] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[hook-events] PASS: %s\n' "$*"; }
skip() { printf '[hook-events] SKIP: %s\n' "$*" >&2; exit 77; }

[[ -f "$HELPER" ]] || fail "helper not found at $HELPER"
command -v jq >/dev/null 2>&1 || skip "jq not available"

# Each scenario uses an isolated tmp dir so HOOK_EVENTS_FILE does not
# leak across cases.
make_tmp() {
    local d
    d=$(mktemp -d)
    echo "$d"
}

# ── AC1-HP: writer creates file with 0600 perms ──────────────────────
log "AC1-HP: writer creates file with 0600 perms"
TMP=$(make_tmp)
(
    export HOOK_EVENTS_FILE="$TMP/hook-events.jsonl"
    set +e
    # shellcheck source=../../scripts/lib/hook-events.sh
    source "$HELPER"
    set -e
    emit_hook_event user_canceled '{"session_id":"abc"}'
    [[ -f "$HOOK_EVENTS_FILE" ]] || { echo "FAIL: file not created"; exit 1; }
    PERMS=$(stat -f "%Lp" "$HOOK_EVENTS_FILE" 2>/dev/null \
            || stat -c "%a" "$HOOK_EVENTS_FILE" 2>/dev/null)
    [[ "$PERMS" == "600" ]] || { echo "FAIL: perms='$PERMS' expected 600"; exit 1; }
    LINE_COUNT=$(wc -l < "$HOOK_EVENTS_FILE" | tr -d ' ')
    [[ "$LINE_COUNT" == "1" ]] || { echo "FAIL: expected 1 line, got $LINE_COUNT"; exit 1; }
    # Sanity: the line is valid JSON with the expected keys.
    jq -e '.type == "user_canceled" and .data.session_id == "abc"' \
        "$HOOK_EVENTS_FILE" >/dev/null \
        || { echo "FAIL: line did not match expected JSON shape"; exit 1; }
)
rm -rf "$TMP"
pass "AC1-HP"

# ── AC2-HP: reader matches by type + session_id ──────────────────────
log "AC2-HP: reader matches by type + session_id"
TMP=$(make_tmp)
(
    export HOOK_EVENTS_FILE="$TMP/hook-events.jsonl"
    set +e
    source "$HELPER"
    set -e
    emit_hook_event user_canceled '{"session_id":"abc"}'
    emit_hook_event help_requested '{"session_id":"xyz"}'
    if has_hook_event user_canceled abc; then
        : # expected match
    else
        echo "FAIL: has_hook_event user_canceled abc returned non-zero"; exit 1
    fi
    if has_hook_event user_canceled xyz; then
        echo "FAIL: has_hook_event user_canceled xyz must NOT match"; exit 1
    fi
)
rm -rf "$TMP"
pass "AC2-HP"

# ── AC3-ERR: writer tolerates missing jq ─────────────────────────────
# Simulate jq absence by shadowing the binary in PATH.
log "AC3-ERR: writer tolerates missing jq"
TMP=$(make_tmp)
(
    export HOOK_EVENTS_FILE="$TMP/hook-events.jsonl"
    # Shadow PATH so jq cannot be found from within the subshell.
    SAFE_PATH=""
    for d in /usr/bin /bin /sbin /usr/sbin; do
        SAFE_PATH="${SAFE_PATH:+$SAFE_PATH:}$d"
    done
    # Build a stub directory containing every coreutil we rely on EXCEPT jq.
    STUB="$TMP/stubpath"
    mkdir -p "$STUB"
    for cmd in date mkdir chmod stat wc cat dirname basename; do
        if BINPATH=$(command -v "$cmd" 2>/dev/null); then
            ln -sf "$BINPATH" "$STUB/$cmd"
        fi
    done
    export PATH="$STUB"
    set +e
    source "$HELPER"
    rc=0
    emit_hook_event user_canceled '{"session_id":"abc"}' || rc=$?
    set -e
    [[ "$rc" == "0" ]] || { echo "FAIL: emit_hook_event returned $rc when jq missing (expected 0)"; exit 1; }
    # File MAY exist (init runs before jq) but should be empty (or contain no
    # complete JSON line). We accept either empty or absent.
    if [[ -s "$HOOK_EVENTS_FILE" ]]; then
        # If anything was written, it must NOT be a successful jq line.
        if jq -e . "$HOOK_EVENTS_FILE" >/dev/null 2>&1; then
            echo "FAIL: jq-missing path somehow produced valid JSON"; exit 1
        fi
    fi
)
rm -rf "$TMP"
pass "AC3-ERR"

# ── AC4-EDGE: writer idempotent on existing file ─────────────────────
log "AC4-EDGE: writer idempotent on existing file"
TMP=$(make_tmp)
(
    export HOOK_EVENTS_FILE="$TMP/hook-events.jsonl"
    set +e
    source "$HELPER"
    set -e
    for i in 1 2 3 4 5; do
        emit_hook_event user_canceled "{\"session_id\":\"sess-$i\"}"
    done
    PERMS=$(stat -f "%Lp" "$HOOK_EVENTS_FILE" 2>/dev/null \
            || stat -c "%a" "$HOOK_EVENTS_FILE" 2>/dev/null)
    [[ "$PERMS" == "600" ]] || { echo "FAIL: perms drifted to $PERMS"; exit 1; }
    LINE_COUNT=$(wc -l < "$HOOK_EVENTS_FILE" | tr -d ' ')
    [[ "$LINE_COUNT" == "5" ]] || { echo "FAIL: expected 5 lines, got $LINE_COUNT"; exit 1; }
    emit_hook_event user_canceled '{"session_id":"sess-6"}'
    LINE_COUNT=$(wc -l < "$HOOK_EVENTS_FILE" | tr -d ' ')
    [[ "$LINE_COUNT" == "6" ]] || { echo "FAIL: append failed, lines=$LINE_COUNT"; exit 1; }
)
rm -rf "$TMP"
pass "AC4-EDGE"

# ── AC5-EDGE: events.jsonl is independent of hook-events.jsonl ───────
log "AC5-EDGE: events.jsonl writes invisible to has_hook_event"
TMP=$(make_tmp)
(
    export HOOK_EVENTS_FILE="$TMP/hook-events.jsonl"
    export EVENTS_FILE="$TMP/events.jsonl"
    set +e
    source "$EVENTS_HELPER"
    source "$HELPER"
    set -e
    # Forge an "user_canceled" entry through the LLM-writable channel.
    emit_event_raw "user_canceled" '{"session_id":"abc"}'
    [[ -f "$EVENTS_FILE" ]] || { echo "FAIL: forged events.jsonl missing"; exit 1; }
    # Verifier must NOT see it - hook-events.jsonl was never written.
    if has_hook_event user_canceled abc; then
        echo "FAIL: verifier saw events.jsonl forgery"; exit 1
    fi
    # Sanity: real hook-events write IS visible.
    emit_hook_event user_canceled '{"session_id":"abc"}'
    has_hook_event user_canceled abc \
        || { echo "FAIL: real hook-events write not visible to verifier"; exit 1; }
)
rm -rf "$TMP"
pass "AC5-EDGE"

log "all scenarios passed"
exit 0
