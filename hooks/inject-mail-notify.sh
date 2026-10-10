#!/usr/bin/env bash
# fno hook: UserPromptSubmit - notify about pending mail
# hooks/inject-mail-notify.sh -- durable mail delivery at the turn boundary.
#
# The native verb owns rendering, UserPromptSubmit JSON serialization, stdout
# flush, and only then cursor acknowledgement: a Rust early-dispatch arm
# answers in microseconds where the old Python path paid a full interpreter
# start and was cancelled at its budget on every run. This shell layer is a
# cheap gate now: it checks the session identity, the bus log, and the
# binary, then relays the verb's already-valid envelope through fd 3
# byte-for-byte, so there is no second capture or write boundary between
# visible delivery and acknowledgement. Silent when there is no harness
# identity or mail; the one load-aware hook budget bounds a hung binary;
# failures never block the turn.

set -uo pipefail

# Survive a caller env with no usable PATH (see worktree-write-protect.sh).
PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/hook-budget.sh
source "$HOOK_DIR/../scripts/lib/hook-budget.sh" 2>/dev/null || exit 0

# Overload skip before the verb: past the threshold the preamble alone can
# pass the harness cap, so the verb runs only to be killed mid-ack, which is
# the lost-mail case. Skipped mail stays pending and delivers next turn.
hook_overloaded && exit 0

# No bus log, nothing to deliver: one stat guards the common no-mail case
# before any fork.
BUS="${FNO_STATE_DIR:-$HOME/.fno}/bus"
[[ -f "$BUS/messages.jsonl" ]] || exit 0

# The binary gate runs before identity: a gate skip records nothing, so a
# missing fno-agents must not be able to masquerade as a missing session id.
command -v fno-agents >/dev/null 2>&1 || exit 0

# No harness identity, no delivery. The pattern admits opencode's ses_ ids
# alongside claude's session uuids; canonical_handle() first-eights both.
# The assignment's status IS the substitution's: rc 127 means jq is missing,
# and reading that as "no session id" would disable delivery on every run
# with no miss row. jq reads the real top-level key; grepping the raw input
# for one would let a planted "session_id" in a prompt fake the identity.
SID=$(jq -r '.session_id // empty' 2>/dev/null)
if (( $? == 127 )); then
    source "$HOOK_DIR/../scripts/lib/events.sh" 2>/dev/null \
        && emit_event_raw_literal mail_notify_self_missed \
            '{"rc":127,"stderr_tail":"jq not found; session id unreadable"}' \
            "hook" 2>/dev/null
    exit 0
fi
case "$SID" in
    '' | *[!A-Za-z0-9_-]*) exit 0 ;;
esac
[[ "${#SID}" -ge 16 ]] || exit 0

# Stdout of the atomic verb IS the hook payload: it streams through fd 3
# (saved below) untouched, byte-for-byte. Stderr lands in the variable so a
# miss can name its cause instead of looking like every other miss. A miss is
# recorded, never raised: the turn always proceeds (contract above). The
# budget is the one load-aware hook budget: under fleet load it SHORTENS to
# 1s and past the load threshold the read is skipped entirely.
budget="$(hook_budget_secs)"
[[ "$budget" -gt 0 ]] || exit 0
exec 3>&1
notify_err="$(with_timeout "$budget" fno-agents mail-notify-self --bus-dir "$BUS" --session "$SID" 2>&1 1>&3 3>&-)"
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
