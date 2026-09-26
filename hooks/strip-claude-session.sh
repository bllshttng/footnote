#!/usr/bin/env bash
# strip-claude-session.sh - commit-msg hook: strip the private Claude session
# URL from a commit message BEFORE the commit exists. scripts/ci/check-no-session-urls.sh
# is only the backstop: once a commit carrying the URL is pushed, a force-push
# does not retract it, so the strip has to happen at commit time.
#
# Two shapes are removed, matching what the CI gate refuses:
#   - the harness's `Claude-Session: <url>` trailer (any leading space, any case)
#   - any line carrying a real claude.ai/code/<token> URL (a bare paste)
# Prose that merely names the concept (a `claude.ai/code` with no token char
# after it) passes untouched, same as the gate. Never fails the commit: a
# message with no session URL passes through byte-identical.

set -euo pipefail
msg_file="${1:-}"
[ -n "$msg_file" ] && [ -f "$msg_file" ] || exit 0

sed -i.bak -E \
  -e '/^[[:space:]]*[Cc]laude-[Ss]ession:/d' \
  -e '/claude\.ai\/code\/[A-Za-z0-9]/d' \
  "$msg_file"
rm -f "$msg_file.bak"
