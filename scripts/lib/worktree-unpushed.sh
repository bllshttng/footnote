#!/usr/bin/env bash
# worktree-unpushed.sh - the "does this HEAD hold anything no remote has?" answer.
#
#   source "${REPO_ROOT}/scripts/lib/worktree-unpushed.sh"
#   n="$(wt_unpushed_count "$wt")"   # 0 = every commit already on some remote
#
# Both removal call sites (the `--merged` sweep in worktree-lifecycle.sh and
# the strict check in archive-worktree.sh) ask this to decide a detached HEAD:
# detached by construction says nothing about content, so the real question is
# whether removing the tree destroys any commit, and that is exactly reachability
# from the currently verified remote branch tips. One implementation, two consumers - the same shape as
# worktree-reapable.sh, kept in a separate lib because it is pure git where the
# reapable question needs the fno classifier, and a pure-git answer stays
# correct in a vendored test fixture with no toolchain.
#
# FAIL TOWARD KEEP. Only a literal count of 0 from git authorizes reaping;
# a missing path, a git error, or any non-numeric answer prints 1. A 0 is
# also only truthful against CURRENT remote refs: a branch deleted on the
# server leaves its local tracking ref behind, and that stale ref would
# answer for a commit no remote carries anymore, so the count is taken only
# after a verified `fetch --all --prune` - a refresh that cannot be verified
# (no network, dead remote) reads as "might hold unique commits". A repo
# with no remotes needs no special case: the verified ref list is empty, so
# every commit counts and the tree stays.

# Set to 1 once a complete fetch --all --prune has succeeded in this shell.
# A sweep reuses that answer across its candidates, while archive-worktree.sh
# clears it at the destructive boundary and refreshes again. A failed refresh
# caches too, so one unreachable remote costs one connect timeout per shell.
_WT_REMOTE_REFS_FRESH="${_WT_REMOTE_REFS_FRESH:-0}"
_WT_REMOTE_REFS_STALE="${_WT_REMOTE_REFS_STALE:-0}"
_WT_VERIFIED_REMOTE_REFS="${_WT_VERIFIED_REMOTE_REFS:-}"

# A successful fetch only verifies the refs selected by each remote's fetch
# refspec. Refuse a narrowed, negative, or fetch-all-skipped mapping:
# refs/remotes can still contain an older branch outside what was fetched,
# and that stale ref must not vouch for a commit the server no longer carries.
_wt_remote_refspecs_cover_all_heads() {
    local path="${1:-.}" remotes="" remote="" spec="" covers="" skip=""
    remotes="$(git -C "$path" remote 2>/dev/null)" || return 1
    while IFS= read -r remote; do
        [[ -z "$remote" ]] && continue
        skip="$(git -C "$path" config --bool --get "remote.${remote}.skipFetchAll" 2>/dev/null || true)"
        [[ "$skip" == "true" ]] && return 1
        skip="$(git -C "$path" config --bool --get "remote.${remote}.skipDefaultUpdate" 2>/dev/null || true)"
        [[ "$skip" == "true" ]] && return 1
        covers=""
        while IFS= read -r spec; do
            case "$spec" in
                ^refs/heads/*) return 1 ;;
                "+refs/heads/*:refs/remotes/${remote}/*"|"refs/heads/*:refs/remotes/${remote}/*")
                    covers=1 ;;
            esac
        done < <(git -C "$path" config --get-all "remote.${remote}.fetch" 2>/dev/null || true)
        [[ -n "$covers" ]] || return 1
    done <<< "$remotes"
    return 0
}

# Refresh every remote's tracking refs, pruning ones whose upstream branch is
# gone on the server. Once per calling shell, the sweep judges many trees
# against one refresh. Remote-agnostic by
# design: the count asks "does ANY remote carry this", so it must verify
# whichever remotes exist, never a hardcoded name - origin-keyed fatal
# semantics belong to the merged-sweep caller, which fetches origin itself.
# Returns 0 (all fresh) or 1 (some remote unverifiable).
wt_refresh_remote_refs() {
    local path="${1:-.}" remotes="" remote="" refs=""
    [[ "$_WT_REMOTE_REFS_FRESH" == 1 ]] && return 0
    [[ "$_WT_REMOTE_REFS_STALE" == 1 ]] && return 1
    if ! _wt_remote_refspecs_cover_all_heads "$path"; then
        _WT_REMOTE_REFS_STALE=1
        return 1
    fi
    if git -C "$path" fetch --all --prune >/dev/null 2>&1; then
        remotes="$(git -C "$path" remote 2>/dev/null)" || {
            _WT_REMOTE_REFS_STALE=1
            return 1
        }
        _WT_VERIFIED_REMOTE_REFS=""
        while IFS= read -r remote; do
            [[ -z "$remote" ]] && continue
            refs="$(git -C "$path" for-each-ref --format='%(refname)' "refs/remotes/${remote}/" 2>/dev/null)" || {
                _WT_REMOTE_REFS_STALE=1
                return 1
            }
            _WT_VERIFIED_REMOTE_REFS="${_WT_VERIFIED_REMOTE_REFS}${_WT_VERIFIED_REMOTE_REFS:+$'\n'}${refs}"
        done <<< "$remotes"
        _WT_REMOTE_REFS_FRESH=1
        return 0
    fi
    _WT_REMOTE_REFS_STALE=1
    return 1
}

wt_unpushed_count() {
    local path="${1:-}" out=""
    [[ -n "$path" && -d "$path" ]] || { printf '1\n'; return 1; }
    if ! wt_refresh_remote_refs "$path"; then
        echo "worktree-unpushed: remote refs not verifiable; assuming unpushed" >&2
        printf '1\n'; return 1
    fi
    # Git refnames cannot contain whitespace, so splitting this verified list
    # into rev arguments is exact. An empty list means no remote carries HEAD.
    # shellcheck disable=SC2086
    out="$(git -C "$path" rev-list --count HEAD --not $_WT_VERIFIED_REMOTE_REFS 2>/dev/null)" || out=""
    if [[ ! "$out" =~ ^[0-9]+$ ]]; then
        printf '1\n'
        return 1
    fi
    printf '%s\n' "$out"
}
