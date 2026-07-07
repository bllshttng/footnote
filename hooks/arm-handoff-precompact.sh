#!/usr/bin/env bash
# arm-handoff-precompact.sh - PreCompact guard (c): record handoff intent.
#
# Runs alongside save-session.py on PreCompact. When a live target session is
# past the context-pressure threshold with outstanding work (no <promise> in the
# last assistant turn), it writes an arming marker that PostCompact re-surfaces,
# nudging the agent to run handoff.sh at the NEXT wave boundary.
#
# It NEVER spawns and NEVER archives the manifest or releases the node: claim - a
# compaction can land mid-wave, which handoff.sh forbids. The real handoff stays
# owned by handoff.sh at a safe boundary. Advisory; always exits 0.
set -uo pipefail

STATE_FILE=".fno/target-state.md"
FNO_DIR=".fno"
[[ -f "$STATE_FILE" ]] || exit 0

PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
GUARD_LIB="$PLUGIN_ROOT/scripts/lib/target-guard.sh"
[[ -f "$GUARD_LIB" ]] || exit 0
# shellcheck source=../scripts/lib/target-guard.sh
source "$GUARD_LIB" 2>/dev/null || exit 0

# Owner liveness. The manifest is statusless post-wedge (ab-d0337fbc), so
# target_is_active's status check no longer applies - gate on the live owner pid
# directly (a dead owner means stale state from a prior session; do not arm).
OWNER_PID="$(target_state_field owner_pid "$STATE_FILE" 2>/dev/null || true)"
if [[ "$OWNER_PID" =~ ^[0-9]+$ ]]; then
  kill -0 "$OWNER_PID" 2>/dev/null || exit 0
fi

# Key markers on the MANIFEST session_id so they share handoff.sh's namespace
# (handoff.sh writes/clears .handoff-{done,armed}-<manifest_session_id>).
SESSION_ID="$(target_state_field session_id "$STATE_FILE" 2>/dev/null || true)"
[[ -n "$SESSION_ID" ]] || exit 0
[[ -f "$FNO_DIR/.handoff-done-$SESSION_ID" ]] && exit 0   # handoff already ran
[[ -f "$FNO_DIR/.handoff-armed-$SESSION_ID" ]] && exit 0  # already armed

# transcript_path from stdin (PreCompact payload). Skip the read on a TTY.
TRANSCRIPT=""
if [ ! -t 0 ] && command -v jq >/dev/null 2>&1; then
  TRANSCRIPT="$(cat 2>/dev/null | jq -r '.transcript_path // empty' 2>/dev/null || true)"
fi
# Unreadable transcript -> outstanding-work is UNKNOWN -> decline (no false handoff).
[[ -n "$TRANSCRIPT" && -f "$TRANSCRIPT" ]] || exit 0

# <promise> in the LAST assistant turn -> the session is finishing, do not arm.
# Select the last assistant message FIRST, then scan only its text, so a stale
# <promise> from an earlier turn does not suppress arming when the latest turn
# still has outstanding work.
if command -v jq >/dev/null 2>&1; then
  LAST_ASSISTANT="$(jq -cR 'fromjson? | select(.type=="assistant")' "$TRANSCRIPT" 2>/dev/null | tail -n 1 \
    | jq -r '.message.content[]? | select(.type=="text") | .text' 2>/dev/null || true)"
  printf '%s' "$LAST_ASSISTANT" | grep -q '<promise>' && exit 0
fi

# Context pressure via the sanctioned transcript probe. Any nonzero exit or a
# missing probe -> treat as no pressure -> decline (fail-safe).
PROBE="$PLUGIN_ROOT/skills/target/scripts/context-probe.sh"
[[ -f "$PROBE" ]] || exit 0
PROBE_OUT="$(bash "$PROBE" "$TRANSCRIPT" 2>/dev/null)" || exit 0
USED_PCT="$(printf '%s' "$PROBE_OUT" | jq -r '.used_pct // 0' 2>/dev/null || echo 0)"
[[ "$USED_PCT" =~ ^[0-9]+$ ]] || exit 0

# Arm threshold reuses the handoff trigger (Discretion #2; default 50).
TRIGGER="50"
if command -v fno >/dev/null 2>&1; then
  _t="$(fno config get target.handoff.used_pct_trigger 2>/dev/null || true)"
  [[ "$_t" =~ ^[0-9]+$ ]] && TRIGGER="$_t"
fi
[[ "$USED_PCT" -ge "$TRIGGER" ]] || exit 0

# Arm: record intent only. No spawn, no manifest archive, no claim release.
NODE_ID="$(target_state_field graph_node_id "$STATE_FILE" 2>/dev/null || true)"
TS="$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo '')"
printf '{"node_id":"%s","used_pct":%s,"ts":"%s"}\n' "$NODE_ID" "$USED_PCT" "$TS" \
  > "$FNO_DIR/.handoff-armed-$SESSION_ID" 2>/dev/null || true
exit 0
