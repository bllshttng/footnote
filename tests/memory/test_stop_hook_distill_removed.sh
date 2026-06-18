#!/usr/bin/env bash
# Tests that the old Haiku distill block and recursion guard are gone from the stop hook,
# but the Task-1.2 post-merge fallback block is still present.
# AC2.1-HP / AC2.1-EDGE

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HOOK="$REPO_ROOT/hooks/target-stop-hook.sh"
PASS=0
FAIL=0

pass() { echo "PASS: $1"; PASS=$((PASS+1)); }
fail() { echo "FAIL: $1"; FAIL=$((FAIL+1)); }

# Strip comment lines (lines starting with optional whitespace then #),
# then check for active-code matches.
non_comment_lines() {
    grep -v '^[[:space:]]*#' "$HOOK"
}

# AC2.1-HP-1: DISTILL_SCRIPT= assignment not present in active code
if non_comment_lines | grep -q 'DISTILL_SCRIPT='; then
    fail "DISTILL_SCRIPT= still present in active code"
else
    pass "DISTILL_SCRIPT= not present in active code"
fi

# AC2.1-HP-2: bash \"\$DISTILL_SCRIPT\" invocation not present in active code
if non_comment_lines | grep -q 'bash.*DISTILL_SCRIPT'; then
    fail "bash \"\$DISTILL_SCRIPT\" invocation still present in active code"
else
    pass "bash \"\$DISTILL_SCRIPT\" invocation not present in active code"
fi

# AC2.1-EDGE-3: TARGET_INSIDE_DISTILL recursion guard early-return is gone
# The guard was: if [[ "${TARGET_INSIDE_DISTILL:-0}" == "1" ]]; then ... exit 0; fi
if non_comment_lines | grep -q 'TARGET_INSIDE_DISTILL.*==.*1'; then
    fail "TARGET_INSIDE_DISTILL recursion guard still present in active code"
else
    pass "TARGET_INSIDE_DISTILL recursion guard removed from active code"
fi

# AC2.1-HP-4: post-merge-pass sentinel check still present (Task 1.2 block preserved)
if grep -q '\.memory-pass-pending' "$HOOK"; then
    pass ".memory-pass-pending sentinel check still present"
else
    fail ".memory-pass-pending sentinel check MISSING (Task 1.2 block was removed!)"
fi

# AC2.1-HP-5: post-merge-pass.sh invocation still present
if grep -q 'post-merge-pass\.sh' "$HOOK"; then
    pass "post-merge-pass.sh invocation still present"
else
    fail "post-merge-pass.sh invocation MISSING (Task 1.2 block was removed!)"
fi

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
