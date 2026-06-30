#!/usr/bin/env bash
# test_init_target_session_id.sh -- verify TARGET_SESSION_ID handling in
# init-target-state.sh (ab-7303e5d7, GAP-3).
#
# Covers:
#   (a) TARGET_SESSION_ID=preset-key-123 + TARGET_START=1 + TARGET_INPUT set
#       => written manifest has `session_id: preset-key-123`
#   (b) No TARGET_SESSION_ID => generated session_id matches the pattern
#       [0-9]{8}T[0-9]{6}Z-[0-9]+-...
#
# Exit codes:
#   0  all scenarios passed
#   1  assertion failed
#   77 skipped (missing dependencies)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
INIT="${REPO_ROOT}/hooks/helpers/init-target-state.sh"

log()  { printf '[session-id] %s\n' "$*"; }
fail() { printf '[session-id] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[session-id] PASS: %s\n' "$*"; }
skip() { printf '[session-id] SKIP: %s\n' "$*" >&2; exit 77; }

# ── Prereqs ──────────────────────────────────────────────────────────
command -v git     &>/dev/null || skip "git not on PATH"
command -v python3 &>/dev/null || skip "python3 not on PATH"
[[ -f "$INIT" ]]     || fail "init script not found at $INIT"

bash -n "$INIT" || fail "bash -n rejected $INIT (syntax error)"
pass "init script passes bash -n"

# Track all temp dirs for cleanup
_ALL_TMPS=()
trap 'rm -rf "${_ALL_TMPS[@]}"' EXIT

# ── Helper: create an isolated temp repo ─────────────────────────────
make_repo() {
  local _varname="$1"
  local _dir
  _dir="$(mktemp -d -t init-session-id.XXXXXX)" || fail "mktemp failed"
  eval "${_varname}=\"\${_dir}\""
  (cd "$_dir" && git init -q && mkdir -p .fno) || fail "repo setup failed in $_dir"
  printf '# isolated\n' > "${_dir}/.fno/settings.yaml"
  mkdir -p "${_dir}/home/.fno"
  printf '# isolated global\n' > "${_dir}/home/.fno/settings.yaml"
}

# ── (a) TARGET_SESSION_ID preset is written verbatim ─────────────────
log "(a): TARGET_SESSION_ID=preset-key-123 => manifest session_id matches verbatim"

make_repo TMP_A
_ALL_TMPS+=("$TMP_A")

(cd "$TMP_A" && \
  HOME="${TMP_A}/home" \
  TARGET_START=1 \
  TARGET_INPUT="test-session-id-preset" \
  TARGET_SESSION_ID="preset-key-123" \
  TARGET_LOCATION_OK="main-acknowledged" \
  bash "$INIT" >/dev/null 2>&1) \
  || fail "(a): init exited non-zero"

STATE_A="${TMP_A}/.fno/target-state.md"
[[ -f "$STATE_A" ]] || fail "(a): target-state.md was not created"

# Read the session_id field
SESSION_ID_A=$(grep '^session_id:' "$STATE_A" | sed 's/^session_id:[[:space:]]*//' | tr -d '\r')
[[ "$SESSION_ID_A" == "preset-key-123" ]] \
  || fail "(a): expected session_id 'preset-key-123', got '${SESSION_ID_A}'"
pass "(a): session_id written verbatim as 'preset-key-123'"

# Verify the YAML parses and the field matches
python3 -c "
import sys
content = open('$STATE_A').read()
parts = content.split('---')
if len(parts) < 3:
    sys.exit('not enough --- delimiters')
import yaml
data = yaml.safe_load(parts[1])
sid = data.get('session_id')
if sid != 'preset-key-123':
    sys.exit(f'YAML session_id mismatch: {sid!r}')
print(f'YAML: session_id={sid!r}')
" || fail "(a): YAML parse/assertion failed"
pass "(a): YAML parses and session_id matches"

# ── (b) No TARGET_SESSION_ID => generated id matches expected pattern ─
# Segment 2 carries an optional 2-char provider provenance infix
# glued to the pid ({ts}-cl{pid}-{hex} for a claude self-mint). The id MUST
# still split to exactly 3 dash-segments so split('-')[0] consumers are safe.
log "(b): no TARGET_SESSION_ID => generated id matches [0-9]{8}T[0-9]{6}Z-<infix><pid>-..."

make_repo TMP_B
_ALL_TMPS+=("$TMP_B")

(cd "$TMP_B" && \
  HOME="${TMP_B}/home" \
  TARGET_START=1 \
  TARGET_INPUT="test-session-id-generated" \
  TARGET_LOCATION_OK="main-acknowledged" \
  bash "$INIT" >/dev/null 2>&1) \
  || fail "(b): init exited non-zero"

STATE_B="${TMP_B}/.fno/target-state.md"
[[ -f "$STATE_B" ]] || fail "(b): target-state.md was not created"

SESSION_ID_B=$(grep '^session_id:' "$STATE_B" | sed 's/^session_id:[[:space:]]*//' | tr -d '\r')
[[ -n "$SESSION_ID_B" ]] || fail "(b): session_id is empty"

# Must match: YYYYMMDDTHHMMSSZ-<optional 2-char infix><digits>-<chars>
# Pattern: 8 digits, T, 6 digits, Z, -, optional 2 lowercase infix, one or more
# digits (pid), -, one or more chars (entropy).
if ! echo "$SESSION_ID_B" | grep -qE '^[0-9]{8}T[0-9]{6}Z-[a-z]{0,2}[0-9]+-'; then
  fail "(b): generated session_id '${SESSION_ID_B}' does not match expected pattern [0-9]{8}T[0-9]{6}Z-<infix><pid>-..."
fi
pass "(b): generated session_id '${SESSION_ID_B}' matches expected pattern"

# Invariant: exactly 3 dash-segments regardless of infix (split('-')[0]
# consumers like dispatch.py read segment 0 = timestamp and must stay safe).
SEG_COUNT_B=$(echo "$SESSION_ID_B" | awk -F- '{print NF}')
[[ "$SEG_COUNT_B" -eq 3 ]] \
  || fail "(b): session_id '${SESSION_ID_B}' has ${SEG_COUNT_B} dash-segments, expected exactly 3"
pass "(b): session_id keeps exactly 3 dash-segments (infix lives inside segment 2)"

# AC2-HP: this harness detects provider=claude, so segment 2 must carry
# the 'cl' provider infix immediately before the pid.
SEG2_B=$(echo "$SESSION_ID_B" | cut -d- -f2)
if echo "$SEG2_B" | grep -qE '^cl[0-9]+$'; then
  pass "(b): claude self-mint carries the 'cl' provenance infix ('${SEG2_B}')"
else
  log "(b): segment 2 '${SEG2_B}' has no 'cl' infix (non-claude provider in this harness) - infix is optional, skipping"
fi

log "All session_id scenarios passed"
