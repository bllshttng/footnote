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
# from refs/remotes. One implementation, two consumers - the same shape as
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
# with no remotes needs no special case: `--not --remotes` then excludes
# nothing, every commit counts, and the tree stays.

# Set to 1 once a fetch --all --prune has succeeded in this process; exported so a
# sweep's archive-worktree children inherit the freshness instead of
# re-fetching per reaped tree.
_WT_REMOTE_REFS_FRESH="${_WT_REMOTE_REFS_FRESH:-0}"

# Refresh every remote's tracking refs, pruning ones whose upstream branch is
# gone on the server. Once per process (and per inheriting child process):
# the sweep judges many trees against one refresh.
wt_refresh_remote_refs() {
    [[ "$_WT_REMOTE_REFS_FRESH" == 1 ]] && return 0
    if git -C "${1:-.}" fetch --all --prune >/dev/null 2>&1; then
        _WT_REMOTE_REFS_FRESH=1
        export _WT_REMOTE_REFS_FRESH
        return 0
    fi
    return 1
}

wt_unpushed_count() {
    local path="${1:-}" out=""
    [[ -n "$path" && -d "$path" ]] || { printf '1\n'; return; }
    if ! wt_refresh_remote_refs "$path"; then
        echo "worktree-unpushed: remote refs not verifiable (fetch --all --prune failed); assuming unpushed" >&2
        printf '1\n'; return
    fi
    out="$(git -C "$path" rev-list --count HEAD --not --remotes 2>/dev/null)" || out=""
    [[ "$out" =~ ^[0-9]+$ ]] || out=1
    printf '%s\n' "$out"
}
