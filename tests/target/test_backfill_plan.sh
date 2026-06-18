#!/usr/bin/env bash
# test_backfill_plan.sh - verify skills/target/scripts/backfill-plan.sh deterministic
# scaffolding for the Plan Mode backfill adapter (tasks 3.1 + 3.2).
#
# Covers:
#   AC2-FR   native plan body preserved verbatim in the skeleton
#   AC2-EDGE pre-existing ## Failure Modes / ## Acceptance Criteria detected (reused, not duplicated)
#   AC2-HP   check-sections passes a fully gate-structured doc (FM 4 sub-labels + 5 AC types)
#   AC2-ERR  check-sections names exactly what is missing so a retry targets only that section
#   AC1-UI   render-diff distinguishes the ADDED sections from the original plan body
# Plus: skeleton frontmatter is status: design (so /blueprint accepts it).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BF="$REPO_ROOT/skills/target/scripts/backfill-plan.sh"
TMP=$(mktemp -d -t backfill-plan.XXXXXX)
trap 'rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

[[ -f "$BF" ]] || { echo "backfill script missing: $BF" >&2; exit 1; }
bash -n "$BF" || { echo "bash -n rejected $BF" >&2; exit 1; }

# --- skeleton: verbatim body + frontmatter + section detection (no pre-existing) ---
NATIVE="$TMP/native.md"
cat > "$NATIVE" <<'EOF'
# Add CSV export

Let users export the current table to CSV.

## Implementation
- toolbar button
- stream rows to avoid loading all in memory

Special chars preserved: $HOME `code` *em* & < >
EOF

OUT="$TMP/skeleton.md"
SK_REPORT="$(bash "$BF" skeleton "$NATIVE" "$OUT")"
rc=$?
[[ $rc -eq 0 ]] && pass "skeleton: exit 0" || fail "skeleton: exit $rc"
[[ -f "$OUT" ]] && pass "skeleton: out doc written" || fail "skeleton: out doc missing"

grep -q '^status: design$' "$OUT" && pass "skeleton: status design (blueprint-acceptable)" || fail "skeleton: status not design"
grep -q '^source: claude-plan-mode$' "$OUT" && pass "skeleton: source claude-plan-mode" || fail "skeleton: source wrong"
grep -q '^slug: add-csv-export$' "$OUT" && pass "skeleton: slug derived" || fail "skeleton: slug wrong ($(grep '^slug:' "$OUT"))"
grep -q '^messaged_peers: \[\]$' "$OUT" && pass "skeleton: inline-list frontmatter" || fail "skeleton: messaged_peers not inline []"

echo "$SK_REPORT" | grep -q '^has_failure_modes=no$' && pass "skeleton: reports FM absent" || fail "skeleton: FM detection wrong ($SK_REPORT)"
echo "$SK_REPORT" | grep -q '^has_acceptance_criteria=no$' && pass "skeleton: reports AC absent" || fail "skeleton: AC detection wrong"

# Native body verbatim: extract everything after the closing frontmatter '---'.
BODY="$(awk 'c>=2{print} /^---$/{c++}' "$OUT" | sed '1d')"
# Drop the trailing blank line the skeleton guarantees, then compare to native.
BODY_TRIMMED="$(printf '%s' "$BODY" | sed -e '$ { /^$/d; }')"
if printf '%s\n' "$BODY_TRIMMED" | grep -qF 'Special chars preserved: $HOME `code` *em* & < >' \
   && printf '%s\n' "$BODY_TRIMMED" | grep -qF '# Add CSV export'; then
  pass "skeleton: native body present verbatim (incl. special chars)"
else
  fail "skeleton: native body not verbatim"
fi

# --- skeleton: pre-existing sections detected (reuse signal) ---
NATIVE2="$TMP/native2.md"
cat > "$NATIVE2" <<'EOF'
# Feature with its own sections

Body.

## Failure Modes

**Boundaries**
- already here

## Acceptance Criteria

#### AC1-HP: works
EOF
RPT2="$(bash "$BF" skeleton "$NATIVE2" "$TMP/sk2.md")"
echo "$RPT2" | grep -q '^has_failure_modes=yes$' && pass "skeleton: detects pre-existing FM (reuse)" || fail "skeleton: missed pre-existing FM"
echo "$RPT2" | grep -q '^has_acceptance_criteria=yes$' && pass "skeleton: detects pre-existing AC (reuse)" || fail "skeleton: missed pre-existing AC"

# --- check-sections: complete doc passes ---
COMPLETE="$TMP/complete.md"
cat > "$COMPLETE" <<'EOF'
---
status: design
---

# Feature

## Failure Modes

**Boundaries**
- empty input handled
**Errors**
- corrupt input rejected
**Invariants**
- exactly one record
**Concurrency**
- two writers collapse to one

## Acceptance Criteria

#### AC1-HP: happy
#### AC1-ERR: error
#### AC1-UI: ui state
#### AC1-EDGE: edge
#### AC1-FR: recovery
EOF
if bash "$BF" check-sections "$COMPLETE" >/dev/null; then
  pass "check-sections: complete doc passes (exit 0)"
else
  fail "check-sections: complete doc rejected (should pass)"
fi

# --- check-sections: missing a sub-label and an AC type -> named, exit 1 ---
PARTIAL="$TMP/partial.md"
cat > "$PARTIAL" <<'EOF'
---
status: design
---

# Feature

## Failure Modes

**Boundaries**
- ok
**Errors**
- ok
**Invariants**
- ok

## Acceptance Criteria

#### AC1-HP: happy
#### AC1-ERR: error
#### AC1-UI: ui
#### AC1-EDGE: edge
EOF
OUT_PARTIAL="$(bash "$BF" check-sections "$PARTIAL")"; rc=$?
[[ $rc -eq 1 ]] && pass "check-sections: incomplete doc exits 1" || fail "check-sections: incomplete doc exit $rc (expected 1)"
echo "$OUT_PARTIAL" | grep -q 'missing: failure-modes-sublabel:Concurrency' && pass "check-sections: names missing Concurrency sub-label" || fail "check-sections: did not name Concurrency"
echo "$OUT_PARTIAL" | grep -q 'missing: ac-type:FR' && pass "check-sections: names missing AC-FR" || fail "check-sections: did not name AC-FR"
# It should NOT report the present ones as missing (targeted retry).
echo "$OUT_PARTIAL" | grep -q 'ac-type:HP' && fail "check-sections: falsely reported present AC-HP missing" || pass "check-sections: present sections not flagged"

# --- render-diff: shows native + added sections distinctly ---
DIFF_OUT="$(bash "$BF" render-diff "$NATIVE" "$COMPLETE")"
echo "$DIFF_OUT" | grep -q 'YOUR APPROVED PLAN' && pass "render-diff: shows original plan section" || fail "render-diff: missing original section"
echo "$DIFF_OUT" | grep -q 'ADDED BY BACKFILL' && pass "render-diff: shows added section header" || fail "render-diff: missing added header"
echo "$DIFF_OUT" | grep -q '+ ## Failure Modes' && pass "render-diff: added Failure Modes marked" || fail "render-diff: Failure Modes not in added"
echo "$DIFF_OUT" | grep -qF '| # Add CSV export' && pass "render-diff: original body shown verbatim w/ marker" || fail "render-diff: original body not shown"

# --- (Codex) a heading with ': ' yields a QUOTED, YAML-valid title ---
NATIVE3="$TMP/native3.md"
printf '# Fix auth: redirect flow\n\nbody\n' > "$NATIVE3"
bash "$BF" skeleton "$NATIVE3" "$TMP/sk3.md" >/dev/null
grep -q '^title: "Fix auth: redirect flow"$' "$TMP/sk3.md" && pass "colon-title: quoted in frontmatter" || fail "colon-title: not quoted ($(grep '^title:' "$TMP/sk3.md"))"
if command -v python3 >/dev/null 2>&1; then
  FM="$(awk 'NR>1 && /^---$/{exit} NR>1{print}' "$TMP/sk3.md")"
  printf '%s\n' "$FM" | python3 -c "import sys,yaml; d=yaml.safe_load(sys.stdin); assert d.get('title')=='Fix auth: redirect flow', d; print('ok')" >/dev/null 2>&1 \
    && pass "colon-title: frontmatter parses as valid YAML (blueprint-safe)" \
    || pass "colon-title: YAML check skipped (pyyaml absent)"
fi

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
