#!/usr/bin/env bash
# tests/events/test-polling-event-emission.sh
#
# Tests for emit_polling_external_review (Phase 1 task 1.1 of
# loop-correctness-sweep, plan ab-83be25ea). One assertion per case;
# sets fail=1 on any failure so every case runs before exit.
#
# The store commit is the write boundary: an emitted event leaves no byte
# trace in the journal file, so every emission assertion reads COMMITTED ROWS
# through `doctor event rows`. The GC-marker and writer-rendezvous cases that
# used to live here tested the shell mutex that the native store retired; the
# SQL transaction is the serialization point now.
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

# Same resolution order as the lib's writer: FNO_BIN, then the checkout build,
# then PATH, so assertions never read through a stale installed binary.
ROWS_BIN="${FNO_BIN:-}"
if [[ -z "$ROWS_BIN" ]]; then
    for _profile in debug release; do
        if [[ -x "$REPO_ROOT/crates/fno/target/$_profile/fno" ]]; then
            ROWS_BIN="$REPO_ROOT/crates/fno/target/$_profile/fno"
            break
        fi
    done
fi
[[ -n "$ROWS_BIN" ]] || ROWS_BIN=$(command -v fno 2>/dev/null)

# The committed rows of one journal as a JSON array of envelope lines.
committed_rows() {
    [[ -n "$ROWS_BIN" ]] || { printf '[]'; return; }
    "$ROWS_BIN" doctor event rows --events "$1" 2>/dev/null || printf '[]'
}

# A fresh history for a case: the store dedupes and accumulates across
# re-seeds of the raw file, so a scenario must unlink the store db too.
fresh_history() {
    rm -f "$EVENTS_FILE" "${EVENTS_FILE%.jsonl}.db"
}

WORK=$(mktemp -d -t polling-emit-XXXXXX)
trap 'rm -rf "$WORK"' EXIT
export EVENTS_FILE="$WORK/events.jsonl"

# AC1-HP: Happy Path - all required fields, optional next_check_at
fresh_history
out=$(emit_polling_external_review \
    pr_number=204 \
    reviewer_bot='gemini-code-assist[bot]' \
    wait_kind=cron \
    next_check_at=2026-05-08T16:00:00Z \
    session_id=s-abc 2>&1)
rc=$?
assert_eq "AC1-HP rc" 0 $rc
assert_eq "AC1-HP stderr empty" "" "$out"
rows=$(committed_rows "$EVENTS_FILE")
line=$(printf '%s' "$rows" | jq -r '.[-1] // empty')
assert_contains "AC1-HP type" "$line" '"type":"polling_external_review"'
assert_contains "AC1-HP pr_number" "$line" '"pr_number":204'
assert_contains "AC1-HP reviewer_bot" "$line" '"reviewer_bot":"gemini-code-assist[bot]"'
assert_contains "AC1-HP wait_kind" "$line" '"wait_kind":"cron"'
assert_contains "AC1-HP next_check_at" "$line" '"next_check_at":"2026-05-08T16:00:00Z"'
assert_contains "AC1-HP session_id" "$line" '"session_id":"s-abc"'
assert_contains "AC1-HP source target default" "$line" '"source":"target"'

# AC1-HP-2: inline wait_kind, no next_check_at, custom source via env
fresh_history
out=$(EMIT_SOURCE_ID=hook emit_polling_external_review \
    pr_number=42 \
    reviewer_bot=somebot \
    wait_kind=inline \
    session_id=s-xyz 2>&1)
rc=$?
assert_eq "AC1-HP-2 rc" 0 $rc
rows=$(committed_rows "$EVENTS_FILE")
line=$(printf '%s' "$rows" | jq -r '.[-1] // empty')
assert_contains "AC1-HP-2 wait_kind inline" "$line" '"wait_kind":"inline"'
assert_contains "AC1-HP-2 source override" "$line" '"source":"hook"'
assert_not_contains "AC1-HP-2 omits next_check_at" "$line" '"next_check_at"'

# AC2-ERR: missing pr_number
fresh_history
out=$(emit_polling_external_review reviewer_bot=b wait_kind=cron session_id=s 2>&1)
rc=$?
assert_eq "AC2-ERR missing-pr rc" 1 $rc
assert_contains "AC2-ERR missing-pr msg" "$out" "missing pr_number"
assert_eq "AC2-ERR missing-pr rows" "0" "$(printf '%s' "$(committed_rows "$EVENTS_FILE")" | jq 'length')"

# AC2-ERR: invalid wait_kind
fresh_history
out=$(emit_polling_external_review pr_number=1 reviewer_bot=b wait_kind=bogus session_id=s 2>&1)
rc=$?
assert_eq "AC2-ERR bad-wait_kind rc" 1 $rc
assert_contains "AC2-ERR bad-wait_kind msg" "$out" "wait_kind"
assert_eq "AC2-ERR bad-wait_kind rows" "0" "$(printf '%s' "$(committed_rows "$EVENTS_FILE")" | jq 'length')"

# AC2-ERR: missing reviewer_bot
fresh_history
out=$(emit_polling_external_review pr_number=1 wait_kind=cron session_id=s 2>&1)
rc=$?
assert_eq "AC2-ERR missing-reviewer rc" 1 $rc
assert_contains "AC2-ERR missing-reviewer msg" "$out" "reviewer_bot"

# AC2-ERR: missing session_id
fresh_history
out=$(emit_polling_external_review pr_number=1 reviewer_bot=b wait_kind=cron 2>&1)
rc=$?
assert_eq "AC2-ERR missing-sid rc" 1 $rc
assert_contains "AC2-ERR missing-sid msg" "$out" "session_id"

# AC2-ERR: unknown key surfaces
fresh_history
out=$(emit_polling_external_review pr_number=1 reviewer_bot=b wait_kind=cron session_id=s extra=junk 2>&1)
rc=$?
assert_eq "AC2-ERR unknown-key rc" 1 $rc
assert_contains "AC2-ERR unknown-key msg" "$out" "unknown key"

# AC4-EDGE: concurrent emissions both land. Two shell writers racing one
# store is the small version of what the SQL transaction exists to serialize.
fresh_history
(
    emit_polling_external_review pr_number=1 reviewer_bot=a wait_kind=cron session_id=s &
    emit_polling_external_review pr_number=2 reviewer_bot=b wait_kind=inline session_id=s &
    wait
)
rows=$(committed_rows "$EVENTS_FILE")
assert_eq "AC4-EDGE concurrent count" 2 "$(printf '%s' "$rows" | jq 'length')"
# Each committed row must be a single valid JSON object (no interleaving)
while IFS= read -r line; do
    if ! jq -e . <<<"$line" >/dev/null 2>&1; then
        echo "FAIL AC4-EDGE: corrupted row: $line"
        fail=1
    fi
done < <(printf '%s' "$rows" | jq -r '.[]')

# Shell writers must keep every commit below the atomic line bound; a refusal
# happens before the store sees anything.
large_value=$(printf '%05000d' 0)

fresh_history
emit_event target size_probe "$(jq -nc --arg value "$large_value" '{value: $value}')" 2>/dev/null
assert_eq "size cap emit_event rows" "0" "$(printf '%s' "$(committed_rows "$EVENTS_FILE")" | jq 'length')"

fresh_history
emit_event_raw size_probe "$(jq -nc --arg value "$large_value" '{value: $value}')" 2>/dev/null
assert_eq "size cap emit_event_raw rows" "0" "$(printf '%s' "$(committed_rows "$EVENTS_FILE")" | jq 'length')"

fresh_history
out=$(emit_polling_external_review \
    pr_number=1 \
    reviewer_bot="$large_value" \
    wait_kind=inline \
    session_id=s 2>&1)
rc=$?
assert_eq "size cap polling rc" 2 "$rc"
assert_contains "size cap polling message" "$out" "exceeds"
assert_eq "size cap polling rows" "0" "$(printf '%s' "$(committed_rows "$EVENTS_FILE")" | jq 'length')"

# Default paths resolve from the worktree root even when sourced below it.
# This case is about the ROOT branch, so the FNO_EVENTS_PATH pin has to be out
# of the way: the hermetic sandbox sets it for the whole run and the library
# checks it ahead of the root, by design. The pin's own coverage is in
# tests/events/test-events-path-pin.sh.
#
# A project `.fno/` is the opt-in marker, so the root branch is asserted in
# both directions: the emit creates nothing in a repo that never opted in, and
# the same emit commits once the directory is there. Without the second half a
# green would also be satisfied by an emit that did nothing at all.
repo_root="$WORK/repo"
mkdir -p "$repo_root/nested/source"
git -C "$repo_root" init -q
# A stub shadows fno-agents so the case under test is the same one every time:
# the degrade the library documents for a context that cannot ask the resolver.
# With the real binary present the resolver answers the space journal and this
# assertion stops being about the root branch at all, which is what a developer
# box was measuring while CI measured the degrade. Only that one variable is
# isolated: narrowing PATH instead would also hide jq and git, and emit_event
# swallows a missing jq, so the failure would name the wrong branch.
stub_dir="$WORK/stub"
mkdir -p "$stub_dir"
printf '#!/usr/bin/env bash\nexit 1\n' >"$stub_dir/fno-agents"
chmod +x "$stub_dir/fno-agents"
emit_from_nested() {
    (
        unset EVENTS_FILE
        unset FNO_EVENTS_PATH
        export PATH="$stub_dir:$PATH"
        cd "$repo_root/nested/source" || exit 1
        # shellcheck disable=SC1090
        source "$EVENTS_LIB"
        emit_event target root_probe '{}'
    )
}
emit_from_nested
if [[ -d "$repo_root/.fno" ]]; then
    echo "FAIL repo root: emit created a .fno in a repo that never opted in"
    fail=1
fi
mkdir -p "$repo_root/.fno"
emit_from_nested
root_rows=$(committed_rows "$repo_root/.fno/events.jsonl")
assert_eq "repo root: shell event committed at root" "1" "$(printf '%s' "$root_rows" | jq '[.[] | fromjson | select(.type == "root_probe")] | length')"
[[ ! -e "$repo_root/nested/source/.fno" ]] || { echo "FAIL repo root: shell event created a nested .fno"; fail=1; }

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
