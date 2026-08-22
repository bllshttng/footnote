#!/usr/bin/env bash
# SessionStart hook: drain THIS session's own cross-harness mail (US5).
#
# The receive side of the a2a relay. `fno agents mail drain-self` computes this
# session's <short-id> handle from the ambient env markers and prints any
# unread bus mail addressed to it, then advances its own cursor. Wired here so a
# codex/gemini session actually RECEIVES mail sent to `fno agents mail send <handle>`,
# not just becomes addressable. Silent when there is no harness identity in env
# or no unread mail; never blocks session start.

set -uo pipefail

command -v fno >/dev/null 2>&1 || exit 0

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/with-timeout.sh
source "$HOOK_DIR/../scripts/lib/with-timeout.sh" 2>/dev/null || exit 0

OUTPUT=$(with_timeout 2 fno agents mail drain-self 2>/dev/null || true)
[[ -z "$OUTPUT" ]] && exit 0

printf '%s\n' "$OUTPUT"
