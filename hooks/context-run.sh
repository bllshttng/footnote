#!/usr/bin/env bash
# fno hook: runtime helper - context run
# One runner for every fno context producer. The groups live in
# hooks/context-hooks.json; fno-agents context-run runs one group and writes
# one context_snapshot.
#
# SessionStart law d-47400067 pins the hook budget near 2s, but a full pass
# measures tens of seconds on a loaded machine. So this wrapper is
# stale-while-revalidate: it answers at once from the project's cached pass
# output and refreshes that cache in a detached pass for the next session.
# d-bad5a42f: background work never sits on the foreground path.
set -u

# Survive a caller env with no usable PATH (see worktree-write-protect.sh).
PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HOOK_DIR/.." && pwd)"
source "$HOOK_DIR/lib/agents-bin.sh"
BIN="$(fno_agents_bin "$ROOT")"
if [[ -z "$BIN" ]]; then
    # No fno-agents usually means a plugin-only install with no fno yet. The
    # front-door hook prints the install notice, so run it.
    if [[ "${1:-}" == "claude-session-start" || "${1:-}" == "codex-session-start" ]]; then
        bash "$HOOK_DIR/frontdoor-nudge-session-start.sh"
    fi
    echo "fno: fno-agents not found; install footnote: curl -fsSL fno.sh | sh" >&2
    exit 0
fi

GROUP="${1:-}"

run_sync() {
    "$BIN" context-run --group "$GROUP" --plugin-root "$ROOT"
    rc=$?
    if [ "$rc" -ne 0 ]; then
        # The runner exits 0 on its own error paths; a nonzero here is an old
        # binary that does not know the verb. Name the remedy, keep the hook clean.
        echo "fno: context-run exited $rc (stale fno-agents binary?); run fno doctor --fix" >&2
    fi
}

# Per-project cache: keyed by the working directory (the session's project),
# one file per group. FNO_CONTEXT_CACHE_DIR relocates it; the detached pass
# below writes CACHE plus a CACHE.ts epoch sibling next to it.
CACHE_DIR="${FNO_CONTEXT_CACHE_DIR:-${HOME:-/tmp}/.fno/context-cache}"
mkdir -p "$CACHE_DIR" 2>/dev/null
KEY="$(pwd -P | shasum -a 256 | cut -c1-16)"
CACHE="$CACHE_DIR/$KEY-$GROUP.json"
TTL="${FNO_CONTEXT_CACHE_TTL_SECONDS:-86400}"

# Capture the hook payload first: the detached pass needs it on stdin, and
# reading it here frees the wrapper from holding the caller's pipe.
PAYLOAD="$(mktemp "${TMPDIR:-/tmp}/fno-context-payload.XXXXXX" 2>/dev/null)" || PAYLOAD=""
if [[ -n "$PAYLOAD" ]] && ! cat > "$PAYLOAD"; then
    rm -f "$PAYLOAD"
    PAYLOAD=""
fi

if [[ -z "$GROUP" || -z "$PAYLOAD" ]]; then
    # No group name or no place for the payload: there is nothing to cache
    # against. Today's synchronous pass is the fallback, so the hook degrades
    # to known behavior.
    run_sync
    exit 0
fi

# Answer now. A cache written within the TTL replays the previous pass's
# output byte-for-byte; anything else answers with the documented no-op
# object. The detached pass refreshes either way.
TS="$(cat "$CACHE.ts" 2>/dev/null || echo 0)"
case "$TS" in ''|*[!0-9]*) TS=0 ;; esac
NOW="$(date +%s)"
if [[ -s "$CACHE" ]] && [ "$((NOW - TS))" -lt "$TTL" ]; then
    cat "$CACHE"
else
    printf '{}\n'
fi

# Detached refresh. Full stdio redirection so the child never holds the
# hook's pipes (the harness waits on that stdout), nice'd per d-bad5a42f.
# The cache is published by atomic mv only on a clean pass, so a failed run
# leaves the previous cache (or none) rather than a partial one; concurrent
# starters race to last-writer-wins.
nohup nice -n 10 bash -c "
    if '$BIN' context-run --group '$GROUP' --plugin-root '$ROOT' < '$PAYLOAD' > '$CACHE.tmp.$$' 2> '$CACHE_DIR/$KEY-$GROUP.log'; then
        mv -f '$CACHE.tmp.$$' '$CACHE'
        date +%s > '$CACHE.ts'
    else
        rm -f '$CACHE.tmp.$$'
    fi
    rm -f '$PAYLOAD'
" </dev/null >/dev/null 2>&1 &
exit 0
