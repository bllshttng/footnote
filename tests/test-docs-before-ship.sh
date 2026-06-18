#!/usr/bin/env bash
# test-docs-before-ship.sh - regression guard for the phase ordering rule
# that docs must land on the feature branch BEFORE ship creates the PR.
#
# Root issue this test guards against: when auto_merge_approved=true, any
# phase that runs AFTER the auto-merge fires is effectively stranded on a
# follow-up branch, because the merge already shipped the PR. Docs must
# therefore run before ship so they ride in the same PR. See the Preconditions
# section in skills/target/references/ship-phase.md.
#
# What we assert (canonical skills/target/SKILL.md + ship-phase.md):
#  1. The "Philosophy" phase table lists the Docs row BEFORE the Ship row.
#  2. The "Prior-phase mapping" handoff table lists docs BEFORE ship (so the
#     handoff chain matches the docs-before-ship pipeline order).
#  3. The pipeline prose documents that docs/browser run BEFORE /pr create and
#     ride in any auto-merge (the anti-stranding rationale).
#  4. ship-phase.md documents docs-before-ship as a ship precondition.
#  5. The top-level ASCII pipeline box lists /ship-docs before /pr create.
#
# Any future edit that inverts these orderings should fail this test loudly.
#
# Note (2026-06-09): the legacy `docs_generated` boolean gate this test used to
# assert was removed in the control-plane collapse (gate booleans no longer
# exist). Docs-before-ship is now enforced by phase ordering + the ship-phase.md
# prose precondition (target loops back to docs if it has not run), which is
# what checks 3 and 4 assert.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SKILL="$REPO_ROOT/skills/target/SKILL.md"
SHIP_PHASE_REF="$REPO_ROOT/skills/target/references/ship-phase.md"

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

echo "=== Phase ordering regression checks ==="
echo ""

# Sanity: files exist
if [[ ! -f "$SKILL" ]]; then
    fail "skills/target/SKILL.md not found at $SKILL"
    exit 1
fi
if [[ ! -f "$SHIP_PHASE_REF" ]]; then
    fail "skills/target/references/ship-phase.md not found at $SHIP_PHASE_REF"
    exit 1
fi

# ---------------------------------------------------------------------------
# 1. In the Philosophy phase table, the "Docs" row must come BEFORE the "Ship"
#    row. Rows look like "| Docs | `/ship-docs` | ... |" and "| Ship | ... |".
# ---------------------------------------------------------------------------
DOCS_LINE=$(grep -nE '^\|[[:space:]]*Docs[[:space:]]*\|' "$SKILL" | head -1 | cut -d: -f1)
SHIP_LINE=$(grep -nE '^\|[[:space:]]*Ship[[:space:]]*\|' "$SKILL" | head -1 | cut -d: -f1)

if [[ -z "$DOCS_LINE" ]]; then
    fail "Philosophy table: no 'Docs' row found"
elif [[ -z "$SHIP_LINE" ]]; then
    fail "Philosophy table: no 'Ship' row found"
elif (( DOCS_LINE < SHIP_LINE )); then
    pass "Philosophy table: Docs (line $DOCS_LINE) precedes Ship (line $SHIP_LINE)"
else
    fail "Philosophy table: Docs (line $DOCS_LINE) must precede Ship (line $SHIP_LINE) - auto-merge would strand docs otherwise"
fi

# ---------------------------------------------------------------------------
# 2. In the "Prior-phase mapping" handoff table (lower-case rows like
#    "| docs | ... |" and "| ship | ... |"), docs must come before ship so the
#    handoff chain matches the docs-before-ship pipeline order.
# ---------------------------------------------------------------------------
INV_DOCS_LINE=$(grep -nE '^\|[[:space:]]*docs[[:space:]]*\|' "$SKILL" | head -1 | cut -d: -f1)
INV_SHIP_LINE=$(grep -nE '^\|[[:space:]]*ship[[:space:]]*\|' "$SKILL" | head -1 | cut -d: -f1)

if [[ -z "$INV_DOCS_LINE" ]]; then
    fail "Prior-phase mapping: no '| docs |' row found"
elif [[ -z "$INV_SHIP_LINE" ]]; then
    fail "Prior-phase mapping: no '| ship |' row found"
elif (( INV_DOCS_LINE < INV_SHIP_LINE )); then
    pass "Prior-phase mapping table: docs (line $INV_DOCS_LINE) precedes ship (line $INV_SHIP_LINE)"
else
    fail "Prior-phase mapping table: docs (line $INV_DOCS_LINE) must precede ship (line $INV_SHIP_LINE) - handoff chain contradicts docs-before-ship"
fi

# ---------------------------------------------------------------------------
# 3. The pipeline prose must document that docs/browser run BEFORE /pr create
#    and ride in any auto-merge (the anti-stranding rationale). This replaces
#    the removed `docs_generated` boolean-gate assertion.
# ---------------------------------------------------------------------------
if grep -qE 'run BEFORE .*pr create.*included in any auto-merge' "$SKILL"; then
    pass "Pipeline prose documents docs/browser run BEFORE /pr create and ride in any auto-merge"
else
    fail "Pipeline prose missing the docs-before-/pr-create anti-stranding rationale"
fi

# ---------------------------------------------------------------------------
# 4. ship-phase.md must document docs-before-ship as a ship precondition.
#    (The removed `docs_generated` gate token is no longer required; the
#    precondition is now prose.)
# ---------------------------------------------------------------------------
if grep -qE '^## Preconditions' "$SHIP_PHASE_REF" \
    && grep -qE 'ride in the same PR|before creating the PR' "$SHIP_PHASE_REF"; then
    pass "ship-phase.md: Preconditions section documents docs landing before the PR"
else
    fail "ship-phase.md: missing Preconditions section or docs-before-ship precondition prose"
fi

# ---------------------------------------------------------------------------
# 5. Sanity: the SKILL.md top-level ASCII pipeline (inside a box-drawn block)
#    must list /ship-docs before /pr create. Scoped to box lines inside the
#    "## The Full Pipeline" section so we don't match references elsewhere.
# ---------------------------------------------------------------------------
PIPELINE_START=$(grep -nE '^## The Full Pipeline' "$SKILL" | head -1 | cut -d: -f1)
if [[ -z "$PIPELINE_START" ]]; then
    fail "Top-level pipeline: '## The Full Pipeline' heading not found"
else
    # Find the closing `## ` heading that ends the section
    PIPELINE_END=$(awk -v start="$PIPELINE_START" 'NR > start && /^## / { print NR; exit }' "$SKILL")
    PIPELINE_END=${PIPELINE_END:-99999}
    # Extract box-drawn lines (start with │) between PIPELINE_START and PIPELINE_END
    BOX_SHIP_DOCS=$(awk -v start="$PIPELINE_START" -v end="$PIPELINE_END" \
        'NR > start && NR < end && /^│.*\/ship-docs/ { print NR; exit }' "$SKILL")
    BOX_PR_CREATE=$(awk -v start="$PIPELINE_START" -v end="$PIPELINE_END" \
        'NR > start && NR < end && /^│.*\/pr create/ { print NR; exit }' "$SKILL")
    if [[ -z "$BOX_SHIP_DOCS" || -z "$BOX_PR_CREATE" ]]; then
        fail "Top-level ASCII pipeline: missing /ship-docs or /pr create in the box"
    elif (( BOX_SHIP_DOCS < BOX_PR_CREATE )); then
        pass "Top-level ASCII pipeline: /ship-docs (line $BOX_SHIP_DOCS) precedes /pr create (line $BOX_PR_CREATE)"
    else
        fail "Top-level ASCII pipeline: /ship-docs (line $BOX_SHIP_DOCS) must precede /pr create (line $BOX_PR_CREATE)"
    fi
fi

echo ""
echo "================================"
echo "Results: $PASS passed, $FAIL failed"
echo "================================"
exit "$FAIL"
