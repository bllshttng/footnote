#!/usr/bin/env bash
# scripts/ci/smoke-duration-report.sh <shard-name> <elapsed-seconds> <cap-minutes>
#
# Reads ONE smoke shard's own duration and says whether it is approaching its
# cap, on the run that produced it, with no stored state.
#
# WHY THIS EXISTS. The shards already emitted their duration and nothing read
# it. That is not a small gap: the suite grew from a 17.2-minute median to 32.2
# over sixteen days, monotonically, across roughly 300 runs, and nobody noticed
# until eleven PRs stalled at once. Every one of those runs was measurable the
# whole time. The instrument was never missing; the reader was.
#
# WHAT THIS CANNOT SEE, AND IT MATTERS. A per-run threshold is blind to a slow
# ramp in which no single run trips it, which is EXACTLY the failure above: no
# individual run ever looked alarming. This is a down payment on that problem,
# not a solution to it. Owning the growth itself is a separate piece of work.
#
# NEVER SUM THE SHARDS. They run in PARALLEL, so wall clock is the MAX of the
# two and never the total. Their sum is 36m45s against a 32m unsharded
# baseline, so a summing consumer reports the suite got SLOWER when it got 41
# percent faster, and would fire on a perfectly healthy run. This script takes
# exactly one duration and refuses a second, so there is no code path in which
# two shard durations are both in scope.
#
# IT ALWAYS EXITS 0. It runs inside an EXIT trap next to the suite itself, so
# it must never be the thing that reddens a green run. Bad input gets a
# diagnostic, not a failure.

# No `set -e`: a reporter that aborts mid-way would swallow its own verdict
# line, and a missing verdict line is indistinguishable from "never ran".
set -uo pipefail

# Fraction of the cap at which a run starts warning. 80 percent of a 30-minute
# cap is 24 minutes. The slower shard sits near 19m and gains roughly 0.45
# minutes a day, so 80 gives about eleven days of notice. 70 percent would be
# 21 minutes, about four days out, so it would fire almost immediately and
# become permanent noise. A warning that is always on is a warning nobody
# reads, which is the defect this script exists to remove.
WARN_PCT="${SMOKE_WARN_PCT:-80}"

say() { printf 'smoke-duration: %s\n' "$*"; }

if [ "$#" -gt 3 ]; then
    say "refusing $# arguments: this reporter measures ONE shard."
    say "The shards run in PARALLEL, so wall clock is the MAX of the two and"
    say "never the total. Summing them reports the suite got slower when it got"
    say "faster, and would fire on a healthy run. Call it once per shard."
    exit 0
fi

shard="${1:-}"
secs="${2:-}"
cap_min="${3:-}"

if [ -z "$shard" ]; then
    say "no shard name given; nothing measured."
    exit 0
fi

case "$secs" in
    '' | *[!0-9]*)
        say "shard=$shard got a non-numeric duration (${secs:-empty}); nothing measured."
        exit 0
        ;;
esac

case "$cap_min" in
    '' | *[!0-9]* | 0)
        say "shard=$shard got a bad cap (${cap_min:-empty}); nothing measured."
        exit 0
        ;;
esac

# The original emitter, unchanged. Anything already grepping this keeps
# matching, so this script is additive rather than a replacement.
printf '%s-duration-seconds=%s\n' "$shard" "$secs"

cap_secs=$((cap_min * 60))
pct=$((secs * 100 / cap_secs))
threshold=$((cap_secs * WARN_PCT / 100))

verdict=ok
[ "$secs" -ge "$threshold" ] && verdict=approaching

# A positive verdict word on EVERY run. A reader can then tell "the checker ran
# and this was fine" from "the checker never ran", which the absence of a
# warning alone cannot do.
say "shard=$shard seconds=$secs cap=${cap_min}m pct=${pct} verdict=${verdict}"

if [ "$verdict" = approaching ]; then
    msg="$shard took ${secs}s, ${pct}% of its ${cap_min}m cap (warns at ${WARN_PCT}%). Split again or cut work; do not raise the cap."
    # An annotation surfaces in the PR's Checks tab without anyone opening a
    # log. That visibility is the entire difference between this and the line
    # it sits beside.
    printf '::warning title=smoke shard approaching its cap::%s\n' "$msg"
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ] && [ -w "$(dirname "$GITHUB_STEP_SUMMARY")" ]; then
        printf '**smoke duration**: %s\n' "$msg" >> "$GITHUB_STEP_SUMMARY" 2>/dev/null
    fi
fi

exit 0
