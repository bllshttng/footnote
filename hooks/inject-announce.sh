#!/usr/bin/env bash
# hooks/inject-announce.sh -- fleet announcements at a hook boundary.
#
# One announcement is ONE kind=announce bus line; every session reads it
# through its own per-session cursor, so this hook prints once per
# announcement per session and stays silent after. The boundary comes in as
# $1 (start | prompt | compact); for SessionStart the hook input's
# `source: compact` (a post-compact restart) remaps start -> compact so a
# compacted session re-sees the standing announcements it already read.
#
# Fail-open everywhere: no fno-agents binary, no session id in the hook JSON,
# a hung or failing read, all print nothing and exit 0. A hook must never
# block a session on announcement state.

set -uo pipefail

# Survive a caller env with no usable PATH (see worktree-write-protect.sh).
PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

command -v fno-agents >/dev/null 2>&1 || exit 0

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/with-timeout.sh
source "$HOOK_DIR/../scripts/lib/with-timeout.sh" 2>/dev/null || exit 0

boundary="${1:-prompt}"

input="$(cat 2>/dev/null || true)"
session="$(printf '%s' "$input" | jq -r '.session_id // empty' 2>/dev/null || true)"
[ -n "$session" ] || exit 0

if [ "$boundary" = "start" ]; then
    source_field="$(printf '%s' "$input" | jq -r '.source // empty' 2>/dev/null || true)"
    if [ "$source_field" = "compact" ]; then
        boundary="compact"
    fi
fi

out="$(with_timeout 2 fno-agents announce read \
    --session-id "$session" \
    --harness "${FNO_PLATFORM:-claude}" \
    --boundary "$boundary" 2>/dev/null || true)"

if [ -n "$out" ]; then
    printf '%s\n' "$out"
fi
exit 0
