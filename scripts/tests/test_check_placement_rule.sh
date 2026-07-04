#!/usr/bin/env bash
# Tests for scripts/ci/check-placement-rule.sh
# Runs against the REAL repo tree (the script resolves its own repo root via
# git rev-parse), injecting a throwaway offending file per case and removing
# it afterward. Never touches real state - only adds/removes a scratch file
# under version control that is deleted before this script exits.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
CHECK="$REPO_ROOT/scripts/ci/check-placement-rule.sh"
PASS=0
FAIL=0

if [[ ! -f "$CHECK" ]]; then
    echo "FAIL: $CHECK not found - cannot run tests"
    exit 1
fi

pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

# ---- T01: current tree is clean ----
echo "T01: clean tree passes"
bash "$CHECK" >/dev/null 2>&1
RC=$?
if [[ $RC -eq 0 ]]; then pass "rc=0 on the real tree"; else fail "rc=$RC (expected 0 - allowlist may need updating)"; fi

# ---- T02: a new .claude/ path construction is caught ----
echo "T02: new .claude/ reference is caught"
SCRATCH="$REPO_ROOT/cli/src/fno/_scratch_placement_test.py"
printf 'from pathlib import Path\nBAD = Path.home() / ".claude" / "new-state.json"\n' > "$SCRATCH"
OUT=$(bash "$CHECK" 2>&1)
RC=$?
rm -f "$SCRATCH"
if [[ $RC -ne 0 ]]; then pass "rc!=0 with new .claude/ reference"; else fail "rc=0, expected a caught violation"; fi
if echo "$OUT" | grep -q "_scratch_placement_test.py"; then
    pass "report names the offending file"
else
    fail "report did not name the offending file: $OUT"
fi

# ---- T03: a cwd-relative .fno/ write in hooks/*.sh is caught ----
echo "T03: bare .fno/ write in hooks/*.sh is caught"
SCRATCH="$REPO_ROOT/hooks/_scratch_placement_hook.sh"
printf '#!/usr/bin/env bash\necho "x" >> .fno/scratch-log.jsonl\n' > "$SCRATCH"
OUT=$(bash "$CHECK" 2>&1)
RC=$?
rm -f "$SCRATCH"
if [[ $RC -ne 0 ]]; then pass "rc!=0 with bare .fno/ write in a hook"; else fail "rc=0, expected a caught violation"; fi
if echo "$OUT" | grep -q "_scratch_placement_hook.sh"; then
    pass "report names the offending hook"
else
    fail "report did not name the offending hook: $OUT"
fi

# ---- T04: REPO_ROOT-anchored .fno/ write in hooks/*.sh is NOT flagged ----
echo "T04: \$REPO_ROOT-anchored .fno/ write is not flagged"
SCRATCH="$REPO_ROOT/hooks/_scratch_placement_hook_ok.sh"
printf '#!/usr/bin/env bash\necho "x" >> "${REPO_ROOT}/.fno/scratch-log.jsonl"\n' > "$SCRATCH"
OUT=$(bash "$CHECK" 2>&1)
RC=$?
rm -f "$SCRATCH"
if [[ $RC -eq 0 ]]; then pass "anchored write does not trip the check"; else fail "anchored write incorrectly flagged: $OUT"; fi

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
