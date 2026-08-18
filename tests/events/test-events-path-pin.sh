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
#    and leaves the checkout journal byte-identical. Asserted in both directions,
#    because the pinned-journal half is the positive control - an emit that
#    silently did nothing would satisfy the untouched-checkout half alone.
checkout_journal="$REPO_ROOT/.fno/events.jsonl"
before_bytes=0
[[ -f "$checkout_journal" ]] && before_bytes=$(wc -c < "$checkout_journal")

env -u EVENTS_FILE FNO_EVENTS_PATH="$tmp/emit.jsonl" bash -c \
    'cd "$2" || exit 1; source "$1" >/dev/null 2>&1; emit_event target test_events_path_pin "{}"' \
    _ "$EVENTS_LIB" "$REPO_ROOT" >/dev/null 2>&1

pinned_rows=0
if [[ -f "$tmp/emit.jsonl" ]]; then
    pinned_rows=$(grep -c 'test_events_path_pin' "$tmp/emit.jsonl" 2>/dev/null)
    [[ -z "$pinned_rows" ]] && pinned_rows=0
fi
assert_eq "the emit reached the pinned journal" "1" "$pinned_rows"

after_bytes=0
[[ -f "$checkout_journal" ]] && after_bytes=$(wc -c < "$checkout_journal")
assert_eq "the checkout journal is byte-identical" "$before_bytes" "$after_bytes"

if [[ $fail -eq 0 ]]; then
    echo "PASS test-events-path-pin.sh"
    exit 0
fi
echo "FAIL test-events-path-pin.sh"
exit 1
