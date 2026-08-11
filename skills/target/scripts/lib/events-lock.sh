#!/usr/bin/env bash

EVENTS_STALE_MUTEX_SECONDS=120

_event_process_identity() {
    local pid="${1:?pid required}"
    LC_ALL=C ps -o lstart= -p "$pid" 2>/dev/null | tr -s '[:space:]' ' ' | sed 's/^ //; s/ $//'
}

_resolve_event_symlink() {
    local path="${1:?path required}"
    local link_target
    local hops=0
    while [[ -L "$path" ]]; do
        (( hops < 40 )) || return 1
        link_target=$(readlink "$path") || return 1
        if [[ "$link_target" == /* ]]; then
            path="$link_target"
        else
            path="$(dirname "$path")/$link_target"
        fi
        hops=$((hops + 1))
    done
    local physical_dir
    physical_dir=$(cd "$(dirname "$path")" 2>/dev/null && pwd -P) || return 1
    printf '%s/%s' "$physical_dir" "$(basename "$path")"
}

_steal_stale_event_dir() {
    local lock_dir="${1:?event lock directory required}"
    local modified now age owner_before owner_check owner_after reap
    owner_before=$(cat "$lock_dir/owner" 2>/dev/null || true)
    modified=$(stat -c %Y "$lock_dir" 2>/dev/null || stat -f %m "$lock_dir" 2>/dev/null) || return 1
    now=$(date +%s)
    age=$((now - modified))
    (( age > EVENTS_STALE_MUTEX_SECONDS )) || return 1

    owner_check=$(cat "$lock_dir/owner" 2>/dev/null || true)
    [[ "$owner_check" == "$owner_before" ]] || return 1
    reap="${lock_dir}.reap.${BASHPID:-$$}.${RANDOM}"
    mv "$lock_dir" "$reap" 2>/dev/null || return 1
    owner_after=$(cat "$reap/owner" 2>/dev/null || true)
    if [[ "$owner_after" != "$owner_before" ]]; then
        mv "$reap" "$lock_dir" 2>/dev/null || true
        return 1
    fi
    command -p rm -f "$reap/owner" 2>/dev/null || true
    rmdir "$reap" 2>/dev/null || true
    return 0
}
