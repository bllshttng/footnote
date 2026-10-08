#!/usr/bin/env bash
# fno hook: SessionStart - inject mail drain session start
# SessionStart hook: drain THIS session's own cross-harness mail (US5).
#
# The receive side of the a2a relay. `fno agents mail drain-self` computes this
# session's <short-id> handle from the ambient env markers and prints any
# unread bus mail addressed to it, then advances its own cursor. Wired here so a
# codex/gemini session actually RECEIVES mail sent to `fno agents mail send <handle>`,
# not just becomes addressable. Silent when there is no harness identity in env
# or no unread mail; never blocks session start.

set -uo pipefail

# Survive a caller env with no usable PATH (see worktree-write-protect.sh).
PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

command -v fno >/dev/null 2>&1 || exit 0

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/hook-budget.sh
source "$HOOK_DIR/../scripts/lib/hook-budget.sh" 2>/dev/null || exit 0

OUTPUT=$(hook_run_optional fno agents mail drain-self 2>/dev/null || true)
[[ -z "$OUTPUT" ]] && exit 0

printf '%s\n' "$OUTPUT"

# The read-verb lesson rides the drain, once per session and again after
# each compaction: the door prints the line only when the session's teach
# state says due, and --mark stamps it in the same breath. Best-effort - a
# stale or absent binary teaches nothing and never blocks session start.
if [[ -r "$HOOK_DIR/lib/agents-bin.sh" ]]; then
    # shellcheck source=../hooks/lib/agents-bin.sh
    source "$HOOK_DIR/lib/agents-bin.sh"
    TEACH_BIN="$(fno_agents_bin "$HOOK_DIR/..")"
    if [[ -n "$TEACH_BIN" && -x "$TEACH_BIN" ]]; then
        hook_run_optional "$TEACH_BIN" mail-teach --self --mark 2>/dev/null || true
    fi
fi
