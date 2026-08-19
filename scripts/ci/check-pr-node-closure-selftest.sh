#!/usr/bin/env bash
# check-pr-node-closure-selftest.sh - self-test for check-pr-node-closure.sh.
#
# Scenarios: target (branch id claimed) passes, contained (a second claimed id
# also in the trailer) passes, missing (branch id absent from the trailer)
# fails, malformed (trailer present but never names the branch id) fails,
# non-node branch skips, and a prose-only mention (never the exact trailer
# line) fails.
# Exit: 0 pass, 1 fail.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="${SCRIPT_DIR}/check-pr-node-closure.sh"

log()  { printf '[pr-node-closure] %s\n' "$*"; }
fail() { printf '[pr-node-closure] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[pr-node-closure] PASS: %s\n' "$*"; }

[[ -f "$GATE" ]] || fail "gate not found at ${GATE}"
bash -n "$GATE" || fail "gate failed bash -n"

# run <body> <head_ref>; echoes the gate's exit code via $?.
run() {
  local body="$1"; local ref="$2"
  PR_BODY="$body" PR_HEAD_REF="$ref" bash "$GATE" >/dev/null 2>&1
}

# target: the branch's own node id is exactly claimed.
run "Fixes the thing.

Backlog-Closure: x-59a6" "feature/x-59a6" \
  && pass "target: claimed id passes" || fail "target should pass"

# contained: the trailer also names a second (contained) id; the branch's own
# id is still present, so it still passes.
run "Backlog-Closure: x-59a6 x-1111" "feature/x-59a6" \
  && pass "contained: extra claimed id still passes" || fail "contained should pass"

# missing: the branch names an id the trailer never claims.
if run "Backlog-Closure: x-0000" "feature/x-59a6"; then
  fail "missing claim should fail"
else
  pass "missing claim fails"
fi

# malformed: a trailer line exists but never names the branch's id (a typo'd
# token is silently dropped by the parser, so it reads the same as absent).
if run "Backlog-Closure: x-59a7" "feature/x-59a6"; then
  fail "malformed claim should fail"
else
  pass "malformed claim fails"
fi

# non-node-branch: no id-shaped segment in the ref at all.
run "no trailer here" "main" \
  && pass "non-node branch skips" || fail "non-node branch should skip"

# prose-only: the id is mentioned in prose, never on the exact trailer line.
if run "This PR also touches x-59a6 in passing." "feature/x-59a6"; then
  fail "prose-only mention should fail"
else
  pass "prose-only mention fails"
fi

# all-hex suffix: a real id's suffix ("cdef") is itself a valid node-id
# PREFIX shape, so a following segment must never re-glue with it into a
# second, bogus candidate (review fix: reproduced live pre-fix).
run "Backlog-Closure: x-cdef" "feature/x-cdef-1234" \
  && pass "all-hex suffix never invents a second candidate" \
  || fail "all-hex suffix should not invent a bogus second candidate"

log "all scenarios passed"
