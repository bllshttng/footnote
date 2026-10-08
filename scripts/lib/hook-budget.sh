#!/usr/bin/env bash
# hook-budget.sh - the one load-aware budget for the OPTIONAL footnote hooks
# (the context, nudge, inject and announce families), plus the cache those
# hooks read instead of running a live query.
#
# Why: a UserPromptSubmit hook timed out after 30s and blocked the turn. A
# generous harness ceiling is not a budget, because a hook that waits blocks
# turns and mail. Optional context must fail open FAST, and load must SHORTEN
# the budget, never lengthen it.
#
# Contract:
#   idle machine (load1 <= cores)             -> HOOK_BUDGET_IDLE_SECS (3)
#   loaded machine (load1 > cores)            -> HOOK_BUDGET_BUSY_SECS (1)
#   overloaded (load1 >= 8x cores)            -> hook_overloaded is true and
#                                                the hook exits 0 before any
#                                                work (no budget can help)
#   load unreadable                           -> idle budget, never skip; fail
#                                                open; the wall-clock bound
#                                                still caps
# There is no zero-budget tier: five CI suites broke under runner load when a
# threshold read as zero, because the plugin's own hook contracts require the
# read to RUN (a failed read prints its report, an exit code carries the
# answer, a suite measures the cap). Unreadable load still runs. The skip
# tier is the one exception, and only for a READABLE load: at 8+ waiting jobs
# per core the hook's own preamble (bash startup, the source chain, the
# probe's forks) can pass the harness's 4s outer cap on its own, so running
# the read guarantees a killed hook, discarded output, a red harness warning
# and a delayed turn. Skipping instead leaves every cursor untouched: mail
# stays pending, an announcement is re-seen, an offer is re-scanned next
# turn (2026-10-06 screenshots: five prompt hooks timed out on every prompt
# at 16-60 jobs per core). CI stays green because every suite that runs a
# real hook pins FNO_HOOK_BUDGET_SKIP_PER_CORE past any runner load;
# production leaves it unset. Load still SHORTENS the budget to 1s, and a
# fired bound reads as silence: exit 0 with empty output, never an error a
# turn could inherit. The bound rides with_timeout from
# scripts/lib/with-timeout.sh, the one wall-clock bound in this tree, so
# stock macOS (no coreutils timeout) is covered. Never reintroduce a
# timeout(1)/gtimeout(1) preference in front of it.
#
# Gates that DECIDE - target-stop-hook.sh, the PreToolUse write guards,
# edit-integrity.sh, code-review-attest.sh, target-subagent-guard.sh - keep
# their own budgets and do not source this file. docs/hooks-fail-open.md owns
# that list.

HOOK_BUDGET_IDLE_SECS=3
HOOK_BUDGET_BUSY_SECS=1
# Skip tier: at this many waiting jobs per core the hook's own preamble can
# pass the harness's outer cap on its own. FNO_HOOK_BUDGET_SKIP_PER_CORE
# pins it; a suite that runs a real hook pins it past any runner load.
HOOK_BUDGET_SKIP_PER_CORE=8

_hook_budget_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/with-timeout.sh
source "$_hook_budget_dir/with-timeout.sh" 2>/dev/null || return 1

# One-minute load average, empty when unreadable. /proc/loadavg on Linux;
# sysctl on macOS prints "{ 3.24 2.90 2.71 }", so the tier picks field 2.
hook_load1() {
    local load=""
    if [[ -r /proc/loadavg ]]; then
        # A redirection, not $(cat): the probe must not pay a fork, because
        # under the overload hook_overloaded measures, forks are what is slow.
        read -r load _ < /proc/loadavg 2>/dev/null || load=""
    else
        set -- $(sysctl -n vm.loadavg 2>/dev/null)
        # macOS prints "{ 3.24 2.90 2.71 }": the braces word-split into their
        # own tokens (5 fields), so the one-minute average is field 2. A bare
        # 3-tuple or a glued brace is covered for safety. The old 3-or-4 case
        # matched none of these, so on macOS the probe read as EMPTY and the
        # busy tier never engaged.
        case $# in
            3) load="$1" ;;
            4 | 5) load="$2" ;;
        esac
    fi
    printf '%s' "$load"
}

hook_cores() {
    local n
    n=$(getconf NPROCESSORS_ONLN 2>/dev/null)
    case "$n" in
        '' | *[!0-9]*) n=1 ;;
    esac
    printf '%s' "$n"
}

# The seconds an optional hook may keep its turn waiting. Never zero: the
# busy tier is the floor, because the reads must run (see the contract above).
# FNO_HOOK_BUDGET_SECS pins both tiers outright: a test harness runs nudge
# LOGIC on a shared runner whose load is a property of the shard, not the
# code, so the harness pins a generous budget instead of leaving assertion
# outcomes to the co-tenants. Unset in production, where the tiers rule.
hook_budget_secs() {
    case "${FNO_HOOK_BUDGET_SECS:-}" in
        '' | *[!0-9]*) ;;                       # unset or junk: the tiers rule
        *) printf '%s' "$FNO_HOOK_BUDGET_SECS"; return 0 ;;
    esac
    local load
    load=$(hook_load1)
    case "$load" in
        '' | *[!0-9.]*) printf '%s' "$HOOK_BUDGET_IDLE_SECS"; return 0 ;;
    esac
    local budget
    budget=$(awk -v l="$load" -v c="$(hook_cores)" \
        -v idle="$HOOK_BUDGET_IDLE_SECS" -v busy="$HOOK_BUDGET_BUSY_SECS" \
        'BEGIN { print (l > c) ? busy : idle }' 2>/dev/null)
    case "$budget" in
        '' | *[!0-9]*) printf '%s' "$HOOK_BUDGET_IDLE_SECS" ;;
        *) printf '%s' "$budget" ;;
    esac
}

# hook_overloaded: true when load1 is readable and past the skip threshold
# (8x cores; FNO_HOOK_BUDGET_SKIP_PER_CORE pins it). Unreadable load is
# NEVER overloaded: the zero-threshold CI break in the contract above is why
# an unmeasurable tier must run, not skip. The caller exits 0 on true,
# before any work.
hook_overloaded() {
    local load
    load=$(hook_load1)
    case "$load" in
        '' | *[!0-9.]*) return 1 ;;
    esac
    local threshold
    threshold="${FNO_HOOK_BUDGET_SKIP_PER_CORE:-$HOOK_BUDGET_SKIP_PER_CORE}"
    case "$threshold" in
        '' | *[!0-9]*) return 1 ;;
    esac
    awk -v l="$load" -v c="$(hook_cores)" -v k="$threshold" \
        'BEGIN { exit !(l >= c * k) }'
}

# hook_run_optional CMD [ARGS...]: run an optional hook's query under the
# load-aware budget. A fired bound reads as silence (empty output, status 0);
# any other status passes through with the child's stdout. Stdin passes
# through, so pipeline callers keep working.
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

# hook_cache_serve KEY MAX_AGE_SECS FINGERPRINT -- CMD [ARGS...]
# Prints CMD's output, from a cached copy when one is young enough AND its
# input is unchanged, so a repeated context read costs milliseconds instead
# of a live query. FINGERPRINT names the input state (for a transcript read:
# size and mtime); the cache file's first line carries the fingerprint a copy
# was built from, and a fingerprint mismatch forces the live read, because a
# changed input must re-measure. A live read that skipped or expired under
# load serves the STALE copy rather than nothing - context survives the
# moments a live query would not. The cache is refreshed OFF the turn path:
# a copy served past two thirds of its life arms a detached refresher for
# the NEXT boundary; the synchronous run also writes the cache. Atomic mv,
# so two racing refreshers cannot tear a read. Cache files live under
# ${FNO_HOOK_CACHE_DIR:-$HOME/.fno/cache/hook-budget}; KEY is per session or
# per transcript, supplied by the caller. An empty FINGERPRINT means the
# input has no observable state and age alone decides freshness.
hook_cache_serve() {
    local key="$1" max_age="$2" fp="$3"
    shift 3
    [[ "${1:-}" == "--" ]] && shift
    case "$key" in '' | *[!A-Za-z0-9._-]*) return 2 ;; esac
    case "$max_age" in
        '' | *[!0-9]*) return 2 ;;
    esac
    local dir="${FNO_HOOK_CACHE_DIR:-$HOME/.fno/cache/hook-budget}"
    local file="$dir/$key" now mtime age raw stored_fp payload out rc=0
    now=$(date +%s)
    # GNU stat first: BSD `stat -f %m` is the macOS form, and on GNU `stat -f`
    # means FILESYSTEM status, which SUCCEEDS and prints a block whose first
    # line starts with "File:" - under set -u the arithmetic then dies naming
    # File. mtime=$(stat -c %Y ... || stat -f %m ... || printf 0)
    mtime=$(stat -c %Y "$file" 2>/dev/null || stat -f %m "$file" 2>/dev/null || printf 0)
    age=$((now - mtime))
    stored_fp=""
    payload=""
    if [[ -f "$file" ]]; then
        raw=$(cat "$file" 2>/dev/null)
        if [[ "$raw" == *$'\n'* ]]; then
            stored_fp="${raw%%$'\n'*}"
            payload="${raw#*$'\n'}"
        fi
    fi
    if [[ -n "$payload" && "$age" -ge 0 && "$age" -lt "$max_age" \
        && (-z "$fp" || "$fp" == "$stored_fp") ]]; then
        printf '%s' "$payload"
        # Past two thirds of its life: arm the detached refresher for the
        # next boundary. The refresher re-runs the read and stores it under
        # the fingerprint captured HERE, so a copy can never outlive the
        # input state it was built from.
        if ((age * 3 >= max_age * 2)); then
            (
                rbudget=$(hook_budget_secs)
                [[ "$rbudget" -gt 0 ]] || exit 0
                fresh=$(with_timeout "$rbudget" "$@") || exit 0
                [[ -n "$fresh" ]] || exit 0
                printf '%s\n%s' "$fp" "$fresh" > "$file.tmp.$$" \
                    && mv -f "$file.tmp.$$" "$file"
            ) >/dev/null 2>&1 </dev/null &
        fi
        return 0
    fi
    out=$(hook_run_optional "$@") || rc=$?
    if [[ -n "$out" ]]; then
        mkdir -p "$dir" 2>/dev/null
        # Retention: one file per session or transcript, a few KB each. The
        # sweep rides the sync path only (a live read already paid its forks)
        # and trims the oldest half once the count passes the cap.
        local entries
        entries=$(ls "$dir" 2>/dev/null | wc -l)
        if ((entries > 500)); then
            ls -t "$dir" 2>/dev/null | tail -n +251 | while IFS= read -r old; do
                rm -f "$dir/$old"
            done
        fi
        printf '%s\n%s' "$fp" "$out" > "$file.tmp.$$" 2>/dev/null \
            && mv -f "$file.tmp.$$" "$file" 2>/dev/null
        printf '%s' "$out"
        return "$rc"
    fi
    # The live query skipped or expired: serve the stale copy, never nothing.
    # The status still propagates: a failed read is the caller's data, and
    # swallowing it here would read the failure as an empty result.
    [[ -n "$payload" ]] && printf '%s' "$payload"
    return "$rc"
}
