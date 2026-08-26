#!/usr/bin/env bash
# Router contract tests for skills/review/SKILL.md after the sigma retirement.
#
# The router is skill prose, so its contract is pinned textually: every
# assertion names a marker the outcome PRODUCES (a route line, a refusal
# naming its replacement), never a bare absence. Two-sided by design - a
# route line alone does not prove the fan-out is gone, and a zero-dispatch
# line alone does not prove the replacement ran.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
ROUTER="$REPO_ROOT/skills/review/SKILL.md"
LANE="$REPO_ROOT/skills/review/references/single-lane.md"

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

has() { grep -qF -- "$2" "$1"; }

echo "== AC2-MARKER: bare invocation prints the route line AND dispatches zero subagents"
if has "$ROUTER" 'running fno review lane (default, level from diff)'; then
  pass "bare route line present"
else
  fail "bare route line missing"
fi
if has "$ROUTER" 'dispatches ZERO review subagents'; then
  pass "zero-dispatch marker present in the router"
else
  fail "zero-dispatch marker missing from the router"
fi
if has "$LANE" 'do not spawn subagents'; then
  pass "lane reference forbids fan-out"
else
  fail "lane reference lost the fan-out prohibition"
fi

echo "== AC2-ERR: the retired token refuses and names the replacement"
if has "$ROUTER" 'refused: sigma is retired; the default review lane replaced it'; then
  pass "sigma refusal names the default lane as the replacement"
else
  fail "sigma refusal does not name the replacement"
fi
if has "$ROUTER" 'run /fno:review [level] [<target>]'; then
  pass "the refusal carries a runnable replacement command"
else
  fail "the refusal carries no runnable replacement"
fi

echo "== AC2-EDGE: ultra is refused with the standing-rule reason"
if has "$ROUTER" 'refused: ultra is billed separately and no fno surface issues it; use max'; then
  pass "ultra refusal states the standing-rule reason"
else
  fail "ultra refusal missing or unexplained"
fi

echo "== AC2-HP: the surviving modes keep their route lines"
for marker in 'running peer review (cross-model)' 'running research-verify (advisory)' 'emitting self-cert attestation (declare)' 'running prove-it (runtime evidence)' 'running cleanup (apply-or-skip)'; do
  if has "$ROUTER" "$marker"; then pass "route line kept: $marker"; else fail "route line lost: $marker"; fi
done

echo "== AC7-ERR: cleanup runs inline on every harness"
if has "$ROUTER" 'runs inline on every harness'; then
  pass "cleanup route names every-harness availability"
else
  fail "cleanup harness availability not stated in the router"
fi
if [ -e "$REPO_ROOT/skills/review/references/cleanup.md" ]; then
  pass "cleanup reference exists"
else
  fail "cleanup reference missing"
fi

echo "== the grammar teaches the level surface"
if has "$ROUTER" 'low` `medium` `high` `xhigh` `max'; then
  pass "level tokens enumerated in the grammar"
else
  fail "level tokens not enumerated"
fi
if has "$ROUTER" 'A level is never inherited from a previous invocation'; then
  pass "no last-used inheritance, stated"
else
  fail "last-used inheritance prohibition missing"
fi

echo "== the retired references are gone and cannot be loaded"
for gone in references/sigma.md references/agent-selection.md; do
  if [ -e "$REPO_ROOT/skills/review/$gone" ]; then fail "$gone still exists"; else pass "$gone deleted"; fi
done
if grep -q 'references/sigma.md' "$ROUTER"; then
  fail "router still points at the deleted sigma reference"
else
  pass "router no longer points at the deleted reference"
fi

echo "== the six specialist hunters stay individually invocable"
for agent in silent-failure-hunter integration-test-analyzer multi-device-checker ux-flow-tester code-reviewer type-design-analyzer; do
  if [ -f "$REPO_ROOT/agents/$agent.md" ]; then pass "agent resolvable: $agent"; else fail "agent missing: $agent"; fi
done

echo
echo "review-router: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
