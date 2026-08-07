#!/usr/bin/env bash
# tests/ci/test_pr_body_length.sh - self-test for check-pr-body-length.sh.
#
# Scenarios: under cap passes, over cap fails, exception hatch passes,
# fenced code excluded, empty body fails open.
# Exit: 0 pass, 1 fail.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
GATE="${REPO_ROOT}/scripts/ci/check-pr-body-length.sh"

log()  { printf '[pr-body-length] %s\n' "$*"; }
fail() { printf '[pr-body-length] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[pr-body-length] PASS: %s\n' "$*"; }

[[ -f "$GATE" ]] || fail "gate not found at ${GATE}"
bash -n "$GATE" || fail "gate failed bash -n"

# run <body> [max]; echoes the gate's exit code.
run() {
  local body="$1"; local max="${2:-15}"
  PR_BODY="$body" PR_BODY_MAX_LINES="$max" bash "$GATE" >/dev/null 2>&1
}

# Under cap passes.
run "$(printf 'line %d\n' $(seq 1 10))" && pass "under cap passes" || fail "under cap should pass"

# Exactly at cap passes.
run "$(printf 'line %d\n' $(seq 1 15))" && pass "at cap passes" || fail "at cap should pass"

# Over cap fails.
if run "$(printf 'line %d\n' $(seq 1 20))"; then fail "over cap should fail"; else pass "over cap fails"; fi

# Exception hatch passes even when over cap.
if run "$(printf 'brevity-exception: needs a table\n%s\n' "$(printf 'line %d\n' $(seq 1 30))")"; then
  pass "exception hatch passes"
else
  fail "exception hatch should pass"
fi

# Fenced code is excluded from the count.
fence_body="$(
  printf '%s\n' 'intro line' '```'
  printf 'code %d\n' $(seq 1 16)
  printf '%s\n' '```' 'outro line'
)"
run "$fence_body" && pass "fenced code excluded" || fail "fenced code should not count"

# Empty body fails open.
run "" && pass "empty body skips" || fail "empty body should skip"

log "all scenarios passed"
