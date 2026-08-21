#!/usr/bin/env bash
# plans-dir.sh - shared plans-dir resolution for the PreToolUse write guards.
#
# `fno do plan path` IS the save-location convention: it walks the config
# precedence (Claude Code settings.local.json plansDirectory -> settings.json ->
# config.plans_dir in .fno/config.toml -> ~/.fno/config.toml) and joins the
# plans_filename template. Taking `dirname` of a probe path reuses that chain
# instead of reimplementing it in shell twice - one resolver, so the positive
# guard (plan-location-guard.sh) and the negative-guard carve-out
# (worktree-write-protect.sh) cannot disagree about where plans belong.
#
# Every function degrades to "unresolvable" (non-zero, empty stdout) rather than
# guessing. A caller that cannot resolve the plans dir must fall back to its
# prior behavior; it must never treat unresolvable as "outside the plans dir".

set -uo pipefail

# fno_plans_dir -> prints the physical configured plans dir on stdout.
# Returns non-zero (and prints nothing) when `fno` is absent or fails.
fno_plans_dir() {
    local probe dir phys
    command -v fno >/dev/null 2>&1 || return 1
    # --slug is required; the probe slug never touches disk. Config-parse notices
    # go to stderr, but tail -1 also protects against a stray stdout preamble.
    probe="$(fno do plan path --slug _plans_dir_probe 2>/dev/null | tail -1)" || return 1
    [[ "$probe" == /* ]] || return 1
    dir="$(dirname "$probe")"
    # Physical form, via the same resolver the containment test uses, so both
    # sides of that comparison are always in one namespace. The plans dir is
    # commonly reached through a symlink (internal/ -> the vault) and commonly
    # does not exist yet; a prefix test misses on either alone.
    phys="$(fno_physical_path "$dir")"
    printf '%s\n' "${phys:-$dir}"
}

# fno_physical_path PATH -> PATH in one canonical namespace: its deepest
# EXISTING ancestor resolved physically (following symlinks), with the
# not-yet-existing tail re-appended. Returns non-zero when nothing resolves.
#
# This exists because comparing a path that does not exist yet against one that
# does otherwise mixes namespaces. `cd -P` cannot enter a missing directory, so
# an absent plans dir stays logical (`/var/...`) while a real target resolves
# physical (`/private/var/...`) and the two never match - which is how the plans
# dir came to be judged outside ITSELF. Resolving only as far as the deepest
# existing ancestor and dropping the tail has the same defect in reverse: a path
# that does not exist yet then looks like its own parent.
#
# The missing tail needs no symlink resolution (nothing there exists to be a
# link), so links are followed at the leaf and on every directory along the way
# (`cd -P`) - the path itself included, not only its ancestors. One shape is
# deliberately left unresolved: an intermediate symlink to a FILE, which is
# peeled into the tail like any other non-directory. Nothing can be written
# through it (the kernel refuses a path that descends into a file), and the
# carve-out needs EVERY target inside the plans dir, so it cannot launder a
# second one alongside.
fno_physical_path() {
    local path="$1" tail="" base head phys link hops=0
    [[ -n "$path" ]] || return 1
    # Dereference a link AT the path. The walk-up below tests `-d`, so an
    # existing symlink to a FILE fails that test and would be peeled into the
    # tail and never resolved. The plans dir is normally a vault, where
    # symlinked notes are ordinary, so a link there pointing at a tracked source
    # file would otherwise satisfy containment and carve itself out - approving
    # a write onto the shared branch. A path that does not exist has no link to
    # follow, so the not-yet-created case is untouched.
    while [[ -L "$path" ]]; do
        hops=$((hops + 1))
        [[ $hops -le 40 ]] || return 1
        link="$(readlink "$path" 2>/dev/null)" || return 1
        if [[ "$link" == /* ]]; then
            path="$link"
        else
            path="$(dirname "$path")/$link"
        fi
    done
    while [[ ! -d "$path" ]]; do
        base="${path##*/}"
        head="${path%/*}"
        [[ -n "$head" ]] || head="/"
        [[ "$head" != "$path" ]] || return 1
        if [[ -n "$base" && "$base" != "." ]]; then
            tail="$base${tail:+/$tail}"
        fi
        path="$head"
    done
    phys="$(cd -P "$path" 2>/dev/null && pwd -P)" || return 1
    printf '%s\n' "${phys%/}${tail:+/$tail}"
}

# fno_under_plans_dir PLANS_DIR PATH -> 0 when PATH is inside PLANS_DIR.
# An unresolvable PATH or an empty PLANS_DIR is NOT under it, so callers fail
# closed. Both sides are put in physical form first, so this is one comparison
# in one namespace rather than a lexical guess plus a physical fallback.
fno_under_plans_dir() {
    local plans_dir="$1" path="$2" phys
    [[ -n "$plans_dir" && -n "$path" ]] || return 1
    # A `..` component under a directory that does not exist cannot be folded
    # away (there is nothing to resolve it against), and it can sit under the
    # prefix textually while pointing somewhere else. Refuse rather than guess.
    case "$path" in
        */../*|*/..) return 1 ;;
    esac
    phys="$(fno_physical_path "$path")" || return 1
    # The trailing slashes make this a self-or-descendant test without matching
    # a sibling whose name merely shares the prefix (".../plans-archive").
    [[ "$phys/" == "$plans_dir/"* ]]
}
