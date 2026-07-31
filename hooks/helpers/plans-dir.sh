#!/usr/bin/env bash
# plans-dir.sh - shared plans-dir resolution for the PreToolUse write guards.
#
# `fno plan path` IS the save-location convention: it walks the config
# precedence (.claude/settings.local.json plansDirectory -> .claude/settings.json
# -> config.plans_dir in .fno/config.toml -> ~/.fno/config.toml) and joins the
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
    probe="$(fno plan path --slug _plans_dir_probe 2>/dev/null | tail -1)" || return 1
    [[ "$probe" == /* ]] || return 1
    dir="$(dirname "$probe")"
    # Physical form: the plans dir is commonly reached through a symlink
    # (internal/ -> the vault), and a prefix test on the logical path misses.
    phys="$(cd -P "$dir" 2>/dev/null && pwd -P)"
    printf '%s\n' "${phys:-$dir}"
}

# fno_resolve_dir PATH -> prints the physical directory PATH lives in, following
# symlinks and walking up to the deepest ancestor that exists (a not-yet-created
# file has no directory of its own). Returns non-zero when nothing resolves.
fno_resolve_dir() {
    local target="$1" link parent hops=0
    [[ -n "$target" ]] || return 1
    while [[ -L "$target" ]]; do
        hops=$((hops + 1))
        [[ $hops -le 40 ]] || return 1
        link="$(readlink "$target" 2>/dev/null)" || return 1
        if [[ "$link" == /* ]]; then
            target="$link"
        else
            target="$(dirname "$target")/$link"
        fi
    done

    [[ -d "$target" ]] || target="$(dirname "$target")"
    while [[ ! -d "$target" ]]; do
        parent="$(dirname "$target")"
        [[ "$parent" != "$target" ]] || return 1
        target="$parent"
    done
    cd -P "$target" 2>/dev/null && pwd -P
}

# fno_under_plans_dir PLANS_DIR PATH -> 0 when PATH is inside PLANS_DIR.
# PLANS_DIR must already be physical (fno_plans_dir output). An unresolvable
# PATH or an empty PLANS_DIR is NOT under it, so callers fail closed.
fno_under_plans_dir() {
    local plans_dir="$1" path="$2" dir
    [[ -n "$plans_dir" && -n "$path" ]] || return 1
    dir="$(fno_resolve_dir "$path")" || return 1
    # Trailing slashes make this a self-or-descendant test without matching a
    # sibling whose name merely shares the prefix (".../plans-archive").
    [[ "$dir/" == "$plans_dir/"* ]]
}
