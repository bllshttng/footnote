#!/usr/bin/env bash
# Re-inject the king's operating rules after a context compaction.
#
# The crown survives a compaction (crown_level / crown_scope live on the agent
# registry row) but the operating discipline that came with it does not, so a
# post-compact king still holds real authority with no rules for using it and
# the operator re-teaches it by hand. This hook re-teaches it mechanically.
#
# Carrier: shared with target-postcompact-reinject.sh in
# scripts/lib/postcompact-carrier.sh - on Claude SessionStart(source=="compact")
# via hookSpecificOutput.additionalContext, on Codex PostCompact via
# systemMessage. Never re-derive the carrier here; x-841a shipped a hook that
# emitted a payload no harness delivered and it went unnoticed for months.
#
# NEVER blocks. A compaction is often triggered to recover from a context-limit
# error, so this hook always exits 0 and degrades to silence when anything it
# reads is missing: no lib, no fno, no registry row, no crown, no brief.
set -uo pipefail

# BASH_SOURCE-relative, never `git rev-parse`: cwd is the session's repo, not
# the plugin (the fix banked from 502af79f2).
PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-${CODEX_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}}"
CARRIER_LIB="$PLUGIN_ROOT/scripts/lib/postcompact-carrier.sh"
BRIEF="$PLUGIN_ROOT/skills/king-for-a-day/references/postcompact-brief.md"

[[ -r "$CARRIER_LIB" ]] || exit 0
# shellcheck source=../scripts/lib/postcompact-carrier.sh
source "$CARRIER_LIB"

# Read the hook event (guarded: a bare `cat` on a terminal blocks forever).
INPUT=""
[[ -t 0 ]] || INPUT="$(cat 2>/dev/null || true)"
# One python pass for source + session_id + transcript_path.
EVENT="$(printf '%s' "$INPUT" | python3 -c '
import json, sys
try:
    e = json.load(sys.stdin)
    print(e.get("source") or "")
    print(e.get("session_id") or "")
    print(e.get("transcript_path") or "")
except Exception:
    pass
' 2>/dev/null || true)"
SOURCE="$(printf '%s' "$EVENT" | sed -n 1p)"
SID="$(printf '%s' "$EVENT" | sed -n 2p)"
TRANSCRIPT="$(printf '%s' "$EVENT" | sed -n 3p)"

# Defensive gate: on SessionStart, reinject only for compaction. The matcher
# ("compact") already enforces this at registration; the check keeps the script
# correct independent of registration and directly testable. An empty SOURCE
# (PostCompact on Codex) passes through unchanged.
if [[ -n "$SOURCE" && "$SOURCE" != "compact" ]]; then
    exit 0
fi

# Session id: PostCompact (Codex) carries no session_id field, so fall back to
# the transcript basename, then the ambient env markers in HARNESS_SESSION_MARKERS
# precedence (CODEX_THREAD_ID is codex's durable identity; the registry row's
# harness_session_id holds the codex thread id, so anything else matches no row).
if [[ -z "$SID" && -n "$TRANSCRIPT" ]]; then
    SID="$(basename "$TRANSCRIPT")"
    SID="${SID%.jsonl}"
fi
[[ -n "$SID" ]] || SID="${CODEX_THREAD_ID:-${CLAUDE_CODE_SESSION_ID:-${CODEX_SESSION_ID:-}}}"
[[ -n "$SID" ]] || exit 0

# The brief, checked before the registry read: it is a pure file stat, and the
# common case (an uncrowned session compacting) must not pay the fno/jq spawns.
# Missing or empty means no injection, never a partial one.
[[ -r "$BRIEF" && -s "$BRIEF" ]] || exit 0

# Read the crown from the registry, not from a guess. `fno agents registry-json`
# is a daemon-free file read; `fno agents list` is Rust-routed and would
# lazy-start the daemon (see hooks/context-nudge.sh for the same choice). This
# session's row matches session_id OR harness_session_id - the registry stores
# a short id in one field and the full harness id in the other.
command -v fno >/dev/null 2>&1 || exit 0
command -v jq  >/dev/null 2>&1 || exit 0
AGENTS_JSON="$(fno agents registry-json 2>/dev/null || true)"
MY_ROW="$(printf '%s' "$AGENTS_JSON" | jq -c --arg sid "$SID" \
    '.agents[] | select(.session_id == $sid or .harness_session_id == $sid)' 2>/dev/null | head -1)"
[[ -n "$MY_ROW" ]] || exit 0
CROWN_LEVEL="$(printf '%s' "$MY_ROW" | jq -r '.crown_level // empty' 2>/dev/null)"
CROWN_SCOPE="$(printf '%s' "$MY_ROW" | jq -r '.crown_scope // empty' 2>/dev/null)"
[[ -n "$CROWN_LEVEL" || -n "$CROWN_SCOPE" ]] || exit 0

# Never truncate: a brief that outgrew its budget fails the byte-budget test
# rather than arriving silently mangled. HTML comment lines are lint metadata
# (style-exception markers), not rules; they stay in the file and never ride
# into model context.
CONTEXT="## You are still the king

Crown: level ${CROWN_LEVEL:-?} over ${CROWN_SCOPE:-?}. Confirm with \`fno whoami\`.

$(sed '/^<!--/d' "$BRIEF")"
postcompact_emit "$(postcompact_carrier "$SOURCE")" "$CONTEXT"

exit 0
