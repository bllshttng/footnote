#!/usr/bin/env bash

EVENTS_STALE_MUTEX_SECONDS=120

_steal_stale_event_dir() {
    local lock_dir="${1:?event lock directory required}"
    local modified now age owner_before owner_after reap
    modified=$(stat -c %Y "$lock_dir" 2>/dev/null || stat -f %m "$lock_dir" 2>/dev/null) || return 1
    now=$(date +%s)
    age=$((now - modified))
    (( age > EVENTS_STALE_MUTEX_SECONDS )) || return 1

    owner_before=$(cat "$lock_dir/owner" 2>/dev/null || true)
    reap="${lock_dir}.reap.${BASHPID:-$$}.${RANDOM}"
    mv "$lock_dir" "$reap" 2>/dev/null || return 1
    owner_after=$(cat "$reap/owner" 2>/dev/null || true)
    if [[ "$owner_after" != "$owner_before" ]]; then
        mv "$reap" "$lock_dir" 2>/dev/null || true
        return 1
    fi
    rm -f "$reap/owner" 2>/dev/null || true
    rmdir "$reap" 2>/dev/null || true
    return 0
}
