#!/usr/bin/env bash
# test_plan_mode_e2e.sh - end-to-end integration across all Plan Mode front-door
# components (task 5.1): the capture hook, detection, body extraction, the
# backfill skeleton + check-sections gate, render-diff, and atomic consume
# compose into the AC1-HP happy path; an incomplete backfill never reaches a
# runnable/consumed state (AC1-ERR: backfill failure does not execute).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
CAPTURE="$REPO_ROOT/hooks/capture-plan-mode.sh"
DP="$REPO_ROOT/skills/target/scripts/detect-pending-plan.sh"
BF="$REPO_ROOT/skills/target/scripts/backfill-plan.sh"

TMP=$(mktemp -d -t plan-mode-e2e.XXXXXX)
export FNO_CLAIMS_ROOT="$TMP/claims"
mkdir -p "$FNO_CLAIMS_ROOT" "$TMP/.fno"
trap 'rm -rf "$TMP"' EXIT

PASS=0; FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

command -v jq >/dev/null 2>&1 || { echo "SKIP: jq required"; exit 0; }
for f in "$CAPTURE" "$DP" "$BF"; do [[ -f "$f" ]] || { echo "missing: $f" >&2; exit 1; }; done

SC="$TMP/.fno/.pending-plan.md"

# === 1. Capture: simulate an approved ExitPlanMode -> sidecar written ===
PLAN=$'# Add CSV export\n\nLet users export the current table to CSV from the toolbar.'
jq -nc --arg cwd "$TMP" --arg sid "e2e-sess" --arg plan "$PLAN" \
  '{tool_name:"ExitPlanMode", cwd:$cwd, session_id:$sid, tool_input:{plan:$plan}, tool_response:null}' \
  | bash "$CAPTURE"
[[ -f "$SC" ]] && pass "e2e: capture hook wrote the sidecar" || fail "e2e: sidecar not written"

# === 2. Detect: bare /target finds it pending ===
DET="$(bash "$DP" detect --sidecar "$SC")"
echo "$DET" | grep -q '^result=pending$' && pass "e2e: detect -> pending" || fail "e2e: detect $DET"
echo "$DET" | grep -q '^slug=add-csv-export$' && pass "e2e: detect slug from captured plan" || fail "e2e: slug wrong"

# === 3. Body + skeleton: native plan preserved verbatim ===
bash "$DP" body "$TMP/native.md" --sidecar "$SC"
grep -qF 'Let users export the current table to CSV from the toolbar.' "$TMP/native.md" \
  && pass "e2e: body extracted verbatim" || fail "e2e: body not verbatim"
ENR="$TMP/.fno/.pending-plan.enriched.md"
bash "$BF" skeleton "$TMP/native.md" "$ENR" >/dev/null
grep -q '^status: design$' "$ENR" && pass "e2e: skeleton status design" || fail "e2e: skeleton status wrong"

# === 4. AC1-ERR: an INCOMPLETE backfill never passes the gate ===
# (skeleton alone, no synthesized sections -> check-sections must fail)
if bash "$BF" check-sections "$ENR" >/dev/null 2>&1; then
  fail "e2e: incomplete doc passed check-sections (should fail)"
else
  pass "e2e: incomplete backfill fails the gate (AC1-ERR: would not execute)"
fi
# And the sidecar is STILL pending - a failed backfill never consumes it.
grep -q '^status: pending$' "$SC" && pass "e2e: failed backfill leaves sidecar pending" || fail "e2e: sidecar consumed on failure"

# === 5. Synthesize the gate-required sections (the LLM step, inlined here) ===
cat >> "$ENR" <<'EOF'

## Failure Modes

**Boundaries**
- empty table exports a header-only CSV
**Errors**
- a write failure is surfaced, not swallowed
**Invariants**
- exactly one CSV file produced per export action
**Concurrency**
- two export clicks collapse to one download

## Acceptance Criteria

#### AC1-HP: clicking export downloads a CSV of the visible rows
#### AC1-ERR: a failed export shows an error toast
#### AC1-UI: the export button is disabled while exporting
#### AC1-EDGE: exporting an empty table yields headers only
#### AC1-FR: an interrupted export leaves no partial file
EOF
bash "$BF" check-sections "$ENR" >/dev/null \
  && pass "e2e: synthesized doc passes the gate (AC2-HP)" || fail "e2e: synthesized doc still fails gate"

# === 6. render-diff shows the added sections distinctly ===
# Capture first (piping into `grep -q` under pipefail can SIGPIPE the producer).
DIFF_OUT="$(bash "$BF" render-diff "$TMP/native.md" "$ENR")"
echo "$DIFF_OUT" | grep -q 'ADDED BY BACKFILL' \
  && pass "e2e: render-diff distinguishes added sections (AC1-UI)" || fail "e2e: render-diff missing"

# === 7. Consume on confirm-yes -> sidecar consumed, then inert ===
bash "$DP" consume --sidecar "$SC" --holder "target-session:e2e" >/dev/null \
  && pass "e2e: consume succeeds after confirm-yes" || fail "e2e: consume failed"
grep -q '^status: consumed$' "$SC" && pass "e2e: sidecar marked consumed" || fail "e2e: sidecar not consumed"
echo "$(bash "$DP" detect --sidecar "$SC")" | grep -q '^result=none$' \
  && pass "e2e: consumed sidecar is inert to detect" || fail "e2e: consumed sidecar still detected"

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
