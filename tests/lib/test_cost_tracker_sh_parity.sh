#!/usr/bin/env bash
# Parity test: cost-tracker.sh estimate_cost must agree with the Python
# pricing source of truth (the in-package fno.cost.cost_tracker module) by
# construction, because the shell delegates to `python3 -m fno.cost.cost_tracker
# estimate`. Also covers the python3-missing degrade path (warn, echo 0,
# return 1).
#
# Run: bash tests/lib/test_cost_tracker_sh_parity.sh
set -u

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
COST_TRACKER_SH="$REPO_ROOT/scripts/metrics/cost-tracker.sh"
# Point PYTHONPATH at the package source so `-m fno.cost.cost_tracker` resolves
# pre-install (mirrors what the sourced cost-tracker.sh does for estimate_cost).
export PYTHONPATH="$REPO_ROOT/cli/src${PYTHONPATH:+:${PYTHONPATH}}"

FAILURES=0

pass() { echo "  PASS $1"; }
fail() { echo "  FAIL $1: $2"; FAILURES=$((FAILURES + 1)); }

# shellcheck disable=SC1090
source "$COST_TRACKER_SH"

# --- parity: shell output == python output for a matrix of models/tokens ---

check_parity() {
    local model="$1" in_t="$2" out_t="$3" cr="${4:-0}" cc="${5:-0}"
    local shell_out python_out runner
    shell_out=$(estimate_cost "$model" "$in_t" "$out_t" "$cr" "$cc")
    # Compare against the SAME runner estimate_cost resolves (fno needs >=3.11
    # plus its deps; bare python3 may be an older system build without them).
    runner=$(_cost_runner)
    # shellcheck disable=SC2086  # $runner is a deliberate multi-word prefix
    python_out=$($runner -m fno.cost.cost_tracker estimate "$model" "$in_t" "$out_t" "$cr" "$cc")
    if [[ "$shell_out" == "$python_out" ]]; then
        pass "parity $model $in_t/$out_t/$cr/$cc -> $shell_out"
    else
        fail "parity $model" "shell=$shell_out python=$python_out"
    fi
}

check_parity claude-opus-4-8 1000000 1000000
check_parity claude-opus-4-8 50000 10000 2000000 300000
check_parity claude-opus-4-1 1000000 1000000
check_parity sonnet 1000000 1000000
check_parity haiku 1000000 0
check_parity claude-haiku-4-5 123456 7890

# --- exact expected values (pins the pricing, not just parity) ---

expect_value() {
    local label="$1" expected="$2" actual="$3"
    if [[ "$actual" == "$expected" ]]; then
        pass "$label -> $actual"
    else
        fail "$label" "expected $expected, got $actual"
    fi
}

expect_value "opus-4.8 1M/1M" "30.0000" "$(estimate_cost claude-opus-4-8 1000000 1000000)"
expect_value "opus-4.8 cache 1M/1M" "6.7500" "$(estimate_cost claude-opus-4-8 0 0 1000000 1000000)"
expect_value "legacy opus-4.1 1M/1M" "90.0000" "$(estimate_cost claude-opus-4-1 1000000 1000000)"
expect_value "sonnet 1M/1M" "18.0000" "$(estimate_cost sonnet 1000000 1000000)"

# --- three-arg legacy call shape still works (skill docs use it) ---

three_arg=$(estimate_cost opus 50000 10000)
if [[ -n "$three_arg" && "$three_arg" != "0" ]]; then
    pass "three-arg call shape -> $three_arg"
else
    fail "three-arg call shape" "got '$three_arg'"
fi

# --- non-numeric tokens coerce to 0 (existing contract) ---

expect_value "non-numeric tokens coerce to 0" "0.0000" "$(estimate_cost sonnet abc xyz)"

# --- python3 missing: warn to stderr, echo 0, return 1 ---

# estimate_cost sets PYTHONPATH at source time, so only the python3 lookup
# depends on PATH; an empty PATH simulates a python3-less host.
missing_out=$(PATH="" estimate_cost sonnet 1000 1000 2>/dev/null)
missing_rc=$?
missing_err=$(PATH="" estimate_cost sonnet 1000 1000 2>&1 >/dev/null)
if [[ "$missing_out" == "0" && "$missing_rc" -ne 0 && -n "$missing_err" ]]; then
    pass "python3 missing degrades (echo 0, rc=$missing_rc, warns)"
else
    fail "python3 missing degrade" "out=$missing_out rc=$missing_rc err='$missing_err'"
fi

# --- the duplicate shell pricing table is gone ---

if grep -q "input_rate=" "$COST_TRACKER_SH"; then
    fail "duplicate pricing table" "inline rate table still present in cost-tracker.sh"
else
    pass "duplicate shell pricing table deleted"
fi

# --- format_cost untouched ---

expect_value "format_cost" "\$2.35" "$(format_cost 2.3500)"

echo ""
if [[ "$FAILURES" -eq 0 ]]; then
    echo "OK (0 failures)"
    exit 0
else
    echo "FAILED ($FAILURES failures)"
    exit 1
fi
