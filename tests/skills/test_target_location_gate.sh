#!/usr/bin/env bash
# test_target_location_gate.sh - pins skills/target/SKILL.md's location gate
# to the exact text that stalled a worker for 50 minutes (x-1182).
#
# 2026-09-03: an autonomous target spawn hit `verdict=canonical-protected`,
# and the skill's own text told it to OFFER a `[Y/n]` worktree prompt on an
# attended run. hooks/helpers/check-impl-location.sh emits no attendance key
# and runs before `fno do target init` (the only thing that resolves
# attendance), so the branch had no machine input and the model defaulted to
# asking. The fix deletes the offer/refuse fork entirely and routes through
# `fno do target start <node>`, the one-verb cold start, for attended and
# unattended alike.
#
# Positive-marker discipline (AGENTS.md pitfalls corpus): asserting only an
# ABSENCE of [Y/n] proves nothing if the section extractor silently matched
# zero lines. So this test asserts the section is found (non-empty) BEFORE
# asserting anything about its content - a renamed or deleted section fails
# loudly here instead of passing vacuously.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SKILL_MD="$REPO_ROOT/skills/target/SKILL.md"

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

[[ -f "$SKILL_MD" ]] || { echo "missing: $SKILL_MD" >&2; exit 1; }

# ---- extract the location-gate bullet -------------------------------------
# The bullet starts at "- **HARD-GATE (location)" and runs until the next
# top-level "- **" bullet at the same indentation (the MANDATORY bootstrap
# bullet that follows it).
SECTION=$(awk '
  /^- \*\*HARD-GATE \(location\)/ { grabbing = 1 }
  grabbing && /^- \*\*MANDATORY/ && !/HARD-GATE/ { exit }
  grabbing { print }
' "$SKILL_MD")

# AC2-ERR: the extraction itself must be a positive marker. An empty result
# means the section was renamed or removed, and every assertion below would
# otherwise pass vacuously on nothing.
if [[ -z "$SECTION" ]]; then
  fail "location-gate section extraction is EMPTY - the bullet was renamed, moved, or removed; fix the extractor or the section before trusting any assertion below"
else
  pass "location-gate section extracted (${#SECTION} bytes)"
fi

# AC1-HP: the fixed text routes through the one-verb cold start.
if [[ -n "$SECTION" ]] && grep -q 'fno do target start' <<<"$SECTION"; then
  pass "section names 'fno do target start'"
else
  fail "section does not name 'fno do target start' - the [Y/n] leg was not replaced"
fi

# AC1-HP: no interactive Y/n-shaped prompt survives, in any of its spellings.
if [[ -n "$SECTION" ]] && grep -qE '\[Y/n\]|\[y/N\]|\(y/N\)|\(Y/n\)' <<<"$SECTION"; then
  fail "section still contains an interactive [Y/n]-shaped prompt - an autonomous spawn cannot answer it"
else
  pass "no [Y/n]-shaped prompt in the location gate"
fi

# AC1-HP: no hardcoded conductor base path (the second worktree-creation leg
# this plan deletes; .claude/rules/worktrees.md forbids hardcoding a base).
if [[ -n "$SECTION" ]] && grep -q 'conductor/workspaces' <<<"$SECTION"; then
  fail "section still hardcodes a conductor/workspaces base path"
else
  pass "no hardcoded conductor/workspaces path in the location gate"
fi

echo ""
echo "Passed: $PASS, Failed: $FAIL"
[[ $FAIL -eq 0 ]]
