#!/usr/bin/env bash
# fno hook: PreToolUse - control mail drain at the tool boundary.
#
# A control body that demoted durable waits on notify-self, which fires only
# at a prompt boundary; a worker holding one long turn never reaches one, so
# a merge freeze could not stop it. This hook lands CONTROL mail at the next
# tool call instead. The shell layer is a cheap gate only: three stat calls
# against the sender-stamped pending flags, then the hidden verb owns
# rendering, cursors, and acknowledgement. The bus-dir guess covers the
# default install; a config-redirected bus simply degrades to
# prompt-boundary delivery, never a loss.

set -uo pipefail

# Survive a caller env with no usable PATH (see worktree-write-protect.sh).
PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

command -v fno >/dev/null 2>&1 || exit 0
command -v jq >/dev/null 2>&1 || exit 0

SID="$(jq -r '.session_id // empty' 2>/dev/null)"
[[ "$SID" =~ /^[0-9a-f-]{16,}$/ ]] || exit 0

BUS="${FNO_STATE_DIR:-$HOME/.fno}/bus/control-pending"
FIRST8="${SID:0:8}"
LAST8="${SID: -8}"
if [[ ! -f "$BUS/$FIRST8.flag" && ! -f "$BUS/$SID.flag" && ! -f "$BUS/$LAST8.flag" ]]; then
  exit 0
fi

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/with-timeout.sh
source "$HOOK_DIR/../scripts/lib/with-timeout.sh" 2>/dev/null || exit 0

# Stdout of the verb IS the hook payload: it streams through fd 3 untouched,
# byte-for-byte (same discipline as inject-mail-notify.sh). A miss never
# blocks the tool call it rode in on.
exec 3>&1
drain_err="$(with_timeout 5 fno agents mail control-drain 2>&1 1>&3 3>&-)"
exec 3>&-
exit 0
