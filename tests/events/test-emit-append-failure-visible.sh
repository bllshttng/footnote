#!/usr/bin/env bash
# tests/events/test-emit-append-failure-visible.sh
#
# A failed shell event append is visible: _append_bounded_event and both
# emitters say so on stderr and in their exit codes.
#
# Exit-code contract under test:
#   0  stored
#   3  skipped on purpose (the repo never opted in); silent, as today
#   1  lost, with one stderr line naming the label, the path and the reason
#
# The store runs behind FNO_BIN stubs, so no Rust build is needed and every
# refusal shape the binary can produce reduces to an exit code plus a stderr
# line.
#
# Run: bash tests/events/test-emit-append-failure-visible.sh

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

assert_has() {
    local label="$1" needle="$2" haystack="$3"
    if [[ "$haystack" != *"$needle"* ]]; then
        echo "FAIL $label: stderr is missing '$needle'"
        fail=1
    else
        echo "PASS $label"
    fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export TMPDIR="$tmp" FNO_TEST_HERMETIC=1

mkdir -p "$tmp/stub" "$tmp/j" "$tmp/home"

LINE='{"ts":"2026-09-21T00:00:00Z","type":"probe_kind","source":"hook","data":{}}'

# The store refuses with a reason and exit 1.
cat > "$tmp/stub/refusing" <<'EOF'
#!/usr/bin/env bash
cat >/dev/null
echo "error: store refused" >&2
exit 1
EOF

# The store exits 3 for its own reasons; the writer must never surface that
# as its own opt-out code.
cat > "$tmp/stub/exits3" <<'EOF'
#!/usr/bin/env bash
cat >/dev/null
exit 3
EOF

# The store accepts the envelope and keeps it for inspection.
cat > "$tmp/stub/accepting" <<'EOF'
#!/usr/bin/env bash
cat >>"${STUB_SINK:?STUB_SINK required}"
EOF
chmod +x "$tmp/stub/refusing" "$tmp/stub/exits3" "$tmp/stub/accepting"

# --- AC1: a refused append returns 1 and names itself on stderr --------------

stderr_file="$tmp/ac1.err"
rc=$(FNO_BIN="$tmp/stub/refusing" EVENTS_FILE="$tmp/j/events.jsonl" \
    bash -c '
        source "$1" >/dev/null 2>&1 || exit 1
        _append_bounded_event probe "$3" "$2"
        printf "%s" "$?"
    ' _ "$EVENTS_LIB" "$tmp/j/events.jsonl" "$LINE" 2>"$stderr_file")
assert_eq "a refused append returns 1" "1" "$rc"
assert_has "the refusal names the label" "probe" "$(cat "$stderr_file")"
assert_has "the refusal names the journal" "$tmp/j/events.jsonl" "$(cat "$stderr_file")"
assert_has "the refusal carries the store reason" "store refused" "$(cat "$stderr_file")"

# --- AC2: a store exit of 3 is a loss, never the opt-out ----------------------

stderr_file="$tmp/ac2.err"
rc=$(FNO_BIN="$tmp/stub/exits3" EVENTS_FILE="$tmp/j/events.jsonl" \
    bash -c '
        source "$1" >/dev/null 2>&1 || exit 1
        _append_bounded_event probe "$3" "$2"
        printf "%s" "$?"
    ' _ "$EVENTS_LIB" "$tmp/j/events.jsonl" "$LINE" 2>"$stderr_file")
assert_eq "a store exit 3 reads as a loss, never the opt-out" "1" "$rc"
assert_has "the exit-3 refusal names the failure" "refused the append" "$(cat "$stderr_file")"

# --- AC3: no fno binary anywhere is a reported loss ---------------------------

# Source a copy of the lib from a tree with no checkout build beside it, and
# keep fno off PATH, so the binary lookup falls through every rung.
mkdir -p "$tmp/lib/scripts/lib"
cp "$REPO_ROOT/scripts/lib/events.sh" "$REPO_ROOT/scripts/lib/events-lock.sh" \
    "$tmp/lib/scripts/lib/"

stderr_file="$tmp/ac3.err"
rc=$(env -u FNO_BIN PATH="/usr/bin:/bin" EVENTS_FILE="$tmp/j/events.jsonl" \
    bash -c '
        source "$1" >/dev/null 2>&1 || exit 1
        _append_bounded_event probe "$3" "$2"
        printf "%s" "$?"
    ' _ "$tmp/lib/scripts/lib/events.sh" "$tmp/j/events.jsonl" "$LINE" 2>"$stderr_file")
assert_eq "a missing fno binary returns 1" "1" "$rc"
assert_has "the missing-binary refusal says so" "no fno binary found" "$(cat "$stderr_file")"

# --- AC4: a healthy store appends through both emitters silently --------------

export STUB_SINK="$tmp/stub/sink"
: > "$STUB_SINK"
stderr_file="$tmp/ac4.err"
FNO_BIN="$tmp/stub/accepting" EVENTS_FILE="$tmp/j/events.jsonl" \
    bash -c '
        source "$1" >/dev/null 2>&1 || exit 1
        emit_event src t "{}"
        printf "emit_event=%s\n" "$?"
        emit_event_raw t "{}"
        printf "emit_event_raw=%s\n" "$?"
    ' _ "$EVENTS_LIB" >"$tmp/ac4.out" 2>"$stderr_file"
assert_eq "both emitters store on a healthy store" \
    "emit_event=0
emit_event_raw=0" "$(cat "$tmp/ac4.out")"
assert_eq "a healthy append prints nothing on stderr" "" "$(cat "$stderr_file")"
assert_has "the store received the envelope" '"type":"t"' "$(cat "$STUB_SINK")"

# --- AC5: a refused append through the emitters is a rc-1 loss ----------------

stderr_file="$tmp/ac5.err"
FNO_BIN="$tmp/stub/refusing" EVENTS_FILE="$tmp/j/events.jsonl" \
    bash -c '
        source "$1" >/dev/null 2>&1 || exit 1
        emit_event src t "{}"
        printf "emit_event=%s\n" "$?"
        emit_event_raw t "{}"
        printf "emit_event_raw=%s\n" "$?"
    ' _ "$EVENTS_LIB" >"$tmp/ac5.out" 2>"$stderr_file"
assert_eq "both emitters report a refused append" \
    "emit_event=1
emit_event_raw=1" "$(cat "$tmp/ac5.out")"
assert_has "emit_event names itself and the reason" \
    "emit_event: the event store refused" "$(cat "$stderr_file")"
assert_has "emit_event_raw names itself and the reason" \
    "emit_event_raw: the event store refused" "$(cat "$stderr_file")"
assert_has "the refusal carries the store reason" "store refused" "$(cat "$stderr_file")"

# --- AC6: a payload that is not valid JSON is a reported loss -----------------

stderr_file="$tmp/ac6.err"
FNO_BIN="$tmp/stub/accepting" EVENTS_FILE="$tmp/j/events.jsonl" \
    bash -c '
        source "$1" >/dev/null 2>&1 || exit 1
        emit_event src t "notjson"
        printf "emit_event=%s\n" "$?"
        emit_event_raw t "notjson"
        printf "emit_event_raw=%s\n" "$?"
    ' _ "$EVENTS_LIB" >"$tmp/ac6.out" 2>"$stderr_file"
assert_eq "a non-JSON payload is a rc-1 loss" \
    "emit_event=1
emit_event_raw=1" "$(cat "$tmp/ac6.out")"
assert_has "emit_event says the payload is not JSON" "not valid JSON" "$(cat "$stderr_file")"
assert_has "emit_event_raw says the payload is not JSON" "not valid JSON" "$(cat "$stderr_file")"

# --- AC7: a repo that never opted in stays silent -----------------------------

mkdir -p "$tmp/repo"
stderr_file="$tmp/ac7.err"
rc=$(FNO_BIN="$tmp/stub/accepting" EVENTS_FILE="$tmp/repo/.fno/events.jsonl" \
    bash -c '
        source "$1" >/dev/null 2>&1 || exit 1
        emit_event src t "{}"
        printf "%s" "$?"
    ' _ "$EVENTS_LIB" 2>"$stderr_file")
assert_eq "a repo without .fno skips with rc 3" "3" "$rc"
assert_eq "the opt-out stays silent" "" "$(cat "$stderr_file")"
assert_eq "and creates no .fno" "no" "$([[ -d "$tmp/repo/.fno" ]] && echo yes || echo no)"

if (( fail )); then
    echo "test-emit-append-failure-visible: FAIL"
    exit 1
fi
echo "test-emit-append-failure-visible: PASS"
