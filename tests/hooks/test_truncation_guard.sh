#!/usr/bin/env bash
# Test suite for hooks/truncation-guard.py.
#
# The guard refuses a count-or-existence read piped into head/tail, because a
# truncated listing answers a different question than the one asked and the
# answer looks identical (specimen: `pgrep -fl fno-agents | head -4` reported
# "no daemon running" while the daemon was live).
#
# Tests:
#   T1  pgrep piped into head        -> deny, reason names `wc -l`
#   T2  tail -f on a log             -> allow, no envelope
#   T3  ls piped into head -c        -> allow (byte bound, drops no row)
#   T4  gh run list piped into head  -> deny
#   T5  a producer in a later segment of a compound -> deny
#   T6  cat piped into head          -> allow (not a count-or-existence read)
#   T7  a non-Bash tool              -> allow
#   T9  pgrep piped into tail -F     -> allow (GNU follow-with-retry, not a byte/row count)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOK="${REPO_ROOT}/hooks/truncation-guard.py"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); printf '[truncation] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[truncation] FAIL: %s\n' "$*" >&2; }

[[ -f "$HOOK" ]] || { fail "hook not found at $HOOK"; exit 1; }

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT
# Pin the guard's liveness journal into the tmp dir so a test run never
# appends to the real project events log.
export FNO_EVENTS_PATH="${TMP_DIR}/events.jsonl"

# run_hook <tool_name> <command> -> prints the hook's stdout
run_hook() {
  python3 -c '
import json, sys
print(json.dumps({"tool_name": sys.argv[1], "tool_input": {"command": sys.argv[2]}}))
' "$1" "$2" | python3 "$HOOK"
}

expect_deny() {
  local label="$1" cmd="$2" want="$3" out
  out="$(run_hook Bash "$cmd")"
  if [[ "$out" != *'"permissionDecision": "deny"'* ]]; then
    fail "$label: expected deny, got: ${out:-<empty>}"
    return
  fi
  if [[ -n "$want" && "$out" != *"$want"* ]]; then
    fail "$label: reason missing '$want'"
    return
  fi
  pass "$label"
}

expect_allow() {
  local label="$1" cmd="$2" tool="${3:-Bash}" out
  out="$(run_hook "$tool" "$cmd")"
  if [[ -n "$out" ]]; then
    fail "$label: expected silence, got: $out"
    return
  fi
  pass "$label"
}

expect_deny "T1 pgrep | head -4 is refused with the wc -l remedy" \
  'pgrep -fl fno-agents | head -4' 'wc -l'
expect_allow "T2 tail -f on a log is allowed" \
  'tail -f .fno/last-ci.log'
expect_allow "T3 head -c is a byte bound, allowed" \
  'ls | head -c 200'
expect_deny "T4 gh run list | head is refused" \
  'gh run list --limit 50 | head -5' 'truncation guard'
expect_deny "T5 a producer in a later compound segment is refused" \
  'cd /tmp && rg -l fno_agents | head -3' 'wc -l'
expect_allow "T6 cat | head is not a count-or-existence read" \
  'cat README.md | head -20'
expect_allow "T7 a non-Bash tool is allowed" \
  'pgrep x | head -1' 'Edit'
expect_allow "T9 pgrep piped into tail -F is allowed (follow-with-retry, uppercase)" \
  'pgrep -fl fno-agents | tail -F'

# The positive marker: the guard wrote one liveness row per run, so a silent
# allow is distinguishable from a guard that never launched.
if [[ -s "$FNO_EVENTS_PATH" ]] && grep -q '"guard":"truncation-guard"' "$FNO_EVENTS_PATH"; then
  pass "T8 the guard leaves a guard_decision liveness row"
else
  fail "T8 no guard_decision row at $FNO_EVENTS_PATH"
fi

printf '[truncation] %d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
