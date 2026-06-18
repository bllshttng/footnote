#!/usr/bin/env bash
# Test suite for target-preflight canonical check scripts (Story 2)
# TDD: Tests for individual checks - run to get RED before implementing

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
CHECKS_DIR="$REPO_ROOT/skills/target-preflight/scripts/checks"

PASS=0
FAIL=0

pass() { echo "  PASS: $*"; ((PASS++)) || true; }
fail() { echo "  FAIL: $*"; ((FAIL++)) || true; }

TMPDIR_BASE="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_BASE"' EXIT

echo "=== Story 2: Canonical check scripts ==="

# --- Helper: run a check and capture output + exit code ---
run_check() {
    local check_name="$1"
    local check_path="$CHECKS_DIR/${check_name}.sh"
    if [[ ! -f "$check_path" ]]; then
        echo "MISSING_CHECK"
        return 1
    fi
    bash "$check_path" 2>&1
    return 0
}

# --- AC1-HP: Each check runs in under 2 seconds ---
echo ""
echo "--- AC1-HP: All checks run quickly ---"
for check in working-tree-clean branch-state deps-installed test-suite-green codemap-fresh auth-valid disk-space; do
    if [[ ! -f "$CHECKS_DIR/${check}.sh" ]]; then
        fail "AC1-HP: ${check}.sh missing"
        continue
    fi
    START=$(python3 -c "import time; print(int(time.time()*1000))")
    bash "$CHECKS_DIR/${check}.sh" > /dev/null 2>&1
    END=$(python3 -c "import time; print(int(time.time()*1000))")
    ELAPSED=$((END - START))
    if [[ $ELAPSED -lt 2000 ]]; then
        pass "AC1-HP: $check ran in ${ELAPSED}ms (<2s)"
    else
        fail "AC1-HP: $check took ${ELAPSED}ms (>2s budget)"
    fi
done

# --- Contract: each check always exits 0 ---
echo ""
echo "--- Contract: each check exits 0 regardless of result ---"
for check in working-tree-clean branch-state deps-installed test-suite-green codemap-fresh auth-valid disk-space; do
    if [[ ! -f "$CHECKS_DIR/${check}.sh" ]]; then
        fail "Contract: ${check}.sh missing"
        continue
    fi
    bash "$CHECKS_DIR/${check}.sh" > /dev/null 2>&1
    EC=$?
    if [[ $EC -eq 0 ]]; then
        pass "Contract: $check exits 0"
    else
        fail "Contract: $check exited $EC (must always exit 0)"
    fi
done

# --- Contract: each check outputs exactly one line in format: name status message ---
echo ""
echo "--- Contract: each check outputs one valid line ---"
for check in working-tree-clean branch-state deps-installed test-suite-green codemap-fresh auth-valid disk-space; do
    if [[ ! -f "$CHECKS_DIR/${check}.sh" ]]; then
        fail "Contract: ${check}.sh missing"
        continue
    fi
    OUTPUT=$(bash "$CHECKS_DIR/${check}.sh" 2>/dev/null)
    LINE_COUNT=$(echo "$OUTPUT" | grep -c . || true)
    if [[ $LINE_COUNT -eq 1 ]]; then
        pass "Contract: $check outputs exactly one line"
    else
        fail "Contract: $check should output one line, got $LINE_COUNT lines. Output: $OUTPUT"
    fi
    # Check format: name status message
    if echo "$OUTPUT" | grep -qE "^[a-z-]+ (pass|fail|warn|unknown) "; then
        pass "Contract: $check output format is 'name status message'"
    else
        fail "Contract: $check output format invalid. Got: $OUTPUT"
    fi
done

# --- working-tree-clean specific tests ---
echo ""
echo "--- working-tree-clean check tests ---"

# Create clean git repo
CLEAN_REPO="$TMPDIR_BASE/clean"
mkdir -p "$CLEAN_REPO"
cd "$CLEAN_REPO"
git init -q
git config user.email "test@test.com"
git config user.name "Test"
git checkout -q -b feature/test 2>/dev/null || git checkout -q -b main
touch README.md
git add README.md
git commit -q -m "init"

OUTPUT=$(bash "$CHECKS_DIR/working-tree-clean.sh" 2>/dev/null)
if echo "$OUTPUT" | grep -q "^working-tree-clean pass"; then
    pass "working-tree-clean: pass on clean repo"
else
    fail "working-tree-clean: should pass on clean repo. Got: $OUTPUT"
fi

# Create dirty repo
DIRTY_REPO="$TMPDIR_BASE/dirty"
mkdir -p "$DIRTY_REPO"
cd "$DIRTY_REPO"
git init -q
git config user.email "test@test.com"
git config user.name "Test"
git checkout -q -b feature/test 2>/dev/null || git checkout -q -b main
touch README.md
git add README.md
git commit -q -m "init"
echo "dirty" > untracked.txt

OUTPUT=$(bash "$CHECKS_DIR/working-tree-clean.sh" 2>/dev/null)
if echo "$OUTPUT" | grep -q "^working-tree-clean fail"; then
    pass "working-tree-clean: fail on dirty repo"
else
    fail "working-tree-clean: should fail on dirty repo. Got: $OUTPUT"
fi

# AC3-UI: fail message includes context (file names)
if echo "$OUTPUT" | grep -q "untracked.txt"; then
    pass "AC3-UI: working-tree-clean fail message includes file name"
else
    fail "AC3-UI: working-tree-clean fail should include file names. Got: $OUTPUT"
fi

# AC4-EDGE: allowlist support
cd "$DIRTY_REPO"
mkdir -p .fno
echo "untracked.txt" > .fno/preflight-ignore.txt
OUTPUT=$(bash "$CHECKS_DIR/working-tree-clean.sh" 2>/dev/null)
if echo "$OUTPUT" | grep -q "^working-tree-clean pass"; then
    pass "AC4-EDGE: working-tree-clean passes when file is in allowlist"
else
    fail "AC4-EDGE: allowlisted file should not cause fail. Got: $OUTPUT"
fi
rm -f .fno/preflight-ignore.txt

# --- branch-state specific tests ---
echo ""
echo "--- branch-state check tests ---"

# feature branch should pass
cd "$CLEAN_REPO"
OUTPUT=$(bash "$CHECKS_DIR/branch-state.sh" 2>/dev/null)
if echo "$OUTPUT" | grep -qE "^branch-state (pass|warn)"; then
    pass "branch-state: pass/warn on feature branch"
else
    fail "branch-state: should pass/warn on feature branch. Got: $OUTPUT"
fi

# main branch should fail
MAIN_REPO="$TMPDIR_BASE/main-branch"
mkdir -p "$MAIN_REPO"
cd "$MAIN_REPO"
git init -q
git config user.email "test@test.com"
git config user.name "Test"
# Use 'main' branch
git checkout -q -b main 2>/dev/null || true
touch README.md
git add README.md
git commit -q -m "init"

OUTPUT=$(bash "$CHECKS_DIR/branch-state.sh" 2>/dev/null)
if echo "$OUTPUT" | grep -q "^branch-state fail"; then
    pass "branch-state: fail on main branch"
else
    fail "branch-state: should fail on main branch. Got: $OUTPUT"
fi

# --- test-suite-green specific tests (opt-in) ---
echo ""
echo "--- test-suite-green check tests ---"
cd "$CLEAN_REPO"
OUTPUT=$(bash "$CHECKS_DIR/test-suite-green.sh" 2>/dev/null)
if echo "$OUTPUT" | grep -q "^test-suite-green unknown"; then
    pass "test-suite-green: unknown by default (opt-in)"
else
    fail "test-suite-green: should be unknown by default. Got: $OUTPUT"
fi

# --- disk-space specific tests ---
echo ""
echo "--- disk-space check tests ---"
cd "$CLEAN_REPO"
OUTPUT=$(bash "$CHECKS_DIR/disk-space.sh" 2>/dev/null)
if echo "$OUTPUT" | grep -qE "^disk-space (pass|warn|fail)"; then
    pass "disk-space: produces valid status"
else
    fail "disk-space: should produce valid status. Got: $OUTPUT"
fi

# --- AC2-ERR: deps-installed warns (not fails) when tooling absent ---
echo ""
echo "--- AC2-ERR: deps-installed warns on missing tooling ---"
# Create a node-looking dir but no pnpm
NODE_PROJ="$TMPDIR_BASE/node-proj"
mkdir -p "$NODE_PROJ"
cd "$NODE_PROJ"
git init -q
git config user.email "test@test.com"
git config user.name "Test"
echo '{"name":"test"}' > package.json
echo "# lockfile" > pnpm-lock.yaml
# No node_modules yet

OUTPUT=$(bash "$CHECKS_DIR/deps-installed.sh" 2>/dev/null)
# Should be warn (not fail) when node_modules missing
if echo "$OUTPUT" | grep -qE "^deps-installed (warn|pass|unknown)"; then
    pass "AC2-ERR: deps-installed produces warn/pass/unknown (not fail) for missing tooling"
else
    fail "AC2-ERR: deps-installed should warn, not fail. Got: $OUTPUT"
fi

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ $FAIL -eq 0 ]] || exit 1
