#!/usr/bin/env bash
# hook-budget.sh - the one load-aware budget for the OPTIONAL footnote hooks
# (the context, nudge, inject and announce families), plus the cache those
# hooks read instead of running a live query.
#
# Why (x-72bb): a UserPromptSubmit hook timed out after 30s and blocked the
# turn. A generous harness ceiling is not a budget, because a hook that waits
# blocks turns and mail. Optional context must fail open FAST, and load must
# SHORTEN the budget, never lengthen it.
#
# Contract:
#   idle machine (load1 <= cores)            -> HOOK_BUDGET_IDLE_SECS (3)
#   loaded machine (cores < load1 <= 2*cores) -> HOOK_BUDGET_BUSY_SECS (1)
#   past load threshold (load1 > 2*cores)     -> 0, skip entirely
#   load unreadable                           -> idle budget, fail open; the
#                                                wall-clock bound still caps
# A fired bound or a skip reads as silence: exit 0 with empty output, never an
# error a turn could inherit. The bound rides with_timeout from
# scripts/lib/with-timeout.sh, the one wall-clock bound in this tree, so stock
# macOS (no coreutils timeout) is covered. Never reintroduce a
# timeout(1)/gtimeout(1) preference in front of it.
#
# Gates that DECIDE - target-stop-hook.sh, the PreToolUse write guards,
# edit-integrity.sh, code-review-attest.sh, target-subagent-guard.sh - keep
# their own budgets and do not source this file. docs/hooks-fail-open.md owns
# that list.

HOOK_BUDGET_IDLE_SECS=3
HOOK_BUDGET_BUSY_SECS=1

_hook_budget_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/with-timeout.sh
source "$_hook_budget_dir/with-timeout.sh" 2>/dev/null || return 1

# One-minute load average, empty when unreadable. /proc/loadavg on Linux;
# sysctl on macOS prints "{ 3.24 2.90 2.71 }", so the tier picks field 2.
hook_load1() {
    if [[ -r /proc/loadavg ]]; then
        set -- $(cat /proc/loadavg 2>/dev/null)
        printf '%s' "${1:-}"
        return 0
    fi
    set -- $(sysctl -n vm.loadavg 2>/dev/null)
    case $# in
        3) printf '%s' "$1" ;;
        4) printf '%s' "$2" ;;
    esac
}

hook_cores() {
    local n
    n=$(getconf NPROCESSORS_ONLN 2>/dev/null)
    case "$n" in
        '' | *[!0-9]*) n=1 ;;
    esac
    printf '%s' "$n"
}

# The seconds an optional hook may keep its turn waiting.
hook_budget_secs() {
    local load
    load=$(hook_load1)
    case "$load" in
        '' | *[!0-9.]*) printf '%s' "$HOOK_BUDGET_IDLE_SECS"; return 0 ;;
    esac
    local budget
    budget=$(awk -v l="$load" -v c="$(hook_cores)" \
        -v idle="$HOOK_BUDGET_IDLE_SECS" -v busy="$HOOK_BUDGET_BUSY_SECS" \
        'BEGIN { print (l > 2 * c) ? 0 : (l > c) ? busy : idle }' 2>/dev/null)
    case "$budget" in
        '' | *[!0-9]*) printf '%s' "$HOOK_BUDGET_IDLE_SECS" ;;
        *) printf '%s' "$budget" ;;
    esac
}

# hook_run_optional CMD [ARGS...]: run an optional hook's query under the
# load-aware budget. A skip or a fired bound reads as silence (empty output,
# status 0); any other status passes through with the child's stdout. Stdin
# passes through, so pipeline callers keep working.
hook_run_optional() {
    local budget out rc=0
    budget=$(hook_budget_secs)
    case "$budget" in
        '' | 0) return 0 ;;
    esac
    out=$(with_timeout "$budget" "$@") || rc=$?
    [[ "$rc" -eq 124 ]] && return 0
    printf '%s' "$out"
    return "$rc"
}

# hook_cache_serve KEY MAX_AGE_SECS -- CMD [ARGS...]
# Prints CMD's output, from a cached copy when one is young enough, so a
# repeated context read costs milliseconds instead of a live query. The cache
# is refreshed OFF the turn path: a copy served past a third of its life arms
# a detached refresher for the NEXT boundary; the synchronous run also writes
# the cache. A sync run that skipped or expired under load serves the stale
# copy rather than nothing - the point of the cache is that context survives
# the days a live query would not. Cache files live under
# ${FNO_HOOK_CACHE_DIR:-$HOME/.fno/cache/hook-budget}; KEY is per session or
# per transcript, supplied by the caller.
hook_cache_serve() {
    local key="$1" max_age="$2"
    shift 2
    [[ "${1:-}" == "--" ]] && shift
    case "$key" in '' | *[!A-Za-z0-9._-]*) return 2 ;; esac
    case "$max_age" in
        '' | *[!0-9]*) return 2 ;;
    esac
    local dir="${FNO_HOOK_CACHE_DIR:-$HOME/.fno/cache/hook-budget}"
    local file="$dir/$key" now mtime age out rc=0
    now=$(date +%s)
    mtime=$(stat -f %m "$file" 2>/dev/null || stat -c %Y "$file" 2>/dev/null || printf 0)
    age=$((now - mtime))
    if [[ "$mtime" -gt 0 && "$age" -lt "$max_age" ]]; then
        cat "$file" 2>/dev/null
        # Past two thirds of its life: arm the detached refresher for the
        # next boundary. Atomic mv, so two racing refreshers cannot tear a
        # read; last writer wins.
        if ((age * 3 >= max_age * 2)); then
            (
                rbudget=$(hook_budget_secs)
                [[ "$rbudget" -gt 0 ]] || exit 0
                fresh=$(with_timeout "$rbudget" "$@") || exit 0
                [[ -n "$fresh" ]] || exit 0
                printf '%s' "$fresh" > "$file.tmp.$$" && mv -f "$file.tmp.$$" "$file"
            ) >/dev/null 2>&1 </dev/null &
        fi
        return 0
    fi
    out=$(hook_run_optional "$@") || rc=$?
    if [[ -n "$out" ]]; then
        mkdir -p "$dir" 2>/dev/null
        printf '%s' "$out" > "$file.tmp.$$" 2>/dev/null \
            && mv -f "$file.tmp.$$" "$file" 2>/dev/null
        printf '%s' "$out"
        return "$rc"
    fi
    # The live query skipped or expired: serve the stale copy, never nothing.
    [[ "$mtime" -gt 0 ]] && cat "$file" 2>/dev/null
    return 0
}
