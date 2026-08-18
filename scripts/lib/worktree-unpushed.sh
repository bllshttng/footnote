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
# a missing path, a git error, or any non-numeric answer prints 1. A repo
# with no remotes needs no special case: `--not --remotes` then excludes
# nothing, every commit counts, and the tree stays.

wt_unpushed_count() {
    local path="${1:-}" out=""
    [[ -n "$path" && -d "$path" ]] || { printf '1\n'; return; }
    out="$(git -C "$path" rev-list --count HEAD --not --remotes 2>/dev/null)" || out=""
    [[ "$out" =~ ^[0-9]+$ ]] || out=1
    printf '%s\n' "$out"
}
