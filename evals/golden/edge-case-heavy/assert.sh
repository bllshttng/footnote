#!/usr/bin/env bash
# Contract: runs with CWD = the post-run workdir (the completed task's repo/).
# Emit exactly one "ok <name>" or "not ok <name>" line per assertion.
set -u

check() {
    local name="$1"; shift
    if "$@" >/dev/null 2>&1; then
        echo "ok $name"
    else
        echo "not ok $name"
    fi
}

check tests_pass "${PYTHON:-python3}" -m pytest -q
check parse_90s "${PYTHON:-python3}" -c 'from duration import parse_duration; assert parse_duration("90s") == 90'
check parse_5m "${PYTHON:-python3}" -c 'from duration import parse_duration; assert parse_duration("5m") == 300'
check parse_1h30m "${PYTHON:-python3}" -c 'from duration import parse_duration; assert parse_duration("1h30m") == 5400'
check parse_0s "${PYTHON:-python3}" -c 'from duration import parse_duration; assert parse_duration("0s") == 0'
check empty_raises "${PYTHON:-python3}" -c '
from duration import parse_duration
try:
    parse_duration("")
    raise AssertionError("should raise ValueError")
except ValueError:
    pass
'
check negative_raises "${PYTHON:-python3}" -c '
from duration import parse_duration
try:
    parse_duration("-5s")
    raise AssertionError("should raise ValueError")
except ValueError:
    pass
'
check garbage_raises "${PYTHON:-python3}" -c '
from duration import parse_duration
try:
    parse_duration("abc")
    raise AssertionError("should raise ValueError")
except ValueError:
    pass
'
