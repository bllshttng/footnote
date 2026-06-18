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
check parse_rows_importable "${PYTHON:-python3}" -c 'from report import parse_rows'
check format_row_importable "${PYTHON:-python3}" -c 'from report import format_row'
check summarize_importable "${PYTHON:-python3}" -c 'from report import summarize'
