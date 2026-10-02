#!/usr/bin/env bash
# hooks/inside-leg-report.sh - REMOVED NEXT RELEASE.
#
# Replaced by hooks/session-state.sh plus the `fno-agents hook session-state`
# entry (one Rust producer, per-harness adapters, the event map as data in
# harness_capabilities.toml). This stub keeps the old state-word vocabulary
# working for one release for anything still wired to this path: it reads the
# event name off the payload (the state word it was called with matches the
# event's map entry on every wired line) and execs the shim.
set -u
PATH="${PATH:+$PATH:}/usr/bin:/bin"
export PATH
STATE="${1:-working}"
case "$STATE" in
  working | blocked | done | model) ;;
  *) STATE="working" ;;
esac
INPUT="$(cat 2>/dev/null)" || INPUT=""
EVENT="$(printf '%s' "$INPUT" | /usr/bin/python3 -c '
import sys, json
try:
    d = json.load(sys.stdin)
    print((d.get("hook_event_name") or "") if isinstance(d, dict) else "")
except Exception:
    print("")
' 2>/dev/null)"
case "$EVENT" in
  Notification|PreToolUse|UserPromptSubmit|Stop|PostModelSwitch) ;;
  *) EVENT="UserPromptSubmit" ;;
esac
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
printf '%s' "$INPUT" | "$HOOK_DIR/session-state.sh" claude "$EVENT"
exit 0
