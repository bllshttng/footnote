#!/usr/bin/env bash
# Contract tests for the fno review lane's verify stage (references/single-lane.md).
#
# The lane is an inline discipline, so its contract lives in three places these
# tests pin together: the reference text (the rules must exist), report and
# findings fixtures (the REFUTED and cite-or-drop shapes), and the REAL
# classifier (the ground truth the lane's emit decision keys on). Asserting
# finding counts alone proves nothing - a reviewer that simply found less
# looks identical - so every case asserts the marker the outcome produces.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
LANE="$REPO_ROOT/skills/review/references/single-lane.md"
FNO_BIN="${FNO:-fno}"

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t lane-verdicts-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# ---- fixture: one provable guard, one real defect, one falsy-zero path
mkdir -p "$TMP/src"
cat > "$TMP/src/fee.py" <<'EOF'
def charge(cents):
    if cents == 0:
        return None
    return {"amount": cents}
EOF
GUARD='    if cents == 0:'

cat > "$TMP/candidates.json" <<'EOF'
[
  {"file": "src/fee.py", "line": 2, "summary": "zero cents returns a nonzero charge", "category": "correctness", "expected": "REFUTED"},
  {"file": "src/fee.py", "line": 4, "summary": "amount field is the wrong unit", "category": "correctness", "expected": "CONFIRMED"},
  {"file": "src/fee.py", "line": 3, "summary": "None return treated as a charge downstream", "category": "correctness", "expected": "PLAUSIBLE"}
]
EOF

# ---- the checker: the cite-or-drop contract on REFUTED entries, as an
# executable rule. A REFUTED verdict is legal only when its proving line
# exists verbatim in the cited file. The negative-control run inverts the
# grep below and asserts exactly these violations go red.
refuted_proof_violations() { # <report> <srcroot>
  local report="$1" root="$2" entry file quote
  grep -E '^- [^:]+:[0-9]+ REFUTED .* - proving line: "' "$report" 2>/dev/null | while IFS= read -r entry; do
    file="$(printf '%s' "$entry" | sed -E 's/^- ([^:]+):[0-9]+ REFUTED .*$/\1/')"
    quote="$(printf '%s' "$entry" | sed -E 's/.* - proving line: "(.*)"$/\1/')"
    if ! grep -qF -- "$quote" "$root/$file" 2>/dev/null; then
      echo "refuted-without-proof: $file: proving line not in file: $quote"
    fi
  done
}

refuted_in_findings_violations() { # <report> <findings.json>
  local report="$1" findings="$2" entry file lineno
  grep -E '^- [^:]+:[0-9]+ REFUTED ' "$report" 2>/dev/null | while IFS= read -r entry; do
    file="$(printf '%s' "$entry" | sed -E 's/^- ([^:]+):([0-9]+) REFUTED .*$/\1/')"
    lineno="$(printf '%s' "$entry" | sed -E 's/^- ([^:]+):([0-9]+) REFUTED .*$/\2/')"
    if jq -e --arg f "$file" --argjson l "$lineno" \
      'any(.[]; .file == $f and .line == $l)' "$findings" >/dev/null 2>&1; then
      echo "refuted-candidate-in-findings: $file:$lineno"
    fi
  done
}

silent_drop_violations() { # <candidates> <report> <findings.json>
  jq -c '.[]' "$1" | while IFS= read -r cand; do
    file="$(jq -r '.file' <<<"$cand")"
    lineno="$(jq -r '.line' <<<"$cand")"
    exp="$(jq -r '.expected' <<<"$cand")"
    if [ "$exp" != "REFUTED" ]; then
      jq -e --arg f "$file" --argjson l "$lineno" \
        'any(.[]; .file == $f and .line == $l)' "$3" >/dev/null 2>&1 || \
        echo "silent-drop: $file:$lineno expected $exp in findings"
    fi
  done
}

illegal_verdict_violations() { # <findings.json>
  jq -r '.[].verdict // empty' "$1" 2>/dev/null | grep -vE '^(CONFIRMED|PLAUSIBLE)$' | sed 's/^/illegal-verdict: /' || true
}

echo "== reference carries the verdict vocabulary and both anti-noise rules"
for marker in '**CONFIRMED** - can name the inputs/state' '**PLAUSIBLE** - mechanism is real' '**REFUTED** - factually wrong' '**PLAUSIBLE by default**' 'only when constructible from the code' 'never counts its own findings'; do
  if grep -qF -- "$marker" "$LANE"; then pass "reference carries: $marker"; else fail "reference missing: $marker"; fi
done

echo "== AC1-MARKER: guarded candidate is REFUTED quoting the guard line, absent from findings"
cat > "$TMP/report-good.md" <<EOF
review lane: level=medium
Scope: first-round

## Findings
- src/fee.py:4 CONFIRMED amount field is the wrong unit
- src/fee.py:3 PLAUSIBLE None return treated as a charge downstream

## REFUTED
- src/fee.py:2 REFUTED zero cents returns a nonzero charge - proving line: "$GUARD"

Verdict: fail
EOF
cat > "$TMP/findings-good.json" <<'EOF'
[
  {"file": "src/fee.py", "line": 4, "summary": "amount field is the wrong unit", "short_summary": "amount field is the wrong unit", "failure_scenario": "charge(500) books 500 dollars instead of 500 cents", "category": "correctness", "verdict": "CONFIRMED"},
  {"file": "src/fee.py", "line": 3, "summary": "None return treated as a charge downstream", "short_summary": "None return treated as a charge", "failure_scenario": "a caller on a cold path charges None as zero and skips the receipt", "category": "correctness", "verdict": "PLAUSIBLE"}
]
EOF

# Positive control on the symbol itself: the guard line exists in the fixture
# source, so a green run here means the tool looked at the real file.
if grep -qF -- "$GUARD" "$TMP/src/fee.py"; then pass "guard line present in fixture source"; else fail "guard line missing from fixture source (fixture broken)"; fi
if grep -qF -- "$GUARD" "$TMP/report-good.md"; then pass "quoted guard line appears in the report"; else fail "quoted guard line absent from the report"; fi

v="$(refuted_proof_violations "$TMP/report-good.md" "$TMP")"
[ -z "$v" ] && pass "every REFUTED entry proves its verdict from the file" || { fail "illegal REFUTED in good fixture"; echo "$v"; }
v="$(refuted_in_findings_violations "$TMP/report-good.md" "$TMP/findings-good.json")"
[ -z "$v" ] && pass "the refuted candidate is absent from the findings file" || { fail "refuted candidate leaked into findings"; echo "$v"; }
v="$(silent_drop_violations "$TMP/candidates.json" "$TMP/report-good.md" "$TMP/findings-good.json")"
[ -z "$v" ] && pass "no non-refuted candidate silently dropped" || { fail "silent drop"; echo "$v"; }
v="$(illegal_verdict_violations "$TMP/findings-good.json")"
[ -z "$v" ] && pass "findings verdicts are CONFIRMED or PLAUSIBLE only" || { fail "illegal verdict in findings"; echo "$v"; }

echo "== AC1-ERR: a falsy-zero candidate may not be REFUTED as speculative"
cat > "$TMP/report-speculative.md" <<'EOF'
Scope: first-round

## REFUTED
- src/fee.py:3 REFUTED None return treated as a charge downstream - proving line: "seems speculative; the cold path is unlikely in practice"

Verdict: fail
EOF
v="$(refuted_proof_violations "$TMP/report-speculative.md" "$TMP")"
case "$v" in
  *refuted-without-proof*) pass "speculative refutation flagged: the candidate must stay PLAUSIBLE" ;;
  *) fail "speculative refutation accepted; PLAUSIBLE-by-default not enforced" ;;
esac

echo "== AC1-MARKER negative: a mismatched quote cannot carry a REFUTED verdict"
cat > "$TMP/report-mismatch.md" <<'EOF'
Scope: first-round

## REFUTED
- src/fee.py:2 REFUTED zero cents returns a nonzero charge - proving line: "    if cents > 0: return None"

Verdict: fail
EOF
v="$(refuted_proof_violations "$TMP/report-mismatch.md" "$TMP")"
case "$v" in
  *refuted-without-proof*) pass "mismatched proving line flagged" ;;
  *) fail "mismatched proving line accepted" ;;
esac

echo "== AC1-HP: the real classifier counts a CONFIRMED finding BLOCKING"
if ! "$FNO_BIN" do review classify --findings-file "$TMP/findings-good.json" --emit-record > "$TMP/classified.json" 2>"$TMP/classify.err"; then
  fail "classify refused the findings file (deployment without 'do review classify' is too old for this suite)"
  cat "$TMP/classify.err" >&2
else
  b="$(jq -r '.findings_blocking' "$TMP/classified.json")"
  n="$(jq -r '.findings_nonblocking' "$TMP/classified.json")"
  # Both findings carry category correctness, which the allowlist does not
  # name, so both block: verdict CONFIRMED by rule 2, PLAUSIBLE by rule 4.
  [ "$b" = "2" ] && pass "both correctness findings classified BLOCKING (found $b)" || fail "expected 2 blocking, got $b"
  [ "$n" = "0" ] && pass "no allowlisted categories in the payload (found $n nonblocking)" || fail "expected 0 nonblocking, got $n"
fi

echo "== AC1-HP: a style-category finding is nonblocking; the emit ground truth"
cat > "$TMP/findings-style.json" <<'EOF'
[{"file": "src/fee.py", "line": 4, "summary": "dict literal spacing", "failure_scenario": "inconsistent spacing in the returned dict", "category": "style", "verdict": "PLAUSIBLE"}]
EOF
if "$FNO_BIN" do review classify --findings-file "$TMP/findings-style.json" --emit-record > "$TMP/classified-style.json" 2>/dev/null; then
  b="$(jq -r '.findings_blocking' "$TMP/classified-style.json")"
  [ "$b" = "0" ] && pass "style finding classified nonblocking; lane would emit pass" || fail "style finding blocking ($b); emit decision would flip"
else
  fail "classify refused the style findings file"
fi

echo
echo "lane-verdicts: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
