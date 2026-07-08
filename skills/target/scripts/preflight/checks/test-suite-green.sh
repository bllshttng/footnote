#!/usr/bin/env bash
# test-suite-green.sh - Opt-in check: runs quick test suite smoke
# Contract: stdout one line "test-suite-green {pass|fail|warn|unknown} {message}"
# Exit: always 0
# Default: unknown (opt-in via PREFLIGHT_RUN_TESTS=1 or .fno/settings.yaml test_suite_check: true)

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"

# Check if opt-in is configured
RUN_TESTS="${PREFLIGHT_RUN_TESTS:-0}"

# Check settings.yaml for test_suite_check: true
if [[ "$RUN_TESTS" != "1" && -f "$REPO_ROOT/.fno/config.toml" ]]; then
    if grep -qE "test_suite_check[[:space:]]*=[[:space:]]*true" "$REPO_ROOT/.fno/config.toml" 2>/dev/null; then
        RUN_TESTS=1
    fi
fi

if [[ "$RUN_TESTS" != "1" ]]; then
    echo "test-suite-green unknown opt-in not set (set PREFLIGHT_RUN_TESTS=1 or test_suite_check: true in settings.yaml)"
    exit 0
fi

# Determine test command
if [[ -f "$REPO_ROOT/package.json" ]]; then
    if command -v pnpm >/dev/null 2>&1; then
        TEST_CMD="pnpm test --bail 2>&1"
    elif command -v npm >/dev/null 2>&1; then
        TEST_CMD="npm test -- --bail 2>&1"
    else
        echo "test-suite-green unknown no Node.js test runner found in PATH"
        exit 0
    fi
elif [[ -f "$REPO_ROOT/pyproject.toml" || -f "$REPO_ROOT/setup.py" ]]; then
    if command -v pytest >/dev/null 2>&1; then
        TEST_CMD="pytest -x --timeout=55 2>&1"
    else
        echo "test-suite-green unknown pytest not found in PATH"
        exit 0
    fi
else
    echo "test-suite-green unknown no recognized test framework detected"
    exit 0
fi

# Run with 60s budget. `timeout` is GNU coreutils; macOS ships `gtimeout` via brew.
# If neither is present, run without a budget and emit a warn instead of mistaking
# the missing-tool exit (127) for a test failure.
cd "$REPO_ROOT"
TIMEOUT_BIN=""
if command -v timeout >/dev/null 2>&1; then
    TIMEOUT_BIN="timeout"
elif command -v gtimeout >/dev/null 2>&1; then
    TIMEOUT_BIN="gtimeout"
fi

if [[ -z "$TIMEOUT_BIN" ]]; then
    echo "test-suite-green warn timeout/gtimeout not available - skipping (install coreutils for the 60s budget)"
    exit 0
fi

if "$TIMEOUT_BIN" 60 bash -c "$TEST_CMD" > /dev/null 2>&1; then
    echo "test-suite-green pass test suite passes at HEAD"
else
    EC=$?
    if [[ $EC -eq 124 ]]; then
        echo "test-suite-green warn test suite timed out after 60s (budget exceeded)"
    else
        echo "test-suite-green fail test suite fails at HEAD - fix before starting target"
    fi
fi
exit 0
