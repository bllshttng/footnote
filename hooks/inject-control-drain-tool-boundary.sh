#!/usr/bin/env bash
# fno hook: PreToolUse - control mail drain at the tool boundary.
#
# A control body that demoted durable waits on the turn-boundary mail push,
# which fires only at a prompt boundary; a worker holding one long turn never
# reaches one, so a merge freeze could not stop it. This hook lands CONTROL
# mail at the next tool call instead. The shell layer is a cheap gate only:
# three stat calls
# against the sender-stamped pending flags, then the fno-agents verb owns
# scan, cursors, dedup, and rendering. A config-redirected bus degrades to
# prompt-boundary delivery, never a loss.

set -uo pipefail

# Survive a caller env with no usable PATH (see worktree-write-protect.sh).
PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

command -v jq >/dev/null 2>&1 || exit 0

SID="$(jq -r '.session_id // empty' 2>/dev/null)"
[[ "$SID" =~ ^[0-9a-f-]{16,}$ ]] || exit 0

BUS="${FNO_STATE_DIR:-$HOME/.fno}/bus"
FIRST8="${SID:0:8}"
LAST8="${SID: -8}"
if [[ ! -f "$BUS/control-pending/$FIRST8.flag" && ! -f "$BUS/control-pending/$SID.flag" && ! -f "$BUS/control-pending/$LAST8.flag" ]]; then
  exit 0
fi

command -v fno-agents >/dev/null 2>&1 || exit 0

# Stdout of the verb IS the hook payload: it streams through fd 3 untouched,
# byte-for-byte (same discipline as inject-mail-notify.sh). A miss never
# blocks the tool call it rode in on.
exec 3>&1
drain_err="$(fno-agents mail-inject --control-drain --bus-dir "$BUS" --session "$SID" 2>&1 1>&3 3>&-)"
exec 3>&-
exit 0
