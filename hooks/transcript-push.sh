#!/usr/bin/env bash
# fno hook: the sandboxed-session transcript lane's producer shim.
# hooks/transcript-push.sh - one line of policy: exec the
# `fno-agents hook transcript-push` entry. All logic (cursor, delta read,
# RPC) lives in the binary entry; this shim carries the same 2s bound and
# fail-open shape as session-state.sh.
set -u
PATH="${PATH:+$PATH:}/usr/bin:/bin"
export PATH
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Fail closed like the producer it replaces: a missing helper makes this
# fire-and-forget hook exit 0 instead of blocking unbounded.
source "$HOOK_DIR/../scripts/lib/with-timeout.sh" 2>/dev/null || exit 0
# shellcheck source=lib/agents-bin.sh
source "$HOOK_DIR/lib/agents-bin.sh" 2>/dev/null || exit 0
BIN="$(fno_agents_bin "$PWD")"
# No binary -> nothing to report to; stay silent (the lane is best-effort).
[[ -z "$BIN" ]] && exit 0
with_timeout 2 "$BIN" hook transcript-push
exit 0
