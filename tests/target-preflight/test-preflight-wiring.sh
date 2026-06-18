#!/usr/bin/env bash
# Test suite for target preflight wiring (Story 3)
# TDD: Tests for skill.md modifications - run to get RED before implementing

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET_SKILL="$REPO_ROOT/skills/target/SKILL.md"

PASS=0
FAIL=0

pass() { echo "  PASS: $*"; ((PASS++)) || true; }
fail() { echo "  FAIL: $*"; ((FAIL++)) || true; }

echo "=== Story 3: Preflight wiring tests ==="

# --- AC1-HP: skills/target/SKILL.md mentions preflight in init section ---
echo ""
echo "--- AC1-HP: target/SKILL.md wired ---"
if grep -q "target-preflight" "$TARGET_SKILL" 2>/dev/null; then
    pass "AC1-HP: target/SKILL.md references target-preflight"
else
    fail "AC1-HP: target/SKILL.md should reference target-preflight. Missing wiring."
fi

if grep -q "run-checks.sh" "$TARGET_SKILL" 2>/dev/null; then
    pass "AC1-HP: target/SKILL.md references run-checks.sh"
else
    fail "AC1-HP: target/SKILL.md should reference run-checks.sh"
fi

# --- AC2-ERR: skills mention BLOCKED behavior on preflight failure ---
echo ""
echo "--- AC2-ERR: BLOCKED on preflight failure documented ---"
if grep -q "BLOCKED" "$TARGET_SKILL" 2>/dev/null && grep -A5 "preflight\|run-checks" "$TARGET_SKILL" | grep -q "BLOCKED\|blocked"; then
    pass "AC2-ERR: target/SKILL.md documents BLOCKED on preflight failure"
else
    # Check if BLOCKED is mentioned near preflight section
    if grep -q "target-preflight" "$TARGET_SKILL" && grep -q "BLOCKED" "$TARGET_SKILL"; then
        pass "AC2-ERR: target/SKILL.md has both preflight and BLOCKED references"
    else
        fail "AC2-ERR: target/SKILL.md should document BLOCKED on preflight failure"
    fi
fi

# --- AC3-UI: user sees failed checks listed ---
echo ""
echo "--- AC3-UI: user guidance documented ---"
if grep -q "skip-preflight\|skip_preflight\|--skip-preflight" "$TARGET_SKILL" 2>/dev/null; then
    pass "AC3-UI: target/SKILL.md documents --skip-preflight override"
else
    fail "AC3-UI: target/SKILL.md should document --skip-preflight flag"
fi

# --- AC4-EDGE: determinism - preflight appears in init (before think phase) ---
echo ""
echo "--- AC4-EDGE: preflight wired in init section ---"
# Find where preflight is mentioned relative to think phase
TARGET_CONTENT=$(cat "$TARGET_SKILL" 2>/dev/null || echo "")

# Check that run-checks appears before the execute pipeline section
PREFLIGHT_LINE=$(grep -n "run-checks.sh" "$TARGET_SKILL" 2>/dev/null | head -1 | cut -d: -f1 || echo "0")
EXECUTE_LINE=$(grep -n "### 4. Execute Pipeline\|Execute Pipeline" "$TARGET_SKILL" 2>/dev/null | head -1 | cut -d: -f1 || echo "9999")

if [[ -n "$PREFLIGHT_LINE" && "$PREFLIGHT_LINE" -gt 0 && "$PREFLIGHT_LINE" -lt "$EXECUTE_LINE" ]]; then
    pass "AC4-EDGE: preflight wired before Execute Pipeline section (line $PREFLIGHT_LINE < $EXECUTE_LINE)"
else
    fail "AC4-EDGE: preflight should appear before Execute Pipeline (preflight=$PREFLIGHT_LINE, execute=$EXECUTE_LINE)"
fi

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ $FAIL -eq 0 ]] || exit 1
