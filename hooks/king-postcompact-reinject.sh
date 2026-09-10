#!/usr/bin/env bash
# Re-inject the king's operating rules after a context compaction.
#
# The crown survives a compaction (crown_level / crown_scope live on the agent
# registry row) but the operating discipline that came with it does not, so a
# post-compact king still holds real authority with no rules for using it and
# the operator re-teaches it by hand. This hook re-teaches it mechanically. It
# also reads back the crown's precompact canon handoff doc: the judgment halves
# it holds survive the compact in no other channel.
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
SOURCE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ "${FNO_PLATFORM:-}" == "codex" ]]; then
    PLUGIN_ROOT="${CODEX_PLUGIN_ROOT:-${PLUGIN_ROOT:-$SOURCE_ROOT}}"
else
    PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-${CODEX_PLUGIN_ROOT:-$SOURCE_ROOT}}"
fi
CARRIER_LIB="$PLUGIN_ROOT/scripts/lib/postcompact-carrier.sh"
BRIEF="$PLUGIN_ROOT/skills/king-for-a-day/references/postcompact-brief.md"

[[ -r "$CARRIER_LIB" ]] || exit 0
# shellcheck source=../scripts/lib/postcompact-carrier.sh
source "$CARRIER_LIB"

# Read the hook event through the shared lib (TTY-guarded; one python pass).
EVENT="$(postcompact_read_event)"
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

# Session id through the shared resolver: event field, then transcript
# basename, then the env markers in HARNESS_SESSION_MARKERS precedence.
SID="$(postcompact_resolve_sid "$SID" "$TRANSCRIPT")"
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

# This crown's own FAQ, appended after the static brief: bounded on both axes
# (the verb caps entry COUNT; FAQ_MAX_BYTES below caps the aggregate payload,
# since one detailed entry - or several - can still outgrow context on their
# own) and degrade-safe (silence on a missing directory, an unreadable one, or
# no entry for this scope - never a failed hook).
FAQ_MAX_BYTES=4000
if [[ -n "$CROWN_SCOPE" ]]; then
    FAQ_ENTRIES="$(fno agents king faq list --scope "$CROWN_SCOPE" 2>/dev/null || true)"
    FAQ_BYTES="$(printf '%s' "$FAQ_ENTRIES" | wc -c | tr -d ' ')"
    if [[ "$FAQ_BYTES" -gt "$FAQ_MAX_BYTES" ]]; then
        # A raw `head -c` cut can split a multi-byte UTF-8 character mid-
        # sequence, landing an invalid byte in the payload. Decode first and
        # drop the trailing partial character instead of splitting it.
        FAQ_ENTRIES="$(printf '%s' "$FAQ_ENTRIES" | python3 -c "
import sys
sys.stdout.write(sys.stdin.buffer.read(${FAQ_MAX_BYTES}).decode('utf-8', errors='ignore'))
")

_(truncated at ${FAQ_MAX_BYTES}B; \`fno agents king faq list --scope \"${CROWN_SCOPE}\"\` has the rest)_"
    fi
    if [[ -n "$(printf '%s' "$FAQ_ENTRIES" | tr -d '[:space:]')" ]]; then
        CONTEXT="$CONTEXT

## This crown's FAQ

$FAQ_ENTRIES"
    fi
fi

# Canon read-back: the PreCompact hook wrote this crown's rolling handoff doc,
# and the judgment halves it holds (merge order, open decisions, gaps,
# workarounds) exist in no other channel - the session that wrote them is
# gone after the compact, and a successor resolves the SAME scope-keyed doc.
# Only the session-filled blocks ride back: the mechanical halves are stale by
# definition (they describe the pre-compact world), and an unfilled heading
# still carries its default placeholder, recognizable by its signature text.
# Bounded like the FAQ (byte cap) and degrade-safe: no resolvable doc, no doc
# on disk, or nothing filled means no section, never a failed hook.
CANON_MAX_BYTES=4000
if [[ -n "$CROWN_SCOPE" ]]; then
    CANON_PATH="$(fno config paths handoff --scope "$CROWN_SCOPE" 2>/dev/null || true)"
    if [[ -n "$CANON_PATH" && -f "$CANON_PATH" ]]; then
        CANON_SECTIONS="$(awk '
            /^## / { heading = $0; next }
            /<!-- fno:session -->/ { grab = 1; buf = ""; next }
            grab && /<!-- \/fno:session -->/ {
                grab = 0
                if (buf ~ /[^[:space:]]/ && buf !~ /Nothing external knows this/) {
                    print ""
                    print heading
                    printf "%s", buf
                }
                next
            }
            grab { buf = buf $0 "\n" }
        ' "$CANON_PATH" 2>/dev/null)"
        CANON_BYTES="$(printf '%s' "$CANON_SECTIONS" | wc -c | tr -d ' ')"
        if [[ "$CANON_BYTES" -gt "$CANON_MAX_BYTES" ]]; then
            # Same UTF-8-safe cut as the FAQ: decode, drop the trailing
            # partial character, never split one mid-sequence.
            CANON_SECTIONS="$(printf '%s' "$CANON_SECTIONS" | python3 -c "
import sys
sys.stdout.write(sys.stdin.buffer.read(${CANON_MAX_BYTES}).decode('utf-8', errors='ignore'))
")

_(truncated at ${CANON_MAX_BYTES}B; \`${CANON_PATH}\` has the rest)_"
        fi
        if [[ -n "$(printf '%s' "$CANON_SECTIONS" | tr -d '[:space:]')" ]]; then
            CONTEXT="$CONTEXT

## Your crown's handoff (written before the compact)

$CANON_SECTIONS

Full canon doc: \`${CANON_PATH}\`."
        fi
    fi
fi

# Reign limb (x-7b36): when the crowned scope's manifest reports a shape AND
# names THIS session, this is a tenured reign, and its beat needs re-teaching
# after a compact. Reads the same manifest every king arm resolves; a missing
# manifest or a foreign session id means the king-for-a-day brief above is the
# whole teaching, so nothing is appended (fail to the narrower rule).
REPO_ROOT="$(git -C "$PWD" rev-parse --show-toplevel 2>/dev/null || echo "$PWD")"
REIGN_MANIFEST="$(fno agents king manifest-path --harness-session-id "$SID" \
    --state-root "$REPO_ROOT/.fno" 2>/dev/null || true)"
if [[ -n "$REIGN_MANIFEST" && -f "$REIGN_MANIFEST" ]]; then
    REIGN_SHAPE="$(sed -n 's/^shape:[[:space:]]*//p' "$REIGN_MANIFEST" | head -1 | tr -d '[:space:]')"
    REIGN_SID="$(sed -n 's/^harness_session_id:[[:space:]]*//p' "$REIGN_MANIFEST" | head -1 | tr -d '[:space:]')"
    if [[ -n "$REIGN_SHAPE" && "$REIGN_SID" == "$SID" ]]; then
        CONTEXT="$CONTEXT

## You are still reigning (shape: ${REIGN_SHAPE})

The loop, goal and monitors survive a compact: verify with \`/hooks\` and the loop receipt, and re-arm any that is missing. The six monitors: unread mail (60s), board-change proxy (120s), crown liveness (300s), main-branch CI (300s), capacity band debounced across two samples (300s), arm staleness (600s). The two self-injected commands: \`/loop <king.checkin_interval> <king.checkin_text>\` and \`/goal <king.goal_text>\`. Levers in order: mail the stalled worker, \`fno backlog encounter <id> --evidence\` and \`fno backlog update <id> --priority\` (rank is the operator's pin and refuses you), undefer or supersede, ask the operator. Journal \`reign_checkin\`; dispatch only on a red dispatching arm, journaled as \`reign_dispatch_exception\`. Never \`/goal clear\` on NoProgress - escalate-and-park is the stop path."
    fi
fi
postcompact_emit "$(postcompact_carrier "$SOURCE")" "$CONTEXT"

exit 0
