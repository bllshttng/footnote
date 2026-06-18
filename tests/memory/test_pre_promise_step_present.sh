#!/usr/bin/env bash
# Test: pre-promise.md contains the memory pass step with correct structure.
#
# Checks:
#   1. Heading "Memory pass" exists in the file.
#   2. Key prose elements are present (decision bar, write-memory-entry.sh, real signature flags).
#   3. The memory pass step appears AFTER the Pre-Promise Self-Check section and BEFORE the promise output.
#   4. The recipe uses the REAL writer signature (--memory-dir, --session-id, --candidate)
#      and NOT a positional-arg form like "feedback <name>".

set -euo pipefail

PASS_COUNT=0
FAIL_COUNT=0

PRE_PROMISE="$(cd "$(dirname "$0")/../.." && pwd)/skills/target/references/pre-promise.md"

pass() { echo "  PASS: $1"; PASS_COUNT=$((PASS_COUNT + 1)); }
fail() { echo "  FAIL: $1"; FAIL_COUNT=$((FAIL_COUNT + 1)); }

echo "=== test_pre_promise_step_present.sh ==="
echo "File: $PRE_PROMISE"
echo ""

# -----------------------------------------------------------------------
# Check 1: heading exists
# -----------------------------------------------------------------------
if grep -qi "Memory pass" "$PRE_PROMISE"; then
    pass "AC1: heading 'Memory pass' (or 'Memory Pass') present"
else
    fail "AC1: heading 'Memory pass' not found in pre-promise.md"
fi

# -----------------------------------------------------------------------
# Check 2: key prose elements
# -----------------------------------------------------------------------
if grep -q "would removing this from memory" "$PRE_PROMISE"; then
    pass "AC2-a: decision bar phrase present"
else
    fail "AC2-a: decision bar phrase 'would removing this from memory' not found"
fi

if grep -q "write-memory-entry.sh" "$PRE_PROMISE"; then
    pass "AC2-b: write-memory-entry.sh referenced"
else
    fail "AC2-b: write-memory-entry.sh not referenced in pre-promise.md"
fi

# -----------------------------------------------------------------------
# Check 3: real writer signature flags
# -----------------------------------------------------------------------
if grep -q "\-\-memory-dir" "$PRE_PROMISE"; then
    pass "AC3-a: --memory-dir flag present"
else
    fail "AC3-a: --memory-dir flag not found (wrong signature used)"
fi

if grep -q "\-\-session-id" "$PRE_PROMISE"; then
    pass "AC3-b: --session-id flag present"
else
    fail "AC3-b: --session-id flag not found (wrong signature used)"
fi

if grep -q "\-\-candidate" "$PRE_PROMISE"; then
    pass "AC3-c: --candidate flag present"
else
    fail "AC3-c: --candidate flag not found (wrong signature used)"
fi

# -----------------------------------------------------------------------
# Check 4: recipe does NOT use old positional-arg form
# -----------------------------------------------------------------------
# Old (wrong) form from the plan: "feedback <name> <desc> <body_path>"
# The real form uses --candidate JSON; there should be no "feedback <name>" positional call.
if grep -qE 'write-memory-entry\.sh[[:space:]]+(feedback|project)[[:space:]]+[^-]' "$PRE_PROMISE"; then
    fail "AC4: recipe uses WRONG positional-arg signature (feedback|project <name> ...)"
else
    pass "AC4: recipe does not use wrong positional-arg signature"
fi

# -----------------------------------------------------------------------
# Check 5: ordering - Memory pass appears AFTER Pre-Promise Self-Check, BEFORE promise output
# -----------------------------------------------------------------------
self_check_line=$(grep -ni "## Pre-Promise Self-Check" "$PRE_PROMISE" | head -1 | cut -d: -f1)
memory_pass_line=$(grep -in "## Memory Pass" "$PRE_PROMISE" | head -1 | cut -d: -f1)
promise_line=$(grep -n "Promise Output" "$PRE_PROMISE" | head -1 | cut -d: -f1)

if [[ -z "$self_check_line" || -z "$memory_pass_line" || -z "$promise_line" ]]; then
    fail "AC5: could not locate all three section lines (self_check=$self_check_line memory_pass=$memory_pass_line promise=$promise_line)"
else
    if [[ "$memory_pass_line" -gt "$self_check_line" && "$memory_pass_line" -lt "$promise_line" ]]; then
        pass "AC5: Memory pass (line $memory_pass_line) is after Pre-Promise Self-Check (line $self_check_line) and before Promise Output (line $promise_line)"
    else
        fail "AC5: Memory pass (line $memory_pass_line) is NOT correctly positioned between Pre-Promise Self-Check (line $self_check_line) and Promise Output (line $promise_line)"
    fi
fi

# -----------------------------------------------------------------------
# Check 6: step is non-fatal (mentions log warning + does not block promise)
# -----------------------------------------------------------------------
if grep -q "non-fatal\|non.fatal\|does not block\|do not block\|warning" "$PRE_PROMISE"; then
    pass "AC6: non-fatal language present"
else
    fail "AC6: no non-fatal / warning language found for writer-absent case"
fi

# -----------------------------------------------------------------------
# Check 7: silence-is-fine instruction present
# -----------------------------------------------------------------------
if grep -q "write nothing\|silence is fine\|skip\|zero candidate" "$PRE_PROMISE"; then
    pass "AC7: zero-candidates / silence-is-fine guidance present"
else
    fail "AC7: zero-candidates / silence-is-fine guidance not found"
fi

# -----------------------------------------------------------------------
# Check 8: MEMORY_DIR is dynamically constructed AND resolves the CANONICAL
# repo root, not the worktree. Hardcoding would write to the wrong dir for
# every contributor except Jason; `git rev-parse --show-toplevel` returns the
# worktree path, which slash-encodes to a different dir and splits memory when
# run from a conductor worktree. The recipe must derive the canonical root from
# the common git-dir and slash-encode it, matching skills/pr/check.md.
# -----------------------------------------------------------------------
if grep -qE 'MEMORY_DIR="[^"]*projects/-Users-[a-z0-9-]+/memory"' "$PRE_PROMISE"; then
    fail "AC8: MEMORY_DIR appears hardcoded to a literal slash-encoded path; derive it dynamically"
elif grep -q -- '--git-common-dir' "$PRE_PROMISE" && grep -qF "sed 's|/|-|g'" "$PRE_PROMISE"; then
    pass "AC8: MEMORY_DIR derives the canonical root from git --git-common-dir + slash-encoding"
else
    fail "AC8: MEMORY_DIR construction not found or unrecognized (expected canonical git-common-dir scheme)"
fi

# -----------------------------------------------------------------------
# Check 9: pre-promise MEMORY_DIR scheme matches check-pr SKILL.md scheme.
# Both checkpoints MUST land entries in the same dir, and both must resolve the
# CANONICAL root - drift here (or one using --show-toplevel from a worktree)
# means pre-promise writes to project-A while post-merge writes to project-B,
# silently splitting the index.
# -----------------------------------------------------------------------
CHECK_PR="$(cd "$(dirname "$0")/../.." && pwd)/skills/pr/check.md"
if [[ -f "$CHECK_PR" ]]; then
    if grep -q -- '--git-common-dir' "$CHECK_PR" && grep -qF "sed 's|/|-|g'" "$CHECK_PR"; then
        pass "AC9: check-pr SKILL.md uses the same canonical MEMORY_DIR scheme"
    else
        fail "AC9: check-pr SKILL.md does not use the same canonical MEMORY_DIR scheme; the two checkpoints will write to different directories"
    fi
fi

# -----------------------------------------------------------------------
# Summary
# -----------------------------------------------------------------------
echo ""
echo "Results: $PASS_COUNT passed, $FAIL_COUNT failed"
if [[ "$FAIL_COUNT" -gt 0 ]]; then
    exit 1
fi
exit 0
