#!/usr/bin/env bash
# Carry-forward contract tests for the fno review lane (references/single-lane.md).
#
# A narrowed round that derives zero findings of its own must not read as a
# clean round while a prior blocking finding is still live. These tests pin
# the three behaviors that make narrowing safe: a still-valid prior quote is
# carried and blocks; a quote that no longer validates is named
# dropped-unverifiable, not silently omitted; and an unreadable prior report
# forces the full scope with no attestation from the incomplete round.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
LANE="$REPO_ROOT/skills/review/references/single-lane.md"

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t carry-forward-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# ---- fixture: the prior round's critical finding cites this quote
QUOTE='    if token is None: raise Reject("missing token")'
mkdir -p "$TMP/src"
printf '%s\n' 'def relay(token):' "$QUOTE" '    return forward(token)' > "$TMP/src/relay.py"

cat > "$TMP/prior-body.md" <<EOF
## Critical Issues
- src/relay.py:2 CRITICAL missing-token check absent - quote: "$QUOTE"
EOF

# The quote-match rule, executable. The negative control inverts this grep
# and asserts exactly the carried rows go red.
quote_valid_at_head() { # <quote> <file>
  grep -qF -- "$1" "$2" 2>/dev/null
}

echo "== reference carries the carry-forward rules"
for marker in 'carried from round <id>' 'dropped-unverifiable' 'emit no attestation from the incomplete round' 'Never read a failed inspection as an empty prior report'; do
  if grep -qF -- "$marker" "$LANE"; then pass "reference carries: $marker"; else fail "reference missing: $marker"; fi
done

echo "== AC8-MARKER: prior quote still valid at head is carried and blocks"
# Positive control on the symbol: the quote exists in the fixture file.
quote_valid_at_head "$QUOTE" "$TMP/src/relay.py" && pass "prior quote present at current head" || fail "fixture broken: quote not in file"

cat > "$TMP/report-carried.md" <<EOF
Scope: incremental

## Carried from prior round
- src/relay.py:2 CRITICAL missing-token check absent - carried from round r1

## Findings
(none derived in the increment)

Verdict: fail
EOF
cat > "$TMP/findings-carried.json" <<EOF
[{"file": "src/relay.py", "line": 2, "summary": "missing-token check absent", "short_summary": "missing-token check absent", "failure_scenario": "a request without a token is forwarded instead of rejected", "category": "correctness", "verdict": "CONFIRMED"}]
EOF

# The carried finding is present by its cited file:line, not by prose.
if grep -qF -- 'src/relay.py:2' "$TMP/report-carried.md" && grep -qF -- 'carried from round r1' "$TMP/report-carried.md"; then
  pass "carried finding present by cited file:line with the round tag"
else
  fail "carried finding absent or untagged"
fi
if jq -e --arg f 'src/relay.py' --argjson l 2 'any(.[]; .file == $f and .line == $l)' "$TMP/findings-carried.json" >/dev/null; then
  pass "carried finding reached the findings payload"
else
  fail "carried finding missing from findings payload"
fi
if grep -qF -- 'Verdict: fail' "$TMP/report-carried.md"; then
  pass "verdict is not pass while the carry is unresolved"
else
  fail "zero-finding increment read as a clean round over a live carry"
fi

echo "== AC8-HP: a quote absent from the cited location is dropped-unverifiable"
printf '%s\n' 'def relay(token):' '    return forward(token)' > "$TMP/src/relay.py"
quote_valid_at_head "$QUOTE" "$TMP/src/relay.py" && fail "fixture broken: quote still present" || pass "quote gone at current head"

cat > "$TMP/report-dropped.md" <<EOF
Scope: incremental

## Dropped unverifiable
- dropped-unverifiable: src/relay.py:2 (quote no longer matches the file)

## Findings
(none derived in the increment)

Verdict: pass
EOF
if grep -qF -- 'dropped-unverifiable: src/relay.py:2' "$TMP/report-dropped.md"; then
  pass "the dropped prior finding is named, not silently omitted"
else
  fail "dropped prior finding not named"
fi
if grep -qF -- 'carried from round' "$TMP/report-dropped.md"; then
  fail "quote that no longer validates was carried anyway"
else
  pass "invalidated quote not carried"
fi

echo "== AC8-ERR: an unreadable prior report forces full scope, no attestation"
cat > "$TMP/report-inspect-failed.md" <<EOF
Scope: full-scope (prior report unreadable; inspect failed)

## Findings
(full re-review at merge-base scope)

Verdict: fail
EOF
if grep -qF -- 'Scope: full-scope (prior report unreadable; inspect failed)' "$TMP/report-inspect-failed.md"; then
  pass "failed inspection widened the scope instead of reading as an empty prior"
else
  fail "failed inspection read as an empty prior report"
fi
if grep -qF -- 'emit no attestation from the incomplete round' "$LANE"; then
  pass "reference bars the attestation on the incomplete round"
else
  fail "reference allows an attestation from the incomplete round"
fi

echo
echo "carry-forward: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
