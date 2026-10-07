#!/usr/bin/env bash
# test_hook_budget.sh
#
# Contract for scripts/lib/hook-budget.sh, the one load-aware budget for the
# optional hook families: four tiers (idle 3s, loaded 1s, overloaded skip via
# hook_overloaded, unreadable fail-open to the idle tier), silence
# (empty output, status 0) on a fired bound, and a
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
[[ "$out" == "1" ]] && pass "past threshold (load1 > cores) -> busy floor, never skip" || fail "threshold tier: got '$out'"

load_lib
hook_load1() { printf ''; }
out="$(hook_budget_secs)"
[[ "$out" == "3" ]] && pass "unreadable load -> idle tier (fail open)" || fail "unreadable load: got '$out'"

load_lib
hook_load1() { printf 'garbage'; }
out="$(hook_budget_secs)"
[[ "$out" == "3" ]] && pass "non-numeric load -> idle tier" || fail "garbage load: got '$out'"

echo "=== hook_overloaded ==="

# One truth table, one case: load cores [pin] -> expect overloaded (1) or not (0).
load_lib
ovl_fail=0
while read -r ovl_load ovl_cores ovl_pin ovl_want; do
    [[ "$ovl_load" == "-" ]] && continue
    hook_load1() { printf '%s' "$ovl_load"; }
    hook_cores() { printf '%s' "$ovl_cores"; }
    unset FNO_HOOK_BUDGET_SKIP_PER_CORE
    [[ "$ovl_pin" != "-" ]] && FNO_HOOK_BUDGET_SKIP_PER_CORE="$ovl_pin"
    if hook_overloaded; then ovl_got=1; else ovl_got=0; fi
    unset FNO_HOOK_BUDGET_SKIP_PER_CORE
    [[ "$ovl_got" == "$ovl_want" ]] || { ovl_fail=1; echo "    case [$ovl_load $ovl_cores $ovl_pin] -> got $ovl_got, want $ovl_want"; }
done <<'CASES'
70.0 8 - 1
20.0 8 - 0
- 8 - 0
garbage 8 - 0
70.0 8 1000 0
1.0 8 0 1
CASES
[[ $ovl_fail -eq 0 ]] \
    && pass "truth table: 8.75x skips, 2.5x and unreadable never skip, pin raises it, 0 pins it on" \
    || fail "hook_overloaded truth table"

# The sysctl branch (macOS) parses the brace-wrapped 3-tuple: the braces
# word-split into their own tokens (5 fields), so the average is field 2.
# The old 3-or-4 case matched none of them and the probe read as empty, so
# the busy tier never engaged on macOS. Linux always reads /proc, so this
# case can only run where /proc is absent.
if [[ ! -r /proc/loadavg ]]; then
    load_lib
    FAKEBIN="$TMP/fakesysctl"
    mkdir -p "$FAKEBIN"
    printf '#!/bin/sh\necho "{ 7.77 2.90 2.71 }"\n' > "$FAKEBIN/sysctl"
    chmod +x "$FAKEBIN/sysctl"
    ovl_got="$(PATH="$FAKEBIN:$PATH" hook_load1)"
    [[ "$ovl_got" == "7.77" ]] \
        && pass "sysctl brace-wrapped 3-tuple parses to the one-minute average" \
        || fail "sysctl parse: got '$ovl_got'"
fi

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

load_lib fast
hook_load1() { printf '99.0'; }
hook_cores() { printf '8'; }
ran="$TMP/ran"
out="$(hook_run_optional bash -c 'sleep 30; echo late')"
rc=$?
[[ ! -e "$ran" && -z "$out" && "$rc" == "0" ]] && pass "past threshold the read still runs, bounded at the busy tier" \
    || fail "threshold run: out='$out' rc=$rc"

echo "=== hook_cache_serve ==="

load_lib fast
hook_load1() { printf '1.0'; }
hook_cores() { printf '8'; }
export FNO_HOOK_CACHE_DIR="$TMP/cache"
rm -rf "$FNO_HOOK_CACHE_DIR"

marker="$TMP/miss-marker"
out="$(hook_cache_serve k1 300 "" -- bash -c "echo live; touch $marker")"
[[ -e "$marker" && "$out" == "live" ]] && pass "cache miss runs the child and prints its output" \
    || fail "miss: marker=$([ -e "$marker" ] && echo yes || echo no) out='$out'"

rm -f "$marker"
out="$(hook_cache_serve k1 300 "" -- bash -c "touch $marker; echo SHOULD-NOT-RUN")"
[[ ! -e "$marker" && "$out" == "live" ]] && pass "fresh cache serves in milliseconds, child skipped" \
    || fail "hit: marker=$([ -e "$marker" ] && echo yes || echo no) out='$out'"

# A fingerprint mismatch means the input changed: the copy must NOT serve.
out="$(hook_cache_serve k1 300 newinput -- bash -c "echo remeasured")"
[[ "$out" == "remeasured" ]] && pass "changed fingerprint forces the live read" \
    || fail "fingerprint: out='$out'"

# A served copy past two thirds of its life arms the detached refresher; the
# refresher replaces the file for the NEXT boundary. Fresh key: seed it, age
# it, serve it.
out="$(hook_cache_serve k2 300 "" -- bash -c "echo live")"
touch -t "$(date -v-250S +%Y%m%d%H%M.%S 2>/dev/null || date -d '250 seconds ago' +%Y%m%d%H%M.%S)" \
    "$FNO_HOOK_CACHE_DIR/k2"
rm -f "$marker"
out="$(hook_cache_serve k2 300 "" -- bash -c "echo refreshed; touch $marker")"
refreshed=no
for _ in 1 2 3 4 5 6 7 8 9 10; do
    grep -q refreshed "$FNO_HOOK_CACHE_DIR/k2" 2>/dev/null && { refreshed=yes; break; }
    sleep 0.5
done
[[ "$out" == "live" && -e "$marker" && "$refreshed" == "yes" ]] \
    && pass "aging serve prints the copy, refresher updates off the turn path" \
    || fail "refresh: out='$out' marker=$([ -e "$marker" ] && echo yes || echo no) refreshed=$refreshed"

# Expired cache + a live read that expires under the busy-tier bound (a hung
# child) serves stale.
load_lib
hook_load1() { printf '99.0'; }
hook_cores() { printf '8'; }
touch -t "$(date -v-400S +%Y%m%d%H%M.%S 2>/dev/null || date -d '400 seconds ago' +%Y%m%d%H%M.%S)" \
    "$FNO_HOOK_CACHE_DIR/k2"
out="$(hook_cache_serve k2 300 "" -- bash -c 'sleep 5; echo NEVER')"
[[ "$out" == "refreshed" ]] && pass "an expired live read serves the stale copy, not nothing" \
    || fail "stale-serve: out='$out'"

echo
echo "hook-budget: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]] || exit 1
