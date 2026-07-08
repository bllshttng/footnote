#!/usr/bin/env bash
# Test: ship-phase.md PR->node link is VERIFIED (read-back + retry + refuse-to-promise).
#
# x-e106 Part 2. Two halves:
#   A. Prose presence - the ship-phase doc carries the hardened directives so the
#      LLM executes them (read-back via --field pr_number, one retry, help
#      emission on persistent failure, defer-to-reconcile on a different value).
#   B. Behavioral - a faithful reproduction of the verify-retry loop against a
#      stub `fno`, proving the refusal fires (AC2-ERR) and the happy path passes
#      (AC2-HP), the idempotent re-run converges (AC2-FR), and a pre-existing
#      different pr_number is left untouched (AC1-EDGE).

set -euo pipefail

PASS_COUNT=0
FAIL_COUNT=0
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SHIP="$REPO_ROOT/skills/target/references/ship-phase.md"

pass() { echo "  PASS: $1"; PASS_COUNT=$((PASS_COUNT + 1)); }
fail() { echo "  FAIL: $1"; FAIL_COUNT=$((FAIL_COUNT + 1)); }

echo "=== test_ship_phase_link_verify.sh ==="
echo "File: $SHIP"
echo ""

# -----------------------------------------------------------------------
# A. Prose presence
# -----------------------------------------------------------------------
grep -q -- '--field pr_number' "$SHIP" \
  && pass "A1: read-back uses 'fno backlog get --field pr_number'" \
  || fail "A1: read-back via --field pr_number not found"

grep -q 'for attempt in 1 2' "$SHIP" \
  && pass "A2: one-retry loop present" \
  || fail "A2: retry loop not found"

grep -q 'pr-node-link-failed' "$SHIP" \
  && pass "A3: help reason 'pr-node-link-failed' present" \
  || fail "A3: help emission not found"

grep -qi 'refus' "$SHIP" \
  && pass "A4: refuse-to-promise language present" \
  || fail "A4: refuse-to-promise language not found"

grep -qi 'out-of-band authority\|not overwriting\|not writing' "$SHIP" \
  && pass "A5: defer-to-reconcile (AC1-EDGE) branch present" \
  || fail "A5: AC1-EDGE defer branch not found"

# The old fire-and-WARN "non-fatal" framing must be gone from the link step.
if grep -q 'WARN failed to link' "$SHIP"; then
  fail "A6: stale fire-and-WARN link line still present"
else
  pass "A6: stale fire-and-WARN link line removed"
fi

# The get substitutions must fail-open under set -e / pipefail (|| true): every
# `fno backlog get ... --field pr_number` read-back line ends with `|| true`.
readback_lines=$(grep -c 'fno backlog get .*--field pr_number' "$SHIP" || true)
guarded_lines=$(grep -c 'fno backlog get .*--field pr_number.*|| true' "$SHIP" || true)
if [[ "$readback_lines" -gt 0 && "$readback_lines" == "$guarded_lines" ]]; then
  pass "A7: all $readback_lines read-back substitutions fail-open with '|| true'"
else
  fail "A7: $guarded_lines/$readback_lines read-back substitutions carry '|| true'"
fi

# -----------------------------------------------------------------------
# B. Behavioral - reproduce the verify loop against a stub `fno`
# -----------------------------------------------------------------------
STUB_DIR="$(mktemp -d)"
STATE="$STUB_DIR/pr_number"
trap 'rm -rf "$STUB_DIR"' EXIT

cat > "$STUB_DIR/fno" <<STUB
#!/usr/bin/env bash
# Stub: backlog get --field pr_number reads STATE; backlog update writes STATE
# only when STICK=1 (models a write that persists vs one that silently drops).
if [[ "\$1" == "backlog" && "\$2" == "get" ]]; then cat "$STATE"; exit 0; fi
if [[ "\$1" == "backlog" && "\$2" == "update" ]]; then
  if [[ "\${STICK:-0}" == "1" ]]; then
    args=("\$@")
    for ((i=0; i<\${#args[@]}; i++)); do
      if [[ "\${args[i]}" == "--pr-number" ]]; then echo "\${args[i+1]}" > "$STATE"; fi
    done
  fi
  exit 0
fi
exit 0
STUB
chmod +x "$STUB_DIR/fno"
export PATH="$STUB_DIR:$PATH"

# The verify loop, mirroring ship-phase.md (kept in sync by check A2/A3).
run_link_verify() {
  local NODE_ID="$1" PR_NUMBER="$2" PR_URL="$3"
  local existing got link_ok attempt
  existing=$(fno backlog get "$NODE_ID" --field pr_number 2>/dev/null | tr -d '[:space:]')
  if [[ -n "$existing" && "$existing" != "null" && "$existing" != "$PR_NUMBER" ]]; then
    echo "ship: node $NODE_ID already linked to PR #$existing; leaving as-is" >&2
    return 0
  fi
  link_ok=""
  for attempt in 1 2; do
    fno backlog update "$NODE_ID" --pr-number "$PR_NUMBER" --pr-url "$PR_URL" 2>/dev/null || true
    got=$(fno backlog get "$NODE_ID" --field pr_number 2>/dev/null | tr -d '[:space:]')
    [[ "$got" == "$PR_NUMBER" ]] && { link_ok=1; break; }
  done
  if [[ -z "$link_ok" ]]; then
    echo "<help reason=\"pr-node-link-failed\" evidence=\"node $NODE_ID pr_number=${got:-<none>} expected=$PR_NUMBER\">refusing to promise</help>"
    return 1
  fi
  return 0
}

# AC2-ERR: write never sticks -> refusal fires, non-zero exit.
echo "null" > "$STATE"
if out=$(STICK=0 run_link_verify "ab-test01" "42" "http://x/42" 2>/dev/null); then
  fail "B1 (AC2-ERR): persistent link failure should exit non-zero"
else
  if grep -q 'pr-node-link-failed' <<<"$out"; then
    pass "B1 (AC2-ERR): persistent failure emits help + refuses (non-zero exit)"
  else
    fail "B1 (AC2-ERR): non-zero exit but no help emission"
  fi
fi

# AC2-HP: write sticks -> link_ok, exit 0, no help emission.
echo "null" > "$STATE"
if out=$(STICK=1 run_link_verify "ab-test01" "42" "http://x/42" 2>/dev/null); then
  if grep -q 'pr-node-link-failed' <<<"$out"; then
    fail "B2 (AC2-HP): happy path should not emit help"
  else
    pass "B2 (AC2-HP): sticking link passes with no help emission"
  fi
else
  fail "B2 (AC2-HP): sticking link should exit 0"
fi
[[ "$(cat "$STATE")" == "42" ]] \
  && pass "B3 (AC2-HP): read-back confirms pr_number persisted" \
  || fail "B3 (AC2-HP): pr_number not persisted after success"

# AC2-FR: already-linked to the SAME PR -> idempotent re-run converges, exit 0.
echo "42" > "$STATE"
if STICK=1 run_link_verify "ab-test01" "42" "http://x/42" >/dev/null 2>&1; then
  pass "B4 (AC2-FR): re-run on already-linked node converges (exit 0)"
else
  fail "B4 (AC2-FR): idempotent re-run should exit 0"
fi

# AC1-EDGE: pre-existing DIFFERENT pr_number -> defer, do NOT overwrite.
echo "99" > "$STATE"
STICK=1 run_link_verify "ab-test01" "42" "http://x/42" >/dev/null 2>&1 || true
if [[ "$(cat "$STATE")" == "99" ]]; then
  pass "B5 (AC1-EDGE): different pre-existing pr_number left untouched"
else
  fail "B5 (AC1-EDGE): overwrote reconcile authority (got $(cat "$STATE"), expected 99)"
fi

# -----------------------------------------------------------------------
# Also: pre-promise.md carries the last-line node-link assertion.
# -----------------------------------------------------------------------
PRE_PROMISE="$REPO_ROOT/skills/target/references/pre-promise.md"
grep -qi 'last-line assertion\|pr_number.*PR\|node-bound' "$PRE_PROMISE" \
  && grep -q -- '--field pr_number' "$PRE_PROMISE" \
  && pass "C1: pre-promise.md carries the node-link self-check" \
  || fail "C1: pre-promise.md missing the node-link self-check"

# The pre-promise backstop is a real assertion: it refuses on a persistent
# re-link failure (help emission), not a fire-and-forget re-link.
grep -q 'pr-node-link-failed' "$PRE_PROMISE" \
  && grep -qi 'refus' "$PRE_PROMISE" \
  && pass "C2: pre-promise backstop refuses on persistent re-link failure" \
  || fail "C2: pre-promise backstop does not refuse (fire-and-forget re-link)"

echo ""
echo "Results: $PASS_COUNT passed, $FAIL_COUNT failed"
[[ "$FAIL_COUNT" -gt 0 ]] && exit 1
exit 0
