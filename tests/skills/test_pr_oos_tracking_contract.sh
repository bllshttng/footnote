#!/usr/bin/env bash
# Contract test for the PR-create out-of-scope tracking flow.
#
# The flow ships as TWO reachable instruction paths - the pr-create role
# subagent prompt and the bundled canonical create reference. A rule enforced in
# one copy is decorative: the other path still runs. So this test forbids the
# synthetic-carveout fallback in each surface AND pins the two extracted
# sections byte-identical, which makes divergence itself a failure.
#
# Run: bash tests/skills/test_pr_oos_tracking_contract.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
GATE="$REPO_ROOT/scripts/ci/check-oos-tracked.sh"

SURFACES=(
  "$REPO_ROOT/skills/pr/agents/pr-creator.md"
  "$REPO_ROOT/skills/pr/references/create.md"
)

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

# The tracking flow: the numbered item list from "Cite first" through the
# idempotence note that closes it.
section_of() {
  sed -n '/^1\. \*\*Cite first\.\*\*/,/^\*\*Idempotent by construction:/p' "$1"
}

for surface in "${SURFACES[@]}"; do
  label="${surface#"$REPO_ROOT"/}"

  if [[ ! -f "$surface" ]]; then
    fail "$label: surface not found"
    continue
  fi

  section="$(section_of "$surface")"
  if [[ -z "$section" ]]; then
    fail "$label: tracking section not found (has the 'Cite first' list moved?)"
    continue
  fi

  # AC18-HP: a failed filing must never mint a carveout to repair the citation.
  # Pin the INVOCATION, not the mention - the surviving prose names the verb on
  # purpose so a future editor reads why it is absent rather than restoring it.
  if [[ "$section" == *'$(fno carveout add'* || "$section" == *"CV_ID="* ]]; then
    fail "$label: still mints a synthetic carveout fallback"
  else
    pass "$label: no synthetic carveout fallback"
  fi

  # The removal must not take the surrounding contract with it.
  if [[ "$section" == *"leave the line byte-identical (idempotent)"* ]]; then
    pass "$label: cite-first idempotence preserved"
  else
    fail "$label: cite-first idempotence missing"
  fi

  if [[ "$section" == *"fno backlog idea"* ]]; then
    pass "$label: supported backlog filing preserved"
  else
    fail "$label: supported backlog filing missing"
  fi

  # AC18-HP recovery: uncited and loud, never silently waived.
  if [[ "$section" == *'print a `warn:` line naming it'* ]]; then
    pass "$label: loud uncited warning preserved"
  else
    fail "$label: loud uncited warning missing"
  fi

  if [[ "$section" == *'NEVER write an `oos-ok:` waiver'* ]]; then
    pass "$label: oos-ok waiver still forbidden as a tooling-failure escape"
  else
    fail "$label: oos-ok waiver prohibition missing"
  fi

  # A removed step leaves dangling cross-references behind if the surrounding
  # prose is not updated with it. There is no fallback step, so the word should
  # not appear at all.
  if [[ "$section" == *fallback* ]]; then
    fail "$label: dangling reference to the removed fallback step"
  else
    pass "$label: no dangling fallback cross-reference"
  fi
done

# Both shipped paths must stay behaviorally identical, or one can regain the
# fallback while the other is clean.
a="$(section_of "${SURFACES[0]}")"
b="$(section_of "${SURFACES[1]}")"
if [[ "$a" == "$b" && -n "$a" ]]; then
  pass "both shipped instruction paths carry an identical tracking section"
else
  fail "shipped instruction paths diverged - one path can behave differently"
fi

# AC18-CON: an explicitly cited deferred line stays valid through the real gate,
# and the uncited line the flow now leaves behind is what the gate reds on.
if [[ -f "$GATE" ]]; then
  gate_exit() { PR_BODY="$1" bash "$GATE" >/dev/null 2>&1; echo $?; }

  got="$(gate_exit $'## Out of scope\n- Migration cleanup - tracked as cv-1383dc76')"
  if [[ "$got" == 0 ]]; then
    pass "cited cv- carveout line still passes the gate"
  else
    fail "cited cv- carveout line rejected by the gate (exit $got)"
  fi

  got="$(gate_exit $'## Out of scope\n- Migration cleanup - tracked as x-b6e2')"
  if [[ "$got" == 0 ]]; then
    pass "cited node line still passes the gate"
  else
    fail "cited node line rejected by the gate (exit $got)"
  fi

  got="$(gate_exit $'## Out of scope\n- Migration cleanup')"
  if [[ "$got" == 1 ]]; then
    pass "uncited line reds the gate (the recovery path the flow relies on)"
  else
    fail "uncited line did not red the gate (exit $got)"
  fi

  # With the fallback gone, the gate's help text is the only thing telling the
  # author what to do - so it must name the recoveries that do NOT mint an
  # object, not just the three that make the check pass by filing or waiving.
  help="$(PR_BODY=$'## Out of scope\n- Migration cleanup' bash "$GATE" 2>&1 >/dev/null)"
  if [[ "$help" == *"do the work"* && "$help" == *"or cut it"* ]]; then
    pass "gate recovery names inline-fix and removal, not only filing"
  else
    fail "gate recovery omits the inline-fix / removal options"
  fi
else
  fail "gate not found at $GATE"
fi

echo ""
echo "pr-oos-tracking-contract: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
