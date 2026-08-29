#!/usr/bin/env bash
# guard-mark.sh - sourced by PreToolUse guards; provides _guard_mark, the
# positive liveness signal that a guard actually ran and what it decided
# (x-04bc). One guard_decision event row per guard invocation, appended to
# the project events log: without it, a guard that cannot prove it ran is
# indistinguishable from one that never launched, and under
# permissions.defaultMode = dontAsk the guards are the entire safety layer.
# Best-effort by contract: every failure is swallowed and can never change
# a decision.

_GUARD_MARK_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# Resolve the events file without forking git when the cwd is already the
# project root (the common hook case): two stats instead of a rev-parse.
# events.sh honors a pre-set EVENTS_FILE and skips its own resolution,
# including the git fork. An explicit pin still wins.
if [[ -z "${EVENTS_FILE:-}" && -z "${FNO_EVENTS_PATH:-}" ]] \
    && { [[ -d .git ]] || [[ -d .fno ]]; }; then
    EVENTS_FILE="$PWD/.fno/events.jsonl"
fi
# Reuse the events transport (path resolution + GC-safe append) when the
# caller has not sourced it already.
# shellcheck source=../../scripts/lib/events.sh
declare -F _append_bounded_event >/dev/null 2>&1 \
    || source "${_GUARD_MARK_ROOT}/scripts/lib/events.sh" 2>/dev/null || true
unset _GUARD_MARK_ROOT

# _guard_mark <guard-name> <decision>
#
# Row shape matches emit_event_raw output so bash and python guards write
# indistinguishable rows. Built with printf, not jq: every field is fixed
# vocabulary (guard name, allow|block, a tool name captured from the raw
# payload), so the fast allow paths do not pay a jq spawn per tool call.
_guard_mark() {
    declare -F _append_bounded_event >/dev/null 2>&1 || return 0
    local tool="${GUARD_TOOL:-}"
    if [[ -z "$tool" && -n "${PAYLOAD:-}" ]] \
        && [[ "$PAYLOAD" =~ \"tool_name\":[[:space:]]*\"([A-Za-z|]+)\" ]]; then
        tool="${BASH_REMATCH[1]}"
    fi
    local row
    printf -v row \
        '{"ts":"%s","type":"guard_decision","data":{"guard":"%s","decision":"%s","tool":"%s"},"source":"hook"}' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$1" "$2" "${tool:-unknown}"
    _append_bounded_event "guard_mark" "$row" "${EVENTS_FILE:-.fno/events.jsonl}" 2>/dev/null || true
}
