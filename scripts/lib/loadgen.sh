#!/usr/bin/env bash
# loadgen.sh - the sanctioned CPU-load generator.
#
# Why this exists: 2026-08-13, one worker hand-rolled `yes > /dev/null` to
# reproduce a race under contention, never reaped it, and left 73 processes at
# PPID 1 burning 8 cores for 7.5 hours (node x-4825). The PreToolUse guard now
# refuses that shape at the Bash boundary, but the guard cannot see a pytest or
# cargo fixture, and a codex/opencode worker has no Claude hook lane at all.
# Every lane that can run bash can run this script, so it - not the guard - is
# the form a fixture or a non-Claude worker should reach for.
#
# The two guarantees, in the order they matter:
#   BOUND  every generator dies at its `seconds` ceiling even if its creator
#          died first. The bound is mandatory and has no override, because an
#          unbounded generator is the defect this file exists to prevent.
#   NAME   every generator's argv[0] is `fno-load-<label>-<i>`, so `top`
#          answers "whose is this?" without lsof archaeology, and
#          `fno agents orphans --reap` may kill a renamed survivor older than
#          10 minutes even when its parent died before the bound fired.
#
# The name must be argv[0], never the executable. `exec -a` puts it there;
# psutil on macOS reads the executable for name(), so a convention encoded any
# other way is invisible to the sweep's NAME arm (orphans.py display_name).
#
# Usage:
#   bash scripts/lib/loadgen.sh start <label> <seconds> [count]
#   bash scripts/lib/loadgen.sh stop  <label>
#   bash scripts/lib/loadgen.sh list  [label]
#
# start: launches `count` generators (default 1, max 32), each its own
#   `with_timeout`-bounded unit, detached so the caller gets its prompt back.
# stop: TERM then KILL on every generator of that label, and reports how many.
# list: prints pid and name per live generator, then a count. The count is only
#   printed under the listing, so a zero can never masquerade as a scan.
#
# Exit codes: 0 ok, 2 bad usage (missing/malformed label, seconds, or count).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/with-timeout.sh
source "$SCRIPT_DIR/with-timeout.sh"

MAX_BOUND_S=3600
MAX_COUNT=32
NAME_PREFIX="fno-load-"

usage() {
  printf 'usage: loadgen.sh start <label> <seconds> [count] | stop <label> | list [label]\n' >&2
  exit 2
}

# Label is embedded in a pgrep ERE, so the charset stays to [A-Za-z0-9_-]: no
# metacharacter can travel from a label into a pattern, and no quoting rule is
# needed anywhere below.
valid_label() {
  case "$1" in
    '' | *[!A-Za-z0-9_-]*) return 1 ;;
  esac
  ((${#1} <= 64))
}

# The bound rides with_timeout, so it takes with_timeout's validation: a bare
# integer. `30m` would exit the sleep instantly and kill at t=0, a
# misconfiguration wearing a fired bound's clothes. The ceiling keeps an
# abandoned run to an hour even if someone types a bigger number.
valid_seconds() {
  case "$1" in
    '' | *[!0-9]*) return 1 ;;
  esac
  ((10#$1 >= 1 && 10#$1 <= MAX_BOUND_S))
}

pattern_for() {
  # Anchored at argv[0] and closed with the mandatory -<i> suffix, so
  # `stop x` cannot reach `x1`'s generators and the pattern cannot match a
  # command that merely mentions the label.
  printf '^%s%s(-[0-9]+)?$' "$NAME_PREFIX" "$1"
}

pids_for() {
  pgrep -f "$(pattern_for "$1")" 2>/dev/null || true
}

cmd_start() {
  local label="$1" seconds="$2" count="${3:-1}" i pid pids=()
  valid_label "$label" || { printf 'loadgen: label must be [A-Za-z0-9_-], max 64 chars, got: %s\n' "$label" >&2; exit 2; }
  valid_seconds "$seconds" || { printf 'loadgen: seconds must be an integer 1..%s, got: %s\n' "$MAX_BOUND_S" "$seconds" >&2; exit 2; }
  case "$count" in
    '' | *[!0-9]*) printf 'loadgen: count must be an integer, got: %s\n' "$count" >&2; exit 2 ;;
  esac
  ((10#$count >= 1 && 10#$count <= MAX_COUNT)) || { printf 'loadgen: count must be 1..%s, got: %s\n' "$MAX_COUNT" "$count" >&2; exit 2; }

  for ((i = 1; i <= 10#$count; i++)); do
    # Each generator is its own subshell running with_timeout, so the bound
    # survives this script's exit - that is the ceiling an abandoned run hits.
    # `exec -a` renames only the generator: the subshell and the watchdog keep
    # their real names and stay outside the sweep's reap gate.
    ( with_timeout "$seconds" bash -c "exec -a ${NAME_PREFIX}${label}-${i} yes > /dev/null" ) >/dev/null 2>&1 &
  done

  # Positive receipt: name every pid we believe we started. A worker that
  # forgets `stop` can still read this line to see what will die at the bound.
  sleep 0.3
  pids_for "$label"
  printf 'loadgen: started %s generator(s) %s%s-*, bounded %ss, label %s\n' \
    "$count" "$NAME_PREFIX" "$label" "$seconds" "$label"
  printf 'loadgen: they die at the bound; run "%s stop %s" to end them now\n' "$0" "$label"
}

cmd_stop() {
  local label="$1" before after
  valid_label "$label" || { printf 'loadgen: bad label: %s\n' "$label" >&2; exit 2; }
  before="$(pids_for "$label")"
  if [[ -n "$before" ]]; then
    # Group-bound TERM via pkill; the subshell's `wait` then returns and
    # with_timeout reaps its own watchdog, so no stray sleep is left behind.
    pkill -TERM -f "$(pattern_for "$label")" 2>/dev/null || true
    sleep 1
    after="$(pids_for "$label")"
    if [[ -n "$after" ]]; then
      pkill -KILL -f "$(pattern_for "$label")" 2>/dev/null || true
      sleep 0.3
      after="$(pids_for "$label")"
    fi
  else
    after=""
  fi
  # Report the survivors only after proving the instrument saw them: `before`
  # nonempty is the positive control that makes an empty `after` mean killed.
  printf 'loadgen: matched %s, remaining %s\n' \
    "$(printf '%s' "$before" | grep -c . || true)" \
    "$(printf '%s' "$after" | grep -c . || true)"
  [[ -z "$after" ]]
}

cmd_list() {
  local label="${1:-}" pid found=0
  local pat
  if [[ -n "$label" ]]; then
    valid_label "$label" || { printf 'loadgen: bad label: %s\n' "$label" >&2; exit 2; }
    pat="$(pattern_for "$label")"
  else
    pat="^${NAME_PREFIX}[A-Za-z0-9_-]+(-[0-9]+)?$"
  fi
  for pid in $(pgrep -f "$pat" 2>/dev/null || true); do
    printf '  pid %s  %s\n' "$pid" "$(ps -o command= -p "$pid" 2>/dev/null | awk '{print $1}')"
    found=$((found + 1))
  done
  printf 'loadgen: %s live generator(s)\n' "$found"
  ((found > 0))
}

case "${1:-}" in
  start) [[ $# -ge 3 ]] || usage; cmd_start "$2" "$3" "${4:-1}" ;;
  stop) [[ $# -ge 2 ]] || usage; cmd_stop "$2" ;;
  list) cmd_list "${2:-}" ;;
  *) usage ;;
esac
