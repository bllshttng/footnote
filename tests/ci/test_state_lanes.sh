#!/usr/bin/env bash
# The state lane's positive control must pass clean and fail populated.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT" || exit 1

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

CANARY_TEST="cli/tests/unit/test_state_canary.py"
PY="$REPO_ROOT/cli/.venv/bin/python"
[ -x "$PY" ] || PY="python3"

run_lane() {
  local mode="$1"
  PYTHONPATH="$REPO_ROOT/cli/src" "$PY" - "$mode" "$CANARY_TEST" <<'PYEOF' >/dev/null 2>&1
import os
import subprocess
import sys
from pathlib import Path

import fno.test_cmd as tc

mode, test_path = sys.argv[1], sys.argv[2]
tc._STATE_MODE = mode
env = tc._smoke_env(Path.cwd())
env["STATE_LANE"] = mode
sys.exit(subprocess.run(
    [sys.executable, "-m", "pytest", "-q", "-p", "no:cacheprovider", test_path],
    env=env,
    cwd=Path.cwd(),
).returncode)
PYEOF
  return $?
}

echo "state lanes: populated lane must be able to go red"

run_lane clean
clean_rc=$?
if [ "$clean_rc" -eq 0 ]; then
  pass "clean lane: state canary passes"
else
  fail "clean lane: state canary failed (rc=$clean_rc)"
fi

run_lane populated
populated_rc=$?
if [ "$populated_rc" -ne 0 ]; then
  pass "populated lane: state canary fails (rc=$populated_rc)"
else
  fail "populated lane: state canary passed, so the positive control is disarmed"
fi

echo
echo "  passed: $PASS  failed: $FAIL"
[ "$FAIL" -eq 0 ] || exit 1
exit 0
