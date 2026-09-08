#!/usr/bin/env bash
# tests/events/test-shell-append-guards.sh
#
# Two guards on the one shell append primitive, both asserted by a marker the
# outcome produces rather than by an absence.
#
# 1. A hook must not materialise a project `.fno/` in a repo that never opted
#    in. The positive control is the GLOBAL row: a run that wrote nothing at
#    all would satisfy "no .fno was created" and prove nothing.
# 2. A hermetic process must not append to a journal outside its sandbox. The
#    positive control is the refusal message on stderr plus a same-shape write
#    inside the sandbox that still succeeds.
#
# Run: bash tests/events/test-shell-append-guards.sh

set -uo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
EVENTS_LIB="$REPO_ROOT/scripts/lib/events.sh"

fail=0

assert_eq() {
    local label="$1" expected="$2" actual="$3"
    if [[ "$expected" != "$actual" ]]; then
        echo "FAIL $label: expected=$expected actual=$actual"
        fail=1
    else
        echo "PASS $label"
    fi
}

# `grep -c` PRINTS 0 and EXITS 1 on no match, so a `|| echo 0` fallback fires on
# top of grep's own output and the count comes back as two lines. Read grep's
# stdout and substitute 0 only when the file is missing entirely.
count_marker() {
    local n
    n=$(grep -c context_nudge "$1" 2>/dev/null)
    [[ -n "$n" ]] || n=0
    printf '%s' "$n"
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

LINE='{"ts":"2026-09-07T00:00:00Z","type":"context_nudge","source":"hook","data":{}}'

# --- 1. opt-in local journal -------------------------------------------------

fresh="$tmp/fresh-repo"
home="$tmp/home"
mkdir -p "$fresh" "$home/.fno"

env -u EVENTS_FILE -u FNO_TEST_HERMETIC HOME="$home" STATE_DIR="$home/.fno" \
    FNO_EVENTS_PATH="$home/.fno/events.jsonl" bash -c '
        source "$1" >/dev/null 2>&1 || exit 1
        _append_bounded_event probe "$3" "$2/.fno/events.jsonl" || true
        _append_bounded_event probe "$3" "$HOME/.fno/events.jsonl" || true
    ' _ "$EVENTS_LIB" "$fresh" "$LINE" >/dev/null 2>&1

assert_eq "no .fno in a repo that never opted in" \
    "no" "$([[ -d "$fresh/.fno" ]] && echo yes || echo no)"
assert_eq "global journal still carries the row" \
    "1" "$(count_marker "$home/.fno/events.jsonl")"

# An opted-in project keeps today's dual write. This is the control that
# proves the guard discriminates rather than disabling the local journal.
opted="$tmp/opted-repo"
mkdir -p "$opted/.fno"
env -u EVENTS_FILE -u FNO_TEST_HERMETIC HOME="$home" STATE_DIR="$home/.fno" \
    FNO_EVENTS_PATH="$home/.fno/events.jsonl" bash -c '
        source "$1" >/dev/null 2>&1 || exit 1
        _append_bounded_event probe "$3" "$2/.fno/events.jsonl" || true
    ' _ "$EVENTS_LIB" "$opted" "$LINE" >/dev/null 2>&1

assert_eq "an opted-in project still gets its local row" \
    "1" "$(count_marker "$opted/.fno/events.jsonl")"

# --- 2. hermetic escape ------------------------------------------------------

# A journal outside TMPDIR and outside the pin, written by a process that
# declares a sandbox. `outside` sits under $HOME, which is deliberately not an
# allowed root on either side of the fence.
outside_home="$tmp/outside-home"
mkdir -p "$outside_home/live/.fno"
: > "$outside_home/live/.fno/events.jsonl"

stderr_file="$tmp/refusal.txt"
env -u EVENTS_FILE HOME="$outside_home" TMPDIR="$tmp/sandbox" \
    FNO_TEST_HERMETIC=1 FNO_EVENTS_PATH="$tmp/sandbox/events.jsonl" bash -c '
        mkdir -p "$TMPDIR"
        source "$1" >/dev/null 2>&1 || exit 1
        _append_bounded_event probe "$3" "$2" || true
    ' _ "$EVENTS_LIB" "$outside_home/live/.fno/events.jsonl" "$LINE" \
    2>"$stderr_file" >/dev/null

assert_eq "hermetic write outside the sandbox lands nothing" \
    "0" "$(count_marker "$outside_home/live/.fno/events.jsonl")"
assert_eq "the refusal names itself on stderr" \
    "1" "$(grep -c 'refused a journal write outside the test sandbox' "$stderr_file" 2>/dev/null | head -1)"

# Positive control for the fence: the same call inside the sandbox writes.
env -u EVENTS_FILE HOME="$outside_home" TMPDIR="$tmp/sandbox" \
    FNO_TEST_HERMETIC=1 FNO_EVENTS_PATH="$tmp/sandbox/events.jsonl" bash -c '
        mkdir -p "$TMPDIR"
        source "$1" >/dev/null 2>&1 || exit 1
        _append_bounded_event probe "$3" "$2" || true
    ' _ "$EVENTS_LIB" "$tmp/sandbox/events.jsonl" "$LINE" >/dev/null 2>&1

assert_eq "the same write inside the sandbox still lands" \
    "1" "$(count_marker "$tmp/sandbox/events.jsonl")"

# A symlinked leaf is judged on its physical target, which is the 2026-08-20
# mechanism: a worktree journal pointing at the canonical one.
mkdir -p "$tmp/sandbox/worktree"
ln -sf "$outside_home/live/.fno/events.jsonl" "$tmp/sandbox/worktree/events.jsonl"
env -u EVENTS_FILE HOME="$outside_home" TMPDIR="$tmp/sandbox" \
    FNO_TEST_HERMETIC=1 FNO_EVENTS_PATH="$tmp/sandbox/events.jsonl" bash -c '
        source "$1" >/dev/null 2>&1 || exit 1
        _append_bounded_event probe "$3" "$2" || true
    ' _ "$EVENTS_LIB" "$tmp/sandbox/worktree/events.jsonl" "$LINE" >/dev/null 2>&1

assert_eq "a symlink into a live journal is refused on its target" \
    "0" "$(count_marker "$outside_home/live/.fno/events.jsonl")"

# The ancestor is the symlink and the leaf directory does not exist yet, which
# is the real shape: a whole-directory `.fno` link into canonical plus a space
# subdirectory nothing has created. `cd` cannot reach a directory that is not
# there, so a fence that resolves only the immediate parent reads the lexical
# path and lets the write through.
mkdir -p "$outside_home/escape"
ln -sfn "$outside_home/escape" "$tmp/sandbox/wt"
env -u EVENTS_FILE HOME="$outside_home" TMPDIR="$tmp/sandbox" \
    FNO_TEST_HERMETIC=1 FNO_EVENTS_PATH="$tmp/sandbox/events.jsonl" bash -c '
        source "$1" >/dev/null 2>&1 || exit 1
        _append_bounded_event probe "$3" "$2" || true
    ' _ "$EVENTS_LIB" "$tmp/sandbox/wt/spaces/proj/events.jsonl" "$LINE" >/dev/null 2>&1

assert_eq "a symlinked ancestor with a missing leaf is refused too" \
    "0" "$(find "$outside_home/escape" -type f 2>/dev/null | wc -l | tr -d ' ')"

# A skipped append must not report success. Every caller reads 0 as "the line
# is on disk", and one of them counts appended lines.
skipped="$tmp/never-opted/.fno/events.jsonl"
mkdir -p "$tmp/never-opted"
rc=$(env -u EVENTS_FILE -u FNO_TEST_HERMETIC HOME="$home" bash -c '
        source "$1" >/dev/null 2>&1 || exit 1
        _append_bounded_event probe "$3" "$2" >/dev/null 2>&1
        printf "%s" "$?"
    ' _ "$EVENTS_LIB" "$skipped" "$LINE")
assert_eq "a skipped append returns 3, not 0" "3" "$rc"

if (( fail )); then
    echo "test-shell-append-guards: FAIL"
    exit 1
fi
echo "test-shell-append-guards: PASS"
