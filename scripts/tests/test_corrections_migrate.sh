#!/usr/bin/env bash
# Tests for scripts/corrections-migrate-to-fno.sh
# Isolates via CLAUDE_DIR_OVERRIDE (legacy root) and FNO_HOME (new root) so
# the real ~/.claude and ~/.fno are never touched.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MIGRATE="$SCRIPT_DIR/../corrections-migrate-to-fno.sh"
PASS=0
FAIL=0

if [[ ! -f "$MIGRATE" ]]; then
    echo "FAIL: $MIGRATE not found - cannot run tests"
    exit 1
fi

pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

fixture() {
    local d
    d=$(mktemp -d)
    mkdir -p "$d/claude" "$d/fno"
    echo "$d"
}

run_migrate() {
    local d="$1"
    CLAUDE_DIR_OVERRIDE="$d/claude" FNO_HOME="$d/fno" bash "$MIGRATE"
}

# ---- T01: no legacy file -> no-op, new file never created ----
echo "T01: no legacy file"
D=$(fixture)
run_migrate "$D" >/dev/null 2>&1
RC=$?
if [[ $RC -eq 0 ]]; then pass "rc=0"; else fail "rc=$RC (expected 0)"; fi
if [[ ! -f "$D/fno/corrections.log" ]]; then pass "new file not created"; else fail "new file created from nothing"; fi
rm -rf "$D"

# ---- T02: old file present, new absent -> content moves, old tombstoned ----
echo "T02: fresh migration"
D=$(fixture)
printf 'line1\nline2\n' > "$D/claude/corrections.log"
run_migrate "$D" >/dev/null 2>&1
RC=$?
if [[ $RC -eq 0 ]]; then pass "rc=0"; else fail "rc=$RC (expected 0)"; fi
if [[ "$(cat "$D/fno/corrections.log")" == "$(printf 'line1\nline2')" ]]; then
    pass "new file has old content"
else
    fail "new file content mismatch: $(cat "$D/fno/corrections.log" 2>/dev/null)"
fi
if head -1 "$D/claude/corrections.log" | grep -q '^# migrated to '; then
    pass "old file tombstoned"
else
    fail "old file not tombstoned: $(cat "$D/claude/corrections.log")"
fi
if [[ "$(uname)" == "Darwin" ]]; then
    NEW_MODE=$(stat -f "%Lp" "$D/fno/corrections.log" 2>/dev/null)
else
    NEW_MODE=$(stat -c "%a" "$D/fno/corrections.log" 2>/dev/null)
fi
if [[ "$NEW_MODE" == "600" ]]; then
    pass "new file created at mode 600"
else
    fail "new file mode is $NEW_MODE, expected 600"
fi
rm -rf "$D"

# ---- T03: new file already has content -> old content appended, not clobbered ----
echo "T03: append onto existing new-location content"
D=$(fixture)
printf 'existing1\n' > "$D/fno/corrections.log"
printf 'legacy1\nlegacy2\n' > "$D/claude/corrections.log"
run_migrate "$D" >/dev/null 2>&1
RC=$?
if [[ $RC -eq 0 ]]; then pass "rc=0"; else fail "rc=$RC (expected 0)"; fi
EXPECTED=$(printf 'existing1\nlegacy1\nlegacy2')
if [[ "$(cat "$D/fno/corrections.log")" == "$EXPECTED" ]]; then
    pass "existing content preserved + legacy appended"
else
    fail "content mismatch: $(cat "$D/fno/corrections.log")"
fi
rm -rf "$D"

# ---- T04: idempotent re-run after tombstone is a no-op ----
echo "T04: re-run after tombstone"
D=$(fixture)
printf 'line1\n' > "$D/claude/corrections.log"
run_migrate "$D" >/dev/null 2>&1
FIRST_NEW=$(cat "$D/fno/corrections.log")
run_migrate "$D" >/dev/null 2>&1
RC=$?
if [[ $RC -eq 0 ]]; then pass "second run rc=0"; else fail "second run rc=$RC"; fi
if [[ "$(cat "$D/fno/corrections.log")" == "$FIRST_NEW" ]]; then
    pass "no duplicate content on re-run"
else
    fail "content duplicated on re-run: $(cat "$D/fno/corrections.log")"
fi
rm -rf "$D"

# ---- T05: corrections-rejected.log migrates independently ----
echo "T05: corrections-rejected.log"
D=$(fixture)
printf 'rejected1\n' > "$D/claude/corrections-rejected.log"
run_migrate "$D" >/dev/null 2>&1
if [[ "$(cat "$D/fno/corrections-rejected.log")" == "rejected1" ]]; then
    pass "rejected log migrated"
else
    fail "rejected log missing/mismatched"
fi
rm -rf "$D"

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
