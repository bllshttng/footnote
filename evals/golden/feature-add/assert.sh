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
check slugify_hello_world "${PYTHON:-python3}" -c 'from textkit import slugify; assert slugify("Hello World") == "hello-world"'
check slugify_punctuation "${PYTHON:-python3}" -c 'from textkit import slugify; assert slugify("Python 3.11!") == "python-3-11"'
check slugify_trim "${PYTHON:-python3}" -c 'from textkit import slugify; assert slugify("  --trim me--  ") == "trim-me"'
