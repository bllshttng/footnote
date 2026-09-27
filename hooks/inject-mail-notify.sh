#!/usr/bin/env bash
# hooks/inject-mail-notify.sh -- durable mail delivery at the turn boundary.
#
# The hidden CLI verb owns rendering, UserPromptSubmit JSON serialization,
# stdout flush, and only then cursor acknowledgement. This shell layer relays
# that already-valid envelope directly so there is no second capture or write
# boundary between visible delivery and acknowledgement. Silent when there is
# no harness identity or mail; a portable two-second timeout bounds a hung
# binary; failures never block the turn.

set -uo pipefail

# Survive a caller env with no usable PATH (see worktree-write-protect.sh).
PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

command -v fno >/dev/null 2>&1 || exit 0

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/with-timeout.sh
source "$HOOK_DIR/../scripts/lib/with-timeout.sh" 2>/dev/null || exit 0

# Stdout of the atomic verb IS the hook payload: it streams through fd 3
# (saved below) untouched, byte-for-byte. Stderr lands in the variable so a
# miss can name its cause instead of looking like every other miss. A miss is
# recorded, never raised: the turn always proceeds (contract above).
exec 3>&1
notify_err="$(with_timeout 2 fno agents mail notify-self 2>&1 1>&3 3>&-)"
notify_rc=$?
exec 3>&-

# A miss (timeout 124, identity refusal, crash) is recorded so the next
# reading of mail_notify_self_missed rows against agent_mail_drained gaps
# names the cause. Recording a miss never blocks the turn.
if [[ "$notify_rc" -ne 0 ]] && source "$HOOK_DIR/../scripts/lib/events.sh" 2>/dev/null; then
    stderr_tail="$(printf '%s' "$notify_err" | tail -n 1 2>/dev/null | cut -c1-200)"
    emit_event_raw mail_notify_self_missed \
        "$(jq -n --argjson rc "$notify_rc" --arg tail "$stderr_tail" \
            'if $tail == "" then {rc: $rc} else {rc: $rc, stderr_tail: $tail} end' 2>/dev/null)" \
        "hook" 2>/dev/null || true
fi

exit 0
