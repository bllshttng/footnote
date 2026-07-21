#!/usr/bin/env bash
# Fallback trigger for the daily grooming pass (x-1c7b).
#
# The LaunchAgent installed by `fno backlog groom --install-agent` is the
# primary cadence and this must never compete with it: the gate is staleness
# past the predicate's threshold (48h), NOT "no marker today", so a healthy
# agent keeps its quiet 2am slot and this stays dormant for good. It fires only
# in the case that killed four grooming surfaces in a row - nothing scheduled,
# nothing ran, nobody noticed.
#
# Advisory and non-blocking throughout: no fno on PATH, an unreadable claims
# root, or a failed dispatch all exit silently. The marker not advancing is what
# keeps the failure visible, on the next `fno doctor`.

set -uo pipefail

command -v fno >/dev/null 2>&1 || exit 0

today="$(date -u +%Y-%m-%d 2>/dev/null || echo "")"
[[ -n "$today" ]] || exit 0

# Watermark is date-named and created with noclobber, so the create IS the
# arbiter: N worktree sessions starting at once produce exactly one winner
# without a lock. `run_groom`'s daily claim is the backstop behind it.
watermark=".fno/.groom-heal-${today}"
[[ -e "$watermark" ]] && exit 0

# `--check` runs nothing; exit 0 means a pass is genuinely due. Asking before
# claiming the day is what keeps a healthy machine from ever writing a
# watermark.
fno backlog groom --check >/dev/null 2>&1 || exit 0

mkdir -p .fno 2>/dev/null || exit 0
( set -o noclobber; : >"$watermark" ) 2>/dev/null || exit 0

( fno backlog groom >/dev/null 2>&1 & ) 2>/dev/null || true
exit 0
