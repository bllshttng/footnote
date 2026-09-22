#!/usr/bin/env bash
# SessionStart hook: what is waiting on a human - the session's own open
# questions plus one count line, read from the daemon's projection cache
# (~/.fno/attention/items.json, rewritten every beat) instead of the 3s
# `fno inbox outstanding` fold that exits 124 under a session-start budget.
# A cache that is missing or stale falls back to the full fold, which is
# also what a harness without the attention arm reads.
#
# Hook contract: stdout is appended to the session prompt; exit 0 always.
set -uo pipefail

# Survive a caller env with no usable PATH (see worktree-write-protect.sh).
PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

command -v fno >/dev/null 2>&1 || exit 0
# No jq: the cache path cannot parse, so the fold below answers instead of
# an empty session start.
have_jq=1
command -v jq >/dev/null 2>&1 || have_jq=0

CACHE_MAX_AGE_SECS=120
cache="${FNO_HOME:-$HOME/.fno}/attention/items.json"

if [[ $have_jq -eq 1 && -s "$cache" ]]; then
    now=$(date +%s)
    mtime=$(stat -f %m "$cache" 2>/dev/null || stat -c %Y "$cache" 2>/dev/null || echo 0)
    if (( now - mtime <= CACHE_MAX_AGE_SECS )); then
        # This session's own questions: the asker's session id is the target
        # manifest id the session already carries. No manifest means a session
        # asked nothing.
        session_id="$(fno do state show 2>/dev/null | awk '/^session_id:/ {print $2}')"
        if [[ -n "$session_id" ]]; then
            own=$(jq -r --arg s "$session_id" \
                '[.items[] | select(.kind != "mine" and .ready == true and .asker.session_id == $s)] | .[] | "- \(.kind): \(.title)"' \
                "$cache" 2>/dev/null)
        fi
        total=$(jq -r '[.items[] | select(.kind != "mine")] | length' "$cache" 2>/dev/null)
        printf '## Outstanding for you\n\n'
        if [[ -n "${own:-}" ]]; then
            printf '%s\n' "$own"
        else
            printf 'No open questions from this session.\n'
        fi
        printf '%s open across the fleet.\n' "${total:-0}"
        exit 0
    fi
fi

# Fallback: the full fold. A non-zero exit is NOT silence: collapsing a failed
# read into an empty string is the absence-as-success trap this verb exists to
# close. But exit 2 is Typer's "no such command", which a DEPLOYED fno older
# than this feature returns on EVERY session until someone runs
# `fno doctor update`. Nagging forever is noise, so the loud path is reserved
# for a real failure: a fired bound (124) or an unreadable store (1).
WT_LIB="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/../scripts/lib/with-timeout.sh"
[[ -f "$WT_LIB" ]] || exit 0
# shellcheck source=../scripts/lib/with-timeout.sh
source "$WT_LIB" 2>/dev/null || exit 0

rc=0
body=$(with_timeout 3 fno inbox outstanding 2>/dev/null) || rc=$?

[[ $rc -eq 0 ]] && { [[ -n "$body" ]] && printf '%s' "$body"; exit 0; }
[[ $rc -eq 2 ]] && exit 0

printf '## Outstanding for you\n\ncould not be read (fno inbox outstanding exit %s). Run it directly.\n' "$rc"
exit 0
