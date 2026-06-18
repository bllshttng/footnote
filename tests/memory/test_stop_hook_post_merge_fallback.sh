#!/usr/bin/env bash
# Tests for the post-merge fallback block added to hooks/target-stop-hook.sh (Task 1.2).
# Updated by Task 2.1 (memory-pass-redesign ab-3e75dff1): the old distill-block
# regression guards are removed because Task 2.1 intentionally deleted that code.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
STOP_HOOK="$REPO_ROOT/hooks/target-stop-hook.sh"

pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*" >&2; FAILURES=$((FAILURES + 1)); }

FAILURES=0

[[ -f "$STOP_HOOK" ]] || { echo "FAIL: target-stop-hook.sh not found at $STOP_HOOK" >&2; exit 1; }

# -------------------------------------------------------------------
# AC1.2: New post-merge fallback block references sentinel file
# -------------------------------------------------------------------
if grep -q '\.memory-pass-pending' "$STOP_HOOK"; then
    pass "AC1.2: .memory-pass-pending sentinel reference found in target-stop-hook.sh"
else
    fail "AC1.2: .memory-pass-pending not found in target-stop-hook.sh (new block missing)"
fi

# -------------------------------------------------------------------
# AC1.2: New block references post-merge-pass.sh
# -------------------------------------------------------------------
if grep -q 'post-merge-pass\.sh' "$STOP_HOOK"; then
    pass "AC1.2: post-merge-pass.sh reference found in target-stop-hook.sh"
else
    fail "AC1.2: post-merge-pass.sh not found in target-stop-hook.sh (new block missing)"
fi

# -------------------------------------------------------------------
# Post-Task-2.1: old distill block and recursion guard are GONE (that is correct)
# -------------------------------------------------------------------
if ! grep -q 'distill-session\.sh' "$STOP_HOOK"; then
    pass "Post-2.1: distill-session.sh correctly removed from target-stop-hook.sh"
else
    fail "Post-2.1: distill-session.sh still present in target-stop-hook.sh (Task 2.1 should have removed it)"
fi

if ! grep -q 'TARGET_INSIDE_DISTILL' "$STOP_HOOK"; then
    pass "Post-2.1: TARGET_INSIDE_DISTILL recursion guard correctly removed from target-stop-hook.sh"
else
    fail "Post-2.1: TARGET_INSIDE_DISTILL still present in target-stop-hook.sh (Task 2.1 should have removed it)"
fi

# -------------------------------------------------------------------
# AC1.2: New block is non-blocking (has '|| log' or '|| true' wrapping)
# -------------------------------------------------------------------
if grep -A5 'post-merge-pass\.sh' "$STOP_HOOK" | grep -qE '\|\| log|\|\| true'; then
    pass "AC1.2: post-merge-pass.sh invocation is non-blocking (|| log or || true present)"
else
    fail "AC1.2: post-merge-pass.sh invocation missing non-fatal guard (|| log or || true)"
fi

echo ""
if [[ "$FAILURES" -eq 0 ]]; then
    echo "ALL TESTS PASSED (test_stop_hook_post_merge_fallback.sh)"
else
    echo "FAILED: $FAILURES test(s) failed" >&2
    exit 1
fi
