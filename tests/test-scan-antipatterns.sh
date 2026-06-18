#!/usr/bin/env bash
# Test suite for scan-antipatterns.sh
# TDD: Run BEFORE implementing scan-antipatterns.sh to get RED state

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# scan-antipatterns.sh lives in scripts/, not alongside this test (it was moved
# there; the old sibling path made every `bash "$SCAN"` exit 127 — ab-ac10ded5).
SCAN="$SCRIPT_DIR/../scripts/scan-antipatterns.sh"
PASS=0
FAIL=0

pass() { echo "  PASS: $*"; ((PASS++)) || true; }
fail() { echo "  FAIL: $*"; ((FAIL++)) || true; }

TMPDIR_TEST="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_TEST"' EXIT

# --- AC1: Detects TODO/FIXME/HACK/XXX ---
echo "--- AC1: TODO/FIXME/HACK/XXX Detection ---"

mkdir -p "$TMPDIR_TEST/ac1"
cat > "$TMPDIR_TEST/ac1/foo.ts" <<'EOF'
// TODO: fix this later
function doSomething() {
  return 42; // FIXME: wrong value
}
EOF

OUTPUT=$(bash "$SCAN" "$TMPDIR_TEST/ac1" 2>&1 || true)
EXIT_CODE=$(bash "$SCAN" "$TMPDIR_TEST/ac1" 2>&1; echo $?) || true
# just run and check exit
bash "$SCAN" "$TMPDIR_TEST/ac1" > /dev/null 2>&1 && SCAN_EXIT=0 || SCAN_EXIT=$?

if [[ $SCAN_EXIT -ne 0 ]]; then
    pass "AC1: Exits non-zero when TODO/FIXME found"
else
    fail "AC1: Should exit non-zero when TODO/FIXME found"
fi

OUTPUT=$(bash "$SCAN" "$TMPDIR_TEST/ac1" 2>&1 || true)
if echo "$OUTPUT" | grep -qE "TODO|FIXME"; then
    pass "AC1: Reports TODO/FIXME in output"
else
    fail "AC1: Should report TODO/FIXME in output"
fi

# --- AC2: Detects stub patterns ---
echo ""
echo "--- AC2: Stub Pattern Detection ---"

mkdir -p "$TMPDIR_TEST/ac2"
cat > "$TMPDIR_TEST/ac2/stub.ts" <<'EOF'
function getUser() {
  return null;
}
function getItems() {
  return [];
}
EOF

bash "$SCAN" "$TMPDIR_TEST/ac2" > /dev/null 2>&1 && SCAN_EXIT=0 || SCAN_EXIT=$?
if [[ $SCAN_EXIT -ne 0 ]]; then
    pass "AC2: Exits non-zero when stub returns found"
else
    fail "AC2: Should exit non-zero for stub returns"
fi

# --- AC3: Detects hardcoded localhost ---
echo ""
echo "--- AC3: Hardcoded localhost Detection ---"

mkdir -p "$TMPDIR_TEST/ac3"
cat > "$TMPDIR_TEST/ac3/config.ts" <<'EOF'
const API_URL = "http://localhost:3000/api";
EOF

bash "$SCAN" "$TMPDIR_TEST/ac3" > /dev/null 2>&1 && SCAN_EXIT=0 || SCAN_EXIT=$?
if [[ $SCAN_EXIT -ne 0 ]]; then
    pass "AC3: Exits non-zero when localhost URL found"
else
    fail "AC3: Should exit non-zero for localhost URL"
fi

OUTPUT=$(bash "$SCAN" "$TMPDIR_TEST/ac3" 2>&1 || true)
if echo "$OUTPUT" | grep -qiE "localhost|hardcoded"; then
    pass "AC3: Reports localhost in output"
else
    fail "AC3: Should mention localhost in output"
fi

# --- AC4: Actionable report format (file:line) ---
echo ""
echo "--- AC4: Report Format ---"

OUTPUT=$(bash "$SCAN" "$TMPDIR_TEST/ac1" 2>&1 || true)
# grep output format is "file:line:content"
if echo "$OUTPUT" | grep -qE "[^:]+:[0-9]+:"; then
    pass "AC4: Output includes file path and line number"
else
    fail "AC4: Output should include file:line format"
fi

# --- AC5: Clean directory exits 0 ---
echo ""
echo "--- AC5: Exit Codes ---"

mkdir -p "$TMPDIR_TEST/clean"
cat > "$TMPDIR_TEST/clean/good.ts" <<'EOF'
function add(a: number, b: number): number {
  return a + b;
}
export { add };
EOF

bash "$SCAN" "$TMPDIR_TEST/clean" > /dev/null 2>&1 && CLEAN_EXIT=0 || CLEAN_EXIT=$?
if [[ $CLEAN_EXIT -eq 0 ]]; then
    pass "AC5: Exits 0 for clean directory"
else
    fail "AC5: Should exit 0 for clean directory (got $CLEAN_EXIT)"
fi

# --- Summary ---
echo ""
echo "=== Test Results ==="
echo "Passed: $PASS | Failed: $FAIL"
[[ $FAIL -eq 0 ]] && { echo "ALL TESTS PASS"; exit 0; }
echo "TESTS FAILED"
exit 1
