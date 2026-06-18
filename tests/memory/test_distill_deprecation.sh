#!/usr/bin/env bash
# Tests that distill-session.sh is now a deprecation stub.
# AC2.1-FR: bash scripts/memory/distill-session.sh exits 0 with deprecation message.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DISTILL="$REPO_ROOT/scripts/memory/distill-session.sh"
PASS=0
FAIL=0

pass() { echo "PASS: $1"; PASS=$((PASS+1)); }
fail() { echo "FAIL: $1"; FAIL=$((FAIL+1)); }

# AC2.1-FR-1: exits 0
stderr_out=$("$DISTILL" 2>&1 1>/dev/null) || true
exit_code=0
"$DISTILL" >/dev/null 2>/dev/null; exit_code=$?
if [[ "$exit_code" -eq 0 ]]; then
    pass "distill-session.sh exits 0"
else
    fail "distill-session.sh exits $exit_code (expected 0)"
fi

# AC2.1-FR-2: deprecation message on stderr contains "DEPRECATED"
stderr_out=$("$DISTILL" 2>&1 >/dev/null)
if echo "$stderr_out" | grep -q "DEPRECATED"; then
    pass "stderr contains DEPRECATED"
else
    fail "stderr missing DEPRECATED (got: $stderr_out)"
fi

# AC2.1-FR-3: stderr mentions post-merge
if echo "$stderr_out" | grep -q "post-merge"; then
    pass "stderr mentions post-merge"
else
    fail "stderr missing post-merge (got: $stderr_out)"
fi

# AC2.1-FR-4: script writes nothing to a memory dir
TMP_MEM=$(mktemp -d)
MEMORY_DIR_OVERRIDE="$TMP_MEM" "$DISTILL" >/dev/null 2>/dev/null || true
file_count=$(find "$TMP_MEM" -type f | wc -l | tr -d ' ')
rm -rf "$TMP_MEM"
if [[ "$file_count" -eq 0 ]]; then
    pass "distill-session.sh writes nothing to memory dir"
else
    fail "distill-session.sh wrote $file_count files to memory dir (expected 0)"
fi

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
