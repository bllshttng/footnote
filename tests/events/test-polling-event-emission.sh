#!/usr/bin/env bash
# tests/events/test-polling-event-emission.sh
#
# Tests for emit_polling_external_review (Phase 1 task 1.1 of
# loop-correctness-sweep, plan ab-83be25ea). One assertion per case;
# sets fail=1 on any failure so every case runs before exit.
#
# Run: bash tests/events/test-polling-event-emission.sh

set -uo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
EVENTS_LIB="$REPO_ROOT/scripts/lib/events.sh"
VALIDATOR="$REPO_ROOT/scripts/lib/events-validate.sh"

if [[ ! -r "$EVENTS_LIB" ]]; then
    echo "FAIL: events lib not found at $EVENTS_LIB"
    exit 1
fi

# shellcheck disable=SC1090
source "$EVENTS_LIB"
if [[ -r "$VALIDATOR" ]]; then
    # shellcheck disable=SC1090
    source "$VALIDATOR"
fi

fail=0

assert_eq() {
    local label="$1" expected="$2" actual="$3"
    if [[ "$expected" != "$actual" ]]; then
        echo "FAIL $label: expected=$expected actual=$actual"
        fail=1
    fi
}

assert_contains() {
    local label="$1" haystack="$2" needle="$3"
    if [[ "$haystack" != *"$needle"* ]]; then
        echo "FAIL $label: missing '$needle' in: $haystack"
        fail=1
    fi
}

assert_not_contains() {
    local label="$1" haystack="$2" needle="$3"
    if [[ "$haystack" == *"$needle"* ]]; then
        echo "FAIL $label: unexpected '$needle' in: $haystack"
        fail=1
    fi
}

WORK=$(mktemp -d -t polling-emit-XXXXXX)
trap 'rm -rf "$WORK"' EXIT
export EVENTS_FILE="$WORK/events.jsonl"

# AC1-HP: Happy Path - all required fields, optional next_check_at
rm -f "$EVENTS_FILE"
out=$(emit_polling_external_review \
    pr_number=204 \
    reviewer_bot='gemini-code-assist[bot]' \
    wait_kind=cron \
    next_check_at=2026-05-08T16:00:00Z \
    session_id=s-abc 2>&1)
rc=$?
assert_eq "AC1-HP rc" 0 $rc
assert_eq "AC1-HP stderr empty" "" "$out"
[[ -f "$EVENTS_FILE" ]] || { echo "FAIL AC1-HP: events file not created"; fail=1; }
line=$(tail -1 "$EVENTS_FILE" 2>/dev/null)
assert_contains "AC1-HP type" "$line" '"type":"polling_external_review"'
assert_contains "AC1-HP pr_number" "$line" '"pr_number":204'
assert_contains "AC1-HP reviewer_bot" "$line" '"reviewer_bot":"gemini-code-assist[bot]"'
assert_contains "AC1-HP wait_kind" "$line" '"wait_kind":"cron"'
assert_contains "AC1-HP next_check_at" "$line" '"next_check_at":"2026-05-08T16:00:00Z"'
assert_contains "AC1-HP session_id" "$line" '"session_id":"s-abc"'
assert_contains "AC1-HP source target default" "$line" '"source":"target"'

# AC1-HP-2: inline wait_kind, no next_check_at, custom source via env
rm -f "$EVENTS_FILE"
out=$(EMIT_SOURCE_ID=hook emit_polling_external_review \
    pr_number=42 \
    reviewer_bot=somebot \
    wait_kind=inline \
    session_id=s-xyz 2>&1)
rc=$?
assert_eq "AC1-HP-2 rc" 0 $rc
line=$(tail -1 "$EVENTS_FILE" 2>/dev/null)
assert_contains "AC1-HP-2 wait_kind inline" "$line" '"wait_kind":"inline"'
assert_contains "AC1-HP-2 source override" "$line" '"source":"hook"'
assert_not_contains "AC1-HP-2 omits next_check_at" "$line" '"next_check_at"'

# AC2-ERR: missing pr_number
rm -f "$EVENTS_FILE"
out=$(emit_polling_external_review reviewer_bot=b wait_kind=cron session_id=s 2>&1)
rc=$?
assert_eq "AC2-ERR missing-pr rc" 1 $rc
assert_contains "AC2-ERR missing-pr msg" "$out" "missing pr_number"
[[ ! -s "$EVENTS_FILE" ]] || { echo "FAIL AC2-ERR missing-pr: events file written"; fail=1; }

# AC2-ERR: invalid wait_kind
rm -f "$EVENTS_FILE"
out=$(emit_polling_external_review pr_number=1 reviewer_bot=b wait_kind=bogus session_id=s 2>&1)
rc=$?
assert_eq "AC2-ERR bad-wait_kind rc" 1 $rc
assert_contains "AC2-ERR bad-wait_kind msg" "$out" "wait_kind"
[[ ! -s "$EVENTS_FILE" ]] || { echo "FAIL AC2-ERR bad-wait_kind: events file written"; fail=1; }

# AC2-ERR: missing reviewer_bot
rm -f "$EVENTS_FILE"
out=$(emit_polling_external_review pr_number=1 wait_kind=cron session_id=s 2>&1)
rc=$?
assert_eq "AC2-ERR missing-reviewer rc" 1 $rc
assert_contains "AC2-ERR missing-reviewer msg" "$out" "reviewer_bot"

# AC2-ERR: missing session_id
rm -f "$EVENTS_FILE"
out=$(emit_polling_external_review pr_number=1 reviewer_bot=b wait_kind=cron 2>&1)
rc=$?
assert_eq "AC2-ERR missing-sid rc" 1 $rc
assert_contains "AC2-ERR missing-sid msg" "$out" "session_id"

# AC2-ERR: unknown key surfaces
rm -f "$EVENTS_FILE"
out=$(emit_polling_external_review pr_number=1 reviewer_bot=b wait_kind=cron session_id=s extra=junk 2>&1)
rc=$?
assert_eq "AC2-ERR unknown-key rc" 1 $rc
assert_contains "AC2-ERR unknown-key msg" "$out" "unknown key"

# AC4-EDGE: concurrent emissions both land
rm -f "$EVENTS_FILE"
(
    emit_polling_external_review pr_number=1 reviewer_bot=a wait_kind=cron session_id=s &
    emit_polling_external_review pr_number=2 reviewer_bot=b wait_kind=inline session_id=s &
    wait
)
count=$(grep -c '"type":"polling_external_review"' "$EVENTS_FILE" 2>/dev/null || echo 0)
assert_eq "AC4-EDGE concurrent count" 2 $count
# Each line must be a single valid JSON object (no interleaving)
while IFS= read -r line; do
    if ! jq -e . <<<"$line" >/dev/null 2>&1; then
        echo "FAIL AC4-EDGE: corrupted line: $line"
        fail=1
    fi
done < "$EVENTS_FILE"

# AC-VALIDATOR: validator accepts canonical envelope (when validator is loadable)
if declare -F validate_event >/dev/null 2>&1; then
    canonical='{"ts":"2026-05-07T09:30:42Z","type":"polling_external_review","source":"target","data":{"pr_number":204,"reviewer_bot":"gemini-code-assist[bot]","wait_kind":"cron","session_id":"s","next_check_at":"2026-05-08T16:00:00Z"}}'
    if ! validate_event polling_external_review "$canonical" 2>&1; then
        echo "FAIL AC-VALIDATOR: validator rejected canonical event"
        fail=1
    fi
    # Reject missing wait_kind
    bad='{"ts":"2026-05-07T09:30:42Z","type":"polling_external_review","source":"target","data":{"pr_number":1,"reviewer_bot":"b","session_id":"s"}}'
    out=$(validate_event polling_external_review "$bad" 2>&1)
    rc=$?
    assert_eq "AC-VALIDATOR missing-wait_kind rc" 1 $rc
fi

if (( fail == 0 )); then
    echo "PASS test-polling-event-emission.sh"
    exit 0
else
    echo "FAIL test-polling-event-emission.sh"
    exit 1
fi
