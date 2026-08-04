#!/usr/bin/env bash
# test_target_claim_is_live.sh
#
# target_claim_is_live is the strict, claim-only liveness read every teardown
# guard needs. It exists because `kill -0 owner_pid` is false for EVERY live
# session (owner_pid is the transient `fno target init` wrapper pid), so the
# three guards that rested on it preserved nothing and would tear down a
# running target's worktree.
#
# The second half of this file is the part that matters: asserting the function
# is CALLED from all three guards. A correct helper wired into one of three
# reachable paths is decorative, and a unit test of the helper alone reads green
# while two guards stay broken.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
GUARD_LIB="$REPO_ROOT/scripts/lib/target-guard.sh"

[[ -f "$GUARD_LIB" ]] || { echo "FAIL: guard lib not found at $GUARD_LIB" >&2; exit 1; }
# shellcheck source=../../scripts/lib/target-guard.sh
source "$GUARD_LIB"

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t claim-live-XXXXXX)"
trap 'FNO_CLAIMS_ROOT="$TMP" fno claim release "node:x-live" >/dev/null 2>&1; rm -rf "$TMP"' EXIT
export FNO_CLAIMS_ROOT="$TMP"

fno claim acquire "node:x-live" --holder "target-session:test" --ttl 1h >/dev/null 2>&1 \
  || { echo "FAIL: could not acquire the fixture claim" >&2; exit 1; }

# A manifest carrying a dead owner_pid (the shape init always leaves behind)
# and the given claim key.
manifest() {
  local path="$1" claim_key="${2-}"
  mkdir -p "$(dirname "$path")"
  cat > "$path" <<'EOF'
---
session_id: sess-claim-live
owner_pid: 999999
input: x-live
plan_path: /tmp/plan
---
EOF
  [[ -n "$claim_key" ]] && printf 'target_claim_key: "%s"\n' "$claim_key" >> "$path"
  return 0
}

manifest "$TMP/live/.fno/target-state.md" "node:x-live"
target_claim_is_live "$TMP/live/.fno/target-state.md" \
  && pass "live claim + dead owner_pid -> live" || fail "live claim read as dead"

manifest "$TMP/free/.fno/target-state.md" "node:x-never-claimed-zzz"
target_claim_is_live "$TMP/free/.fno/target-state.md" \
  && fail "free claim read as live" || pass "free claim -> not live"

manifest "$TMP/nokey/.fno/target-state.md" ""
target_claim_is_live "$TMP/nokey/.fno/target-state.md" \
  && fail "absent claim key read as live" || pass "no claim key -> not live (strict)"

target_claim_is_live "$TMP/does-not-exist/target-state.md" \
  && fail "missing manifest read as live" || pass "missing manifest -> not live"

# target_is_active keeps the OPPOSITE bias on the same inputs: it fails OPEN,
# because there a false "dead" archives a running session.
target_is_active "$TMP/nokey/.fno/target-state.md" \
  && pass "target_is_active still fails open with no claim key" \
  || fail "target_is_active regressed to strict; a claimless live session now reads dead"
target_is_active "$TMP/free/.fno/target-state.md" \
  && fail "target_is_active called a free claim active" || pass "target_is_active: free claim -> inactive"

# Wiring. Each teardown guard must actually call the helper; a guard that only
# checks owner_pid is a guard that never fires.
for guard in hooks/worktree-remove.sh scripts/setup/archive-worktree.sh scripts/lib/worktree-lifecycle.sh; do
  grep -q 'target_claim_is_live' "$REPO_ROOT/$guard" \
    && pass "$guard consults the claim" \
    || fail "$guard still rests on owner_pid alone"
done

# And the PreCompact arm hook must not have regrown a bare owner_pid gate.
grep -qE 'kill -0 .*OWNER_PID' "$REPO_ROOT/hooks/arm-handoff-precompact.sh" \
  && fail "arm-handoff-precompact.sh regrew a bare owner_pid liveness gate" \
  || pass "arm-handoff-precompact.sh has no bare owner_pid gate"

echo ""
echo "target_claim_is_live: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
