#!/usr/bin/env bash
# hooks/inject-mail-notify.sh -- push-first mail delivery at the turn boundary (x-39a4).
#
# UserPromptSubmit hook. The durable bus delivers on a pull model whose only
# drain point is SessionStart, so mail sent to a long-lived session sits unread
# for the life of that session (a 13.5h run never restarts). This hook makes
# delivery push: every turn it runs `fno mail notify-self` (stat-only) and, when
# there is unread inbound mail OR the session's own sent mail is unclaimed past
# the TTL, injects a one-line nudge as UserPromptSubmit additionalContext.
#
# notify-self NEVER advances the consume cursor, so the nudge is persistent (it
# re-injects each turn while unread) and clears the instant the agent drains.
# Silent when there is no harness identity, no unread mail, or fno/jq is missing;
# a 2s portable timeout bounds a hung binary; always exits 0, never blocks the
# turn.

set -uo pipefail

command -v fno >/dev/null 2>&1 || exit 0
command -v jq >/dev/null 2>&1 || exit 0

# Portable timeout (macOS lacks timeout(1); coreutils installs gtimeout). The
# cap only matters for a hung binary; notify-self is a cheap cursor stat.
_with_timeout() {
  local secs="$1"; shift
  if command -v timeout >/dev/null 2>&1; then
    timeout "$secs" "$@"
  elif command -v gtimeout >/dev/null 2>&1; then
    gtimeout "$secs" "$@"
  else
    "$@"
  fi
}

OUTPUT=$(_with_timeout 2 fno mail notify-self 2>/dev/null || true)
[[ -z "$OUTPUT" ]] && exit 0

# notify-self already defangs </system-reminder> in every interpolated field, so
# OUTPUT is safe to embed. jq --arg keeps the JSON valid regardless.
REMINDER="<system-reminder>
${OUTPUT}
</system-reminder>"

jq -n --arg ctx "$REMINDER" \
  '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":$ctx}}'

exit 0
