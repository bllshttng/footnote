#!/usr/bin/env bash
# postcompact-carrier.sh - shared post-compaction delivery for the reinject hooks.
#
# Sourced (never executed) by hooks/target-postcompact-reinject.sh and
# hooks/king-postcompact-reinject.sh. No side effects at source time.
#
# The carrier rule this lib encodes (learned the hard way in x-841a, hardened in
# 502af79f2): on Claude, SessionStart with source=="compact" is the only
# post-compaction event that injects into MODEL context, and it delivers through
# hookSpecificOutput.additionalContext; PostCompact output on Claude reaches
# stderr and the user only. On Codex the PostCompact hook delivers through
# systemMessage. The carrier is chosen from the HARNESS, never from the event
# payload, so a lost or empty stdin cannot silently downgrade Claude to a
# carrier the model never sees.

# postcompact_carrier - echo the payload key this harness delivers through.
# Keyed on the HARNESS, never on the event payload: the observer wrapper writes
# an empty input file when its own `cat` fails, and an empty SOURCE would fall
# through to systemMessage on claude, which reaches the user but never the model.
# FNO_PLATFORM is authoritative when set (codex-hooks.json sets it to "codex"),
# so it is checked before the ambient CLAUDE_PLUGIN_ROOT, which a nested session
# can leak into a codex environment.
postcompact_carrier() {  # $1 = SOURCE field from the event ("" on codex)
    if [[ -n "${FNO_PLATFORM:-}" ]]; then
        [[ "$FNO_PLATFORM" == "claude" ]] && { echo additionalContext; return; }
        echo systemMessage; return
    fi
    if [[ -n "${CLAUDE_PLUGIN_ROOT:-}" || "${1:-}" == "compact" ]]; then
        echo additionalContext; return
    fi
    echo systemMessage
}

# postcompact_emit CARRIER CONTEXT - print the harness's delivery payload.
postcompact_emit() {
    python3 -c "
import json, sys
carrier, context = sys.argv[1:3]
if carrier == 'additionalContext':
    payload = {'hookSpecificOutput': {'hookEventName': 'SessionStart', 'additionalContext': context}}
else:
    payload = {'systemMessage': context}
print(json.dumps(payload))
" "$1" "$2" 2>/dev/null
}

# postcompact_read_event - read the hook event from stdin (TTY-guarded: a bare
# `cat` on a terminal blocks forever when run by hand) and echo three lines:
# source, session_id, transcript_path ("" each when absent).
postcompact_read_event() {
    local input=""
    [[ -t 0 ]] || input="$(cat 2>/dev/null || true)"
    printf '%s' "$input" | python3 -c '
import json, sys
try:
    e = json.load(sys.stdin)
except Exception:
    e = {}
for k in ("source", "session_id", "transcript_path"):
    print(e.get(k) or "")
' 2>/dev/null
}

# postcompact_resolve_sid EVENT_SID TRANSCRIPT - echo this session's id.
# $1: session_id from the event (claude carries it; codex PostCompact does not).
# $2: transcript_path from the event (its basename is the claude session id).
# Env fallbacks follow HARNESS_SESSION_MARKERS precedence: CODEX_THREAD_ID is
# codex's durable identity and the value the registry row's harness_session_id
# holds, so any other codex env id matches no row. One home for the chain, so
# the next marker change lands once instead of per hook.
postcompact_resolve_sid() {
    local sid="${1:-}" transcript="${2:-}"
    if [[ -z "$sid" && -n "$transcript" ]]; then
        sid="$(basename "$transcript")"
        sid="${sid%.jsonl}"
    fi
    [[ -n "$sid" ]] || sid="${CODEX_THREAD_ID:-${CLAUDE_CODE_SESSION_ID:-${CODEX_SESSION_ID:-}}}"
    printf '%s' "$sid"
}
