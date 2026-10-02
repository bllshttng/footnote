#!/usr/bin/env bash
# test_hook_budget.sh
#
# Contract for scripts/lib/hook-budget.sh, the one load-aware budget for the
# optional hook families: three tiers (idle 3s, loaded 1s, skip past the
# threshold), fail-open to the idle tier when load is unreadable, silence
# (empty output, status 0) on a fired bound or a skip, and a
# stale-while-revalidate cache whose refresh runs detached and whose stale
# copy is served when the live read skipped or failed.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LIB="$REPO_ROOT/scripts/lib/hook-budget.sh"

[[ -f "$LIB" ]] || { echo "FAIL: lib not found at $LIB" >&2; exit 1; }

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

TMP="$(mktemp -d -t hook-budget-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# Source the lib, then pin the load probe: the tier logic is what this suite
# guards, not the host's actual load. Tiers assert the real constants; the
# wall-clock cases pass "fast" to pin the idle tier to 1s.
load_lib() {
    # shellcheck source=scripts/lib/hook-budget.sh
    source "$LIB" || return 1
    if [[ "${1:-}" == "fast" ]]; then
        HOOK_BUDGET_IDLE_SECS=1
        HOOK_BUDGET_BUSY_SECS=1
    fi
}

echo "=== hook-budget tiers ==="

load_lib
hook_load1() { printf '1.0'; }
hook_cores() { printf '8'; }
out="$(hook_budget_secs)"
[[ "$out" == "3" ]] && pass "idle tier (load1 <= cores) -> 3s" || fail "idle tier: got '$out'"

load_lib
hook_load1() { printf '10.0'; }
hook_cores() { printf '8'; }
out="$(hook_budget_secs)"
[[ "$out" == "1" ]] && pass "loaded tier (cores < load1 <= 2x cores) -> 1s" || fail "loaded tier: got '$out'"

load_lib
hook_load1() { printf '17.0'; }
hook_cores() { printf '8'; }
out="$(hook_budget_secs)"
[[ "$out" == "0" ]] && pass "past threshold (load1 > 2x cores) -> skip" || fail "skip tier: got '$out'"

load_lib
hook_load1() { printf ''; }
out="$(hook_budget_secs)"
[[ "$out" == "3" ]] && pass "unreadable load -> idle tier (fail open)" || fail "unreadable load: got '$out'"

load_lib
hook_load1() { printf 'garbage'; }
out="$(hook_budget_secs)"
[[ "$out" == "3" ]] && pass "non-numeric load -> idle tier" || fail "garbage load: got '$out'"

echo "=== hook_run_optional ==="

load_lib fast
hook_load1() { printf '1.0'; }
hook_cores() { printf '8'; }
out="$(hook_run_optional bash -c 'echo through; exit 5')"
rc=$?
[[ "$out" == "through" && "$rc" == "5" ]] && pass "child stdout and status pass through" \
    || fail "passthrough: out='$out' rc=$rc"

out="$(hook_run_optional bash -c 'echo slow; sleep 30')"
rc=$?
[[ -z "$out" && "$rc" == "0" ]] && pass "fired bound reads as silence (empty, rc 0)" \
    || fail "bound: out='$out' rc=$rc"

load_lib
hook_load1() { printf '99.0'; }
hook_cores() { printf '8'; }
ran="$TMP/ran"
out="$(hook_run_optional touch "$ran")"
[[ ! -e "$ran" && -z "$out" ]] && pass "skip tier runs nothing, silent" \
    || fail "skip: file exists or out='$out'"

echo "=== hook_cache_serve ==="

load_lib fast
hook_load1() { printf '1.0'; }
hook_cores() { printf '8'; }
export FNO_HOOK_CACHE_DIR="$TMP/cache"
rm -rf "$FNO_HOOK_CACHE_DIR"

marker="$TMP/miss-marker"
out="$(hook_cache_serve k1 300 -- bash -c "echo live; touch $marker")"
[[ -e "$marker" && "$out" == "live" ]] && pass "cache miss runs the child and prints its output" \
    || fail "miss: marker=$([ -e "$marker" ] && echo yes || echo no) out='$out'"

rm -f "$marker"
out="$(hook_cache_serve k1 300 -- bash -c "touch $marker; echo SHOULD-NOT-RUN")"
[[ ! -e "$marker" && "$out" == "live" ]] && pass "fresh cache serves in milliseconds, child skipped" \
    || fail "hit: marker=$([ -e "$marker" ] && echo yes || echo no) out='$out'"

# A served copy past two thirds of its life arms the detached refresher; the
# refresher replaces the file for the NEXT boundary.
touch -t "$(date -v-250S +%Y%m%d%H%M.%S 2>/dev/null || date -d '250 seconds ago' +%Y%m%d%H%M.%S)" \
    "$FNO_HOOK_CACHE_DIR/k1"
rm -f "$marker"
out="$(hook_cache_serve k1 300 -- bash -c "echo refreshed; touch $marker")"
refreshed=no
for _ in 1 2 3 4 5 6 7 8 9 10; do
    grep -q refreshed "$FNO_HOOK_CACHE_DIR/k1" 2>/dev/null && { refreshed=yes; break; }
    sleep 0.5
done
[[ "$out" == "live" && -e "$marker" && "$refreshed" == "yes" ]] \
    && pass "aging serve prints the copy, refresher updates off the turn path" \
    || fail "refresh: out='$out' marker=$([ -e "$marker" ] && echo yes || echo no) refreshed=$refreshed"

# Expired cache + a live read that skips (past the threshold) serves stale.
load_lib
hook_load1() { printf '99.0'; }
hook_cores() { printf '8'; }
touch -t "$(date -v-400S +%Y%m%d%H%M.%S 2>/dev/null || date -d '400 seconds ago' +%Y%m%d%H%M.%S)" \
    "$FNO_HOOK_CACHE_DIR/k1"
out="$(hook_cache_serve k1 300 -- bash -c 'echo NEVER')"
[[ "$out" == "refreshed" ]] && pass "skip under load serves the stale copy, not nothing" \
    || fail "stale-serve: out='$out'"

echo
echo "hook-budget: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]] || exit 1
