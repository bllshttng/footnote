#!/usr/bin/env bash
# Unit tests for scripts/lib/loadgen.sh.
#
# The guarantees are live-process ones, so this spawns real (short-bounded)
# generators rather than fabricating state: the bound must fire unattended, the
# names must be findable by BOTH stop's pgrep pattern and the orphan sweep's
# attribution (which reads argv[0], never psutil.name() - the trap measured in
# orphans.py display_name), and stop must not reach a longer label that shares
# a prefix.
#
# Runs under /bin/bash explicitly; bash 3.2 is the floor.
#
# Exit codes: 0 all passed, 1 assertion failed, 77 skipped.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LOADGEN="$REPO_ROOT/scripts/lib/loadgen.sh"

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

[[ -f "$LOADGEN" ]] || { echo "FAIL: helper not found at $LOADGEN" >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "SKIP: python3 not on PATH" >&2; exit 77; }
command -v pgrep >/dev/null 2>&1 || { echo "SKIP: pgrep not on PATH" >&2; exit 77; }

bash -n "$LOADGEN" || fail "bash -n rejected $LOADGEN"

LABEL="lgtest$$"

cleanup() {
  bash "$LOADGEN" stop "$LABEL" >/dev/null 2>&1 || true
  pkill -f "^fno-load-${LABEL}" 2>/dev/null || true
}
trap cleanup EXIT

count_live() {
  pgrep -f "^fno-load-${LABEL}(-[0-9]+)?$" 2>/dev/null | grep -c . || true
}

# --- start: two named generators appear under the label's pattern ----------
bash "$LOADGEN" start "$LABEL" 25 2 >/dev/null 2>&1
sleep 0.5
N="$(count_live)"
if [[ "$N" -eq 2 ]]; then
  pass "start: 2 generators matched the label pattern"
else
  fail "start: expected 2 live generators, found $N"
fi

FIRST_PID="$(pgrep -f "^fno-load-${LABEL}-1$" 2>/dev/null | head -1)"
if [[ -n "$FIRST_PID" ]]; then
  pass "start: per-index name fno-load-<label>-<i> is pgrep-visible"
else
  fail "start: fno-load-${LABEL}-1 not found by exact-name pgrep"
fi

# --- the sweep-shape check: argv[0] carries the name, not the executable ---
# This is the documented detection trap: ps -o comm and psutil.name() both read
# the executable (`yes`), and only the argument vector carries the rename. The
# orphan sweep attributes on argv[0], so proving the emitted shape satisfies
# the sweep's own predicate is what makes `fno agents orphans` a net for a
# generator whose creator died before the bound fired.
SWEEP_SHAPE="$(ps -o command= -p "$FIRST_PID" 2>/dev/null | awk '{print $1}')"
python3 - "$SWEEP_SHAPE" <<'EOF' && pass "names satisfy the sweep's attribution (argv[0], renamed)" \
  || fail "sweep attribution rejected the emitted name: $SWEEP_SHAPE"
import sys
sys.path.insert(0, "cli/src")
from fno.agents import orphans
argv0 = sys.argv[1]
info = {"cmdline": [argv0], "name": "yes", "exe": "/bin/yes"}
name = orphans.display_name(info)
assert name.startswith(orphans.FNO_PREFIX), name
assert orphans.was_renamed(info), name
EOF

# --- list reports a positive count under the listing ------------------------
LIST_OUT="$(bash "$LOADGEN" list "$LABEL" 2>/dev/null)"
if printf '%s' "$LIST_OUT" | grep -q "2 live generator(s)"; then
  pass "list: count 2 printed under the pid listing"
else
  fail "list: unexpected output: $LIST_OUT"
fi

# --- stop: kills its label, leaves a sharing-prefix label alone -------------
bash "$LOADGEN" start "${LABEL}x" 25 1 >/dev/null 2>&1
sleep 0.3
STOP_OUT="$(bash "$LOADGEN" stop "$LABEL" 2>/dev/null)"
if printf '%s' "$STOP_OUT" | grep -q "matched 2, remaining 0"; then
  pass "stop: matched 2, remaining 0 (absence proven against a positive)"
else
  fail "stop: unexpected receipt: $STOP_OUT"
fi
if pgrep -f "^fno-load-${LABEL}x-1$" >/dev/null 2>&1; then
  pass "stop: label ${LABEL} did not reach ${LABEL}x"
else
  fail "stop: prefix-sharing label ${LABEL}x was killed by stop ${LABEL}"
fi
bash "$LOADGEN" stop "${LABEL}x" >/dev/null 2>&1

# --- the bound fires with nobody watching -----------------------------------
bash "$LOADGEN" start "$LABEL" 3 1 >/dev/null 2>&1
sleep 6
if pgrep -f "^fno-load-${LABEL}" >/dev/null 2>&1; then
  fail "bound: generator survived past its 3s ceiling"
else
  pass "bound: 3s ceiling killed the unattended generator"
fi

# --- validation: malformed input starts nothing -----------------------------
bad() {
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then
    fail "validation: $desc was accepted"
  else
    pass "validation: $desc refused (exit 2)"
  fi
}
bad "label with a space"   bash "$LOADGEN" start "bad label" 10
bad "label with a dot"     bash "$LOADGEN" stop "x.y"
bad "suffixed seconds"     bash "$LOADGEN" start ok 10m
bad "zero seconds"         bash "$LOADGEN" start ok 0
bad "zero count"           bash "$LOADGEN" start ok 10 0
bad "count over cap"       bash "$LOADGEN" start ok 10 33
bad "missing bound"        bash "$LOADGEN" start ok
if [[ "$(count_live)" -eq 0 ]]; then
  pass "validation: no generator left behind by refusals"
else
  fail "validation: refusals left $(count_live) generator(s) running"
fi

echo
echo "loadgen: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
