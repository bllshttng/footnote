#!/usr/bin/env bash
# test_hermetic_lanes.sh - the dirty lane must be able to go red.
#
# `fno test smoke --ambient dirty` claims that a green run means the ambient
# surface is complete. That claim rests entirely on the lane being ABLE to
# fail: a success condition built on an absence cannot distinguish "no leaks"
# from "the instrument never ran".
#
# So this asserts both halves against the canary, which reads a channel
# fno/hermetic.py deliberately does not scrub:
#
#   clean lane -> canary unset  -> the canary test PASSES
#   dirty lane -> canary leaks  -> the canary test FAILS
#
# A failure of the second half means the positive control has been disarmed -
# usually by someone widening a scrub rule until it swallows the canary - and
# the dirty lane has silently become decorative.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT" || exit 1

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

CANARY_TEST="cli/tests/unit/test_ambient_canary.py"
PY="$REPO_ROOT/cli/.venv/bin/python"
[ -x "$PY" ] || PY="python3"

# Run the canary through fno's own child-env builder, in one lane, and report
# the pytest exit code. Going through _child_env is the point: that is the
# boundary the real smoke run uses, so this exercises the shipped path rather
# than a re-implementation of it.
run_lane() {
  local mode="$1"
  PYTHONPATH="$REPO_ROOT/cli/src" "$PY" - "$mode" "$CANARY_TEST" <<'PYEOF' >/dev/null 2>&1
import os, subprocess, sys
from pathlib import Path
import fno.test_cmd as tc
from fno.hermetic import AMBIENT_LEAK_CANARY

mode, test_path = sys.argv[1], sys.argv[2]
root = Path.cwd()

# The clean lane has to be clean BY CONSTRUCTION, not by assuming this
# process's own parent was. When the whole smoke runs under `--ambient dirty`,
# THIS script is itself a poisoned child: the canary is in its environment by
# design (it is the one thing neutralise deliberately lets through), so a
# clean sub-lane built from os.environ inherits it and the clean half fails.
#
# That is the same defect the suite is being checked for, in the checker. Drop
# it explicitly; the dirty lane gets it back from poison() a line later.
os.environ.pop(AMBIENT_LEAK_CANARY, None)

tc._AMBIENT_MODE = mode
env = tc._child_env(root)
sys.exit(subprocess.run(
    [sys.executable, "-m", "pytest", "-q", "-p", "no:cacheprovider", test_path],
    env=env, cwd=root,
).returncode)
PYEOF
  return $?
}

echo "hermetic lanes: the dirty lane must be able to go red"

run_lane clean
clean_rc=$?
if [ "$clean_rc" -eq 0 ]; then
  pass "clean lane: canary test passes (no ambient state reaches a test)"
else
  fail "clean lane: canary test failed (rc=$clean_rc); something poisons the env outside --ambient dirty"
fi

run_lane dirty
dirty_rc=$?
if [ "$dirty_rc" -ne 0 ]; then
  pass "dirty lane: canary test fails (rc=$dirty_rc) - the lane can detect a leak"
else
  fail "dirty lane: canary test PASSED, so the positive control is disarmed.
       The dirty lane can no longer prove it detects anything, and a green run
       from it is not evidence. Check whether a scrub rule in fno/hermetic.py
       widened far enough to swallow AMBIENT_LEAK_CANARY."
fi

echo
echo "  passed: $PASS  failed: $FAIL"
[ "$FAIL" -eq 0 ] || exit 1
exit 0
