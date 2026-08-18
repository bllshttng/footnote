#!/usr/bin/env bash
# tests/events/test-events-path-pin.sh
#
# The shell half of the journal pin. `paths.project_events_json()` honors
# FNO_EVENTS_PATH on the Python side; this asserts scripts/lib/events.sh reads
# the same var, so the containment is not a guard sitting on one of the writers.
#
# Why the pin exists: fno.hermetic.neutralise deliberately leaves FNO_REPO_ROOT
# unset, so a shell test running inside the checkout resolves the real repo and
# appends a production-shaped row to the journal the operator needs panel folds.
#
# Run: bash tests/events/test-events-path-pin.sh

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

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# 1. The pin resolves the journal when no explicit EVENTS_FILE is set.
got=$(env -u EVENTS_FILE FNO_EVENTS_PATH="$tmp/pinned.jsonl" bash -c \
    'source "$1" >/dev/null 2>&1; printf "%s" "$EVENTS_FILE"' _ "$EVENTS_LIB")
assert_eq "pin resolves EVENTS_FILE" "$tmp/pinned.jsonl" "$got"

# 2. An explicit EVENTS_FILE still outranks the pin, matching the Python side
#    where an explicit events_path= outranks it too.
got=$(EVENTS_FILE="$tmp/explicit.jsonl" FNO_EVENTS_PATH="$tmp/pinned.jsonl" bash -c \
    'source "$1" >/dev/null 2>&1; printf "%s" "$EVENTS_FILE"' _ "$EVENTS_LIB")
assert_eq "explicit EVENTS_FILE outranks the pin" "$tmp/explicit.jsonl" "$got"

# 3. The containment itself: an emit from inside the checkout lands on the pin
#    and never reaches the checkout journal. Asserted in both directions,
#    because the pinned-journal half is the positive control - an emit that
#    silently did nothing would satisfy the untouched-checkout half alone.
#
#    Counted by this test's own marker rather than by bytes. In a worktree that
#    journal is a symlink to the canonical file, which live sessions and daemons
#    append to while this runs, so a byte comparison goes red on somebody else's
#    write. Only this emit can move the marker count.
MARKER=test_events_path_pin
checkout_journal="$REPO_ROOT/.fno/events.jsonl"
count_marker() {
    [[ -f "$1" ]] || { echo 0; return; }
    grep -c "$MARKER" "$1" 2>/dev/null || echo 0
}
before_marks=$(count_marker "$checkout_journal")

env -u EVENTS_FILE FNO_EVENTS_PATH="$tmp/emit.jsonl" bash -c \
    'cd "$2" || exit 1; source "$1" >/dev/null 2>&1; emit_event target test_events_path_pin "{}"' \
    _ "$EVENTS_LIB" "$REPO_ROOT" >/dev/null 2>&1

assert_eq "the emit reached the pinned journal" "1" "$(count_marker "$tmp/emit.jsonl")"

after_marks=$(count_marker "$checkout_journal")
assert_eq "the checkout journal gained no row from this emit" "$before_marks" "$after_marks"

if [[ $fail -eq 0 ]]; then
    echo "PASS test-events-path-pin.sh"
    exit 0
fi
echo "FAIL test-events-path-pin.sh"
exit 1
