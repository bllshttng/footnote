#!/usr/bin/env bash
# SessionStart hook: drain THIS session's own cross-harness mail (US5).
#
# The receive side of the a2a relay. `fno mail drain-self` computes this
# session's <harness>-<id> handle from the ambient env markers and prints any
# unread bus mail addressed to it, then advances its own cursor. Wired here so a
# codex/gemini session actually RECEIVES mail sent to `fno mail send <handle>`,
# not just becomes addressable. Silent when there is no harness identity in env
# or no unread mail; never blocks session start.

set -uo pipefail

command -v fno >/dev/null 2>&1 || exit 0

# Portable timeout (macOS lacks timeout(1); coreutils installs gtimeout). The
# cap only matters for a hung binary; drain-self returns quickly normally.
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

OUTPUT=$(_with_timeout 2 fno mail drain-self 2>/dev/null || true)
[[ -z "$OUTPUT" ]] && exit 0

printf '%s\n' "$OUTPUT"
