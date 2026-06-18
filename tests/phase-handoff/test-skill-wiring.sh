#!/usr/bin/env bash
# Test suite for Story 3: phase artifact wiring in SKILL.md files
# TDD: Run BEFORE modifying SKILL.md to get RED, then modify to get GREEN

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET_SKILL="$REPO_ROOT/skills/target/SKILL.md"

PASS=0
FAIL=0

pass() { echo "  PASS: $*"; ((PASS++)) || true; }
fail() { echo "  FAIL: $*"; ((FAIL++)) || true; }

echo "=== Story 3: Phase artifact wiring in SKILL.md tests ==="

# ---------------------------------------------------------------
# AC1-HP: target/SKILL.md contains ph_write and ph_read references
# ---------------------------------------------------------------
echo ""
echo "--- AC1-HP: target/SKILL.md contains ph_write references ---"

if grep -q "ph_write" "$TARGET_SKILL"; then
  pass "AC1-HP: target/SKILL.md mentions ph_write"
else
  fail "AC1-HP: target/SKILL.md missing ph_write reference"
fi

if grep -q "ph_read" "$TARGET_SKILL"; then
  pass "AC1-HP: target/SKILL.md mentions ph_read"
else
  fail "AC1-HP: target/SKILL.md missing ph_read reference"
fi

# ---------------------------------------------------------------
# AC1-HP: all 9 phases mentioned in target/SKILL.md wiring section
# ---------------------------------------------------------------
echo ""
echo "--- AC1-HP: target/SKILL.md wiring covers all 9 phases ---"

for phase in think plan do clean review validate ship external docs; do
  if grep -q "ph_write.*${phase}\|${phase}.*ph_write\|# ${phase} phase\|ph_write ${phase}" "$TARGET_SKILL"; then
    pass "AC1-HP: target wiring covers '${phase}' phase"
  else
    # Also accept if the section mentions the phase in a table or code block
    if grep -A5 -B5 "ph_write" "$TARGET_SKILL" | grep -q "${phase}"; then
      pass "AC1-HP: target wiring covers '${phase}' phase (in context)"
    else
      fail "AC1-HP: target wiring missing '${phase}' phase coverage"
    fi
  fi
done

# ---------------------------------------------------------------
# AC2-ERR: SKILL.md documents fallback when no prior artifact exists
# ---------------------------------------------------------------
echo ""
echo "--- AC2-ERR: documents fallback when no prior artifact ---"

if grep -q "no prior handoff\|reduced context\|ph_read.*||" "$TARGET_SKILL"; then
  pass "AC2-ERR: target/SKILL.md documents fallback on missing artifact"
else
  fail "AC2-ERR: target/SKILL.md should document fallback for missing artifact"
fi

# ---------------------------------------------------------------
# AC3-UI: SKILL.md mentions one-liner context loaded log
# ---------------------------------------------------------------
echo ""
echo "--- AC3-UI: documents context-loaded log line ---"

if grep -q "context loaded\|loaded context\|handoff loaded" "$TARGET_SKILL"; then
  pass "AC3-UI: target/SKILL.md documents context-loaded log"
else
  fail "AC3-UI: target/SKILL.md should mention logging context loaded from prior phase"
fi

# ---------------------------------------------------------------
# AC4-EDGE: mentions different session_ids prevent collision
# ---------------------------------------------------------------
echo ""
echo "--- AC4-EDGE: documents session_id isolation for concurrent runs ---"

if grep -q "session_id\|worktree" "$TARGET_SKILL"; then
  pass "AC4-EDGE: target/SKILL.md mentions session_id/worktree isolation"
else
  fail "AC4-EDGE: target/SKILL.md does not mention session_id/worktree separation"
fi

# ---------------------------------------------------------------
# phase-handoff.sh exists (dependency check)
# ---------------------------------------------------------------
echo ""
echo "--- Dependency: phase-handoff.sh exists ---"

if [[ -f "$REPO_ROOT/scripts/lib/phase-handoff.sh" ]]; then
  pass "phase-handoff.sh exists (Story 2 complete)"
else
  fail "phase-handoff.sh missing - Story 2 must complete first"
fi

# ---------------------------------------------------------------
# phase-artifacts.md reference doc exists
# ---------------------------------------------------------------
echo ""
echo "--- Dependency: phase-artifacts.md exists ---"

if [[ -f "$REPO_ROOT/skills/target/references/phase-artifacts.md" ]]; then
  pass "phase-artifacts.md exists (Story 1 complete)"
else
  fail "phase-artifacts.md missing - Story 1 must complete first"
fi

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ $FAIL -eq 0 ]] || exit 1
