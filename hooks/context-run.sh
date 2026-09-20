#!/usr/bin/env bash
# One runner for every fno context producer. The groups live in
# hooks/context-hooks.json; fno-agents context-run runs one group and writes
# one context_snapshot.
set -u

# Survive a caller env with no usable PATH (see worktree-write-protect.sh).
PATH="/usr/bin:/bin:/usr/sbin:/sbin${PATH:+:$PATH}"
export PATH
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HOOK_DIR/.." && pwd)"
source "$HOOK_DIR/lib/agents-bin.sh"
BIN="$(fno_agents_bin "$ROOT")"
if [[ -z "$BIN" ]]; then
    # No fno-agents usually means a plugin-only install with no fno yet. The
    # front-door hook is the one producer that starts the installer, so run it.
    if [[ "${1:-}" == "claude-session-start" ]]; then
        bash "$HOOK_DIR/frontdoor-nudge-session-start.sh"
    fi
    echo "fno: context-run unavailable (fno-agents not found); run fno doctor --fix" >&2
    exit 0
fi
"$BIN" context-run --group "${1:-}" --plugin-root "$ROOT"
rc=$?
if [ "$rc" -ne 0 ]; then
    # The runner exits 0 on its own error paths; a nonzero here is an old
    # binary that does not know the verb. Name the remedy, keep the hook clean.
    echo "fno: context-run exited $rc (stale fno-agents binary?); run fno doctor --fix" >&2
fi
exit 0
