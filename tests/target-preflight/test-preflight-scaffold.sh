#!/usr/bin/env bash
# Test suite for target-preflight skill scaffold (Story 1)
# TDD: Tests for run-checks.sh scaffold - run to get RED before implementing

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RUN_CHECKS="$REPO_ROOT/skills/target-preflight/scripts/run-checks.sh"
CHECKS_DIR="$REPO_ROOT/skills/target-preflight/scripts/checks"

PASS=0
FAIL=0

pass() { echo "  PASS: $*"; ((PASS++)) || true; }
fail() { echo "  FAIL: $*"; ((FAIL++)) || true; }

TMPDIR_BASE="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_BASE"' EXIT

echo "=== Story 1: target-preflight scaffold tests ==="

# --- AC1-HP: run-checks.sh exists and is executable ---
echo ""
echo "--- AC1-HP: run-checks.sh exists ---"
if [[ -f "$RUN_CHECKS" ]]; then
    pass "AC1-HP: run-checks.sh exists"
else
    fail "AC1-HP: run-checks.sh missing at $RUN_CHECKS"
fi

# --- AC1-HP: run-checks.sh exits 0 in clean environment ---
echo ""
echo "--- AC1-HP: clean repo exit code ---"
# Create a clean tmp git repo to test against
CLEAN_REPO="$TMPDIR_BASE/clean-repo"
mkdir -p "$CLEAN_REPO"
cd "$CLEAN_REPO"
git init -q
git config user.email "test@test.com"
git config user.name "Test"
git checkout -q -b feature/test 2>/dev/null || git checkout -q -b main
touch README.md
git add README.md
git commit -q -m "init"
# Run in the clean repo
if EXIT_CODE=$(bash "$RUN_CHECKS" 2>&1; echo "EXIT:$?") && echo "$EXIT_CODE" | grep -q "EXIT:0"; then
    pass "AC1-HP: run-checks.sh exits 0 in clean repo"
else
    fail "AC1-HP: run-checks.sh should exit 0 in clean repo. Got: $EXIT_CODE"
fi

# --- AC2-ERR: dirty working tree causes exit 1 ---
echo ""
echo "--- AC2-ERR: dirty tree causes fail ---"
DIRTY_REPO="$TMPDIR_BASE/dirty-repo"
mkdir -p "$DIRTY_REPO"
cd "$DIRTY_REPO"
git init -q
git config user.email "test@test.com"
git config user.name "Test"
git checkout -q -b feature/test 2>/dev/null || git checkout -q -b main
touch README.md
git add README.md
git commit -q -m "init"
echo "dirty" > untracked.txt  # create untracked file

OUTPUT=$(bash "$RUN_CHECKS" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -ne 0 ]]; then
    pass "AC2-ERR: run-checks.sh exits non-zero on dirty tree"
else
    fail "AC2-ERR: run-checks.sh should exit non-zero on dirty tree (exit=$EXIT_CODE)"
fi

# Check working-tree-clean appears in output with fail glyph
if echo "$OUTPUT" | grep -q "✗"; then
    pass "AC2-ERR: output contains fail glyph ✗"
else
    fail "AC2-ERR: output should contain ✗ glyph. Got: $OUTPUT"
fi

# --- AC3-UI: output has per-check lines with glyphs ---
echo ""
echo "--- AC3-UI: human-scannable output ---"
cd "$CLEAN_REPO"
OUTPUT=$(bash "$RUN_CHECKS" 2>&1 || true)
if echo "$OUTPUT" | grep -qE "[✓✗⚠?]"; then
    pass "AC3-UI: output contains glyph characters"
else
    fail "AC3-UI: output should contain glyph characters (✓✗⚠?). Got: $OUTPUT"
fi

# --- AC3-UI: JSON summary on last line ---
LAST_LINE=$(echo "$OUTPUT" | tail -1)
if echo "$LAST_LINE" | grep -qE '^\{.*"passed"'; then
    pass "AC3-UI: last line is JSON summary with 'passed' key"
else
    fail "AC3-UI: last line should be JSON summary. Got: $LAST_LINE"
fi

# --- AC4-EDGE: non-executable check reports unknown, not failed ---
echo ""
echo "--- AC4-EDGE: non-executable check reports unknown ---"
# Create a check script that is not executable
NON_EXEC_DIR="$TMPDIR_BASE/test-checks"
mkdir -p "$NON_EXEC_DIR"
cat > "$NON_EXEC_DIR/bad-check.sh" << 'EOFCHECK'
#!/usr/bin/env bash
echo "bad-check pass this should not run"
EOFCHECK
# Intentionally NOT chmod +x

cd "$CLEAN_REPO"
# Run with a custom checks dir that has a non-executable file
OUTPUT=$(PREFLIGHT_CHECKS_DIR="$NON_EXEC_DIR" bash "$RUN_CHECKS" 2>&1 || true)
EXIT_CODE=$?
if echo "$OUTPUT" | grep -qE "bad-check.*unknown|[?].*bad-check"; then
    pass "AC4-EDGE: non-executable check reported as unknown"
else
    fail "AC4-EDGE: non-executable check should report unknown. Got: $OUTPUT"
fi
if [[ $EXIT_CODE -eq 0 ]]; then
    pass "AC4-EDGE: overall exit 0 when only unknown checks"
else
    fail "AC4-EDGE: should exit 0 when only unknown checks. Got exit=$EXIT_CODE"
fi

# --- FIX 3: broken check propagates first stderr line into report ---
echo ""
echo "--- FIX3: broken check stderr appears in output ---"
BROKEN_STDERR_DIR="$TMPDIR_BASE/test-broken-stderr"
mkdir -p "$BROKEN_STDERR_DIR"
cat > "$BROKEN_STDERR_DIR/broken-check.sh" << 'EOFCHECK'
#!/usr/bin/env bash
echo "broken-check: something went wrong here" >&2
exit 1
EOFCHECK
chmod +x "$BROKEN_STDERR_DIR/broken-check.sh"

cd "$CLEAN_REPO"
OUTPUT=$(PREFLIGHT_CHECKS_DIR="$BROKEN_STDERR_DIR" bash "$RUN_CHECKS" 2>&1 || true)
if echo "$OUTPUT" | grep -q "something went wrong here"; then
    pass "FIX3: first stderr line from broken check surfaced in run-checks output"
else
    fail "FIX3: broken check stderr not surfaced. Got: $OUTPUT"
fi

# --- FIX 4: blank stdout from check is classified as fail (not unknown) ---
echo ""
echo "--- FIX4: blank check output classified as fail ---"
BLANK_OUTPUT_DIR="$TMPDIR_BASE/test-blank-output"
mkdir -p "$BLANK_OUTPUT_DIR"
cat > "$BLANK_OUTPUT_DIR/blank-check.sh" << 'EOFCHECK'
#!/usr/bin/env bash
echo ""
EOFCHECK
chmod +x "$BLANK_OUTPUT_DIR/blank-check.sh"

cd "$CLEAN_REPO"
OUTPUT=$(PREFLIGHT_CHECKS_DIR="$BLANK_OUTPUT_DIR" bash "$RUN_CHECKS" 2>&1 || true)
if echo "$OUTPUT" | grep -qE "[✗].*blank-check|blank-check.*fail"; then
    pass "FIX4: blank check output classified as fail"
else
    fail "FIX4: blank check output not classified as fail. Got: $OUTPUT"
fi

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ $FAIL -eq 0 ]] || exit 1
