#!/usr/bin/env bash
# Fallback trigger for the daily grooming pass.
#
# The gate is staleness past the predicate's threshold, NOT "no marker today":
# the installed LaunchAgent owns the cadence, and this must stay dormant behind
# a healthy one. It fires only in the case that has now killed four grooming
# surfaces - nothing scheduled it, so nothing ever ran it.
#
# Advisory throughout: every failure path exits 0 and dispatches nothing. The
# marker not advancing is what keeps a failure visible, on the next `fno doctor`.

set -uo pipefail

command -v fno >/dev/null 2>&1 || exit 0
today="$(date -u +%Y-%m-%d 2>/dev/null || echo "")"
[[ -n "$today" ]] || exit 0

watermark=".fno/.groom-heal-${today}"
[[ -e "$watermark" ]] && exit 0

# `--check` runs nothing and exits 0 only when a pass is genuinely due. Asking
# before claiming the day is what keeps a healthy machine from ever writing a
# watermark.
fno backlog groom --check >/dev/null 2>&1 || exit 0

# noclobber makes the create itself the arbiter, so N worktree sessions starting
# at once yield exactly one winner. run_groom's daily claim is the backstop.
mkdir -p .fno 2>/dev/null || exit 0
( set -o noclobber; : >"$watermark" ) 2>/dev/null || exit 0

( fno backlog groom >/dev/null 2>&1 & ) 2>/dev/null || true
exit 0
