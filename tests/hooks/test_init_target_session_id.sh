#!/usr/bin/env bash
# test_init_target_session_id.sh -- verify TARGET_SESSION_ID handling in
# init-target-state.sh (ab-7303e5d7, GAP-3).
#
# Covers:
#   (a) TARGET_SESSION_ID=preset-key-123 wins over CODEX_THREAD_ID and the
#       Codex thread is recorded additively in the manifest
#   (b) No TARGET_SESSION_ID or CODEX_THREAD_ID => generated session_id matches
#       the pattern
#       [0-9]{8}T[0-9]{6}Z-[0-9]+-...
#   (d) No TARGET_SESSION_ID + CODEX_THREAD_ID => per-target session id is
#       unique while the thread id remains owner metadata
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
  printf '# isolated\n' > "${_dir}/.fno/config.toml"
  mkdir -p "${_dir}/home/.fno"
  printf '# isolated global\n' > "${_dir}/home/.fno/config.toml"
}

# ── Helper: register a live row owning a session id (collision seed) ──
# Pins state_dir at <home>/.fno via FNO_CONFIG + a migration sentinel so the
# seed and the verb's collider resolve the SAME registry regardless of any
# vault-aware path migration the CLI startup would otherwise perform.
plant_owner() {
  local _home="$1" _id="$2" _canon
  _canon="$(dirname "$(cd "$REPO_ROOT" && git rev-parse --git-common-dir)")"
  mkdir -p "$_home/.fno"
  printf 'schema_version: 1\nconfig:\n  state_dir: %s/.fno/\n' "$_home" > "$_home/.fno/settings.yaml"
  touch "$_home/.fno/.path-migration-done"
  HOME="$_home" FNO_CONFIG="$_home/.fno/settings.yaml" \
    PYTHONPATH="$REPO_ROOT/cli/src" "$_canon/cli/.venv/bin/python" - "$_id" <<'PYEOF'
import sys

sid = sys.argv[1]
from fno.agents.registry import register_existing_session

register_existing_session(provider="codex", session_id=sid, cwd="/x")
PYEOF
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
  CODEX_THREAD_ID="codex-thread-loses-to-explicit" \
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

CODEX_THREAD_ID_A=$(grep '^codex_thread_id:' "$STATE_A" | sed 's/^codex_thread_id:[[:space:]]*//' | tr -d '\r')
[[ "$CODEX_THREAD_ID_A" == "codex-thread-loses-to-explicit" ]] \
  || fail "(a): expected codex_thread_id to be recorded, got '${CODEX_THREAD_ID_A}'"
pass "(a): TARGET_SESSION_ID wins while codex_thread_id is still recorded"

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
  CODEX_THREAD_ID= \
  TARGET_SESSION_ID= \
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

# ── (c) manifest heredoc must not run command substitution ───────────
# Regression: the manifest heredoc is unquoted (`<< EOF`) so it can expand
# $vars, which means an unescaped backtick in a comment line executes as a
# command. The dispatch-pins comment mentions `fno target start`/`init`; if
# those backticks aren't escaped, init spews "No such command 'start'" +
# "init: command not found" to stderr and writes the collapsed literal
# "chosen at /," into every manifest. Prior scenarios ran init with
# `2>&1 >/dev/null`, hiding it - so capture stderr here and assert the
# comment survives verbatim.
log "(c): manifest heredoc keeps backtick comment literal, no command substitution"

make_repo TMP_C
_ALL_TMPS+=("$TMP_C")

STDERR_C="${TMP_C}/init-stderr.txt"
(cd "$TMP_C" && \
  HOME="${TMP_C}/home" \
  TARGET_START=1 \
  TARGET_INPUT="test-heredoc-no-subst" \
  CODEX_THREAD_ID= \
  TARGET_SESSION_ID= \
  TARGET_LOCATION_OK="main-acknowledged" \
  bash "$INIT" >/dev/null 2>"$STDERR_C") \
  || fail "(c): init exited non-zero"

STATE_C="${TMP_C}/.fno/target-state.md"
[[ -f "$STATE_C" ]] || fail "(c): target-state.md was not created"

# The literal comment (backticks intact) must be present verbatim.
grep -qF 'chosen at `fno target start`/`init`, carried' "$STATE_C" \
  || fail "(c): dispatch-pins comment was mangled (backticks ran as command substitution)"
pass "(c): dispatch-pins comment kept its backticks literal"

# stderr must be free of the substitution symptoms.
if grep -qE "No such command 'start'|init: command not found" "$STDERR_C"; then
  fail "(c): init stderr shows command-substitution errors:
$(cat "$STDERR_C")"
fi
pass "(c): init stderr free of command-substitution errors"

# ── (d) CODEX_THREAD_ID owns claims; target session ids stay unique ───
log "(d): no TARGET_SESSION_ID + CODEX_THREAD_ID => unique target session id"

make_repo TMP_D
_ALL_TMPS+=("$TMP_D")

(cd "$TMP_D" && \
  HOME="${TMP_D}/home" \
  TARGET_START=1 \
  TARGET_INPUT="test-codex-thread-id" \
  CODEX_THREAD_ID="019f48e4-codex-thread" \
  TARGET_SESSION_ID= \
  TARGET_LOCATION_OK="main-acknowledged" \
  bash "$INIT" >/dev/null 2>&1) \
  || fail "(d): init exited non-zero"

STATE_D="${TMP_D}/.fno/target-state.md"
[[ -f "$STATE_D" ]] || fail "(d): target-state.md was not created"

SESSION_ID_D=$(grep '^session_id:' "$STATE_D" | sed 's/^session_id:[[:space:]]*//' | tr -d '\r')
CODEX_THREAD_ID_D=$(grep '^codex_thread_id:' "$STATE_D" | sed 's/^codex_thread_id:[[:space:]]*//' | tr -d '\r')
echo "$SESSION_ID_D" | grep -qE '^[0-9]{8}T[0-9]{6}Z-cx[0-9]+-[0-9a-f]{6}$' \
  || fail "(d): expected unique cx-tagged target session_id, got '${SESSION_ID_D}'"
[[ "$SESSION_ID_D" != "019f48e4-codex-thread" ]] \
  || fail "(d): stable Codex thread was reused as the target session_id"
[[ "$CODEX_THREAD_ID_D" == "019f48e4-codex-thread" ]] \
  || fail "(d): expected codex_thread_id in manifest, got '${CODEX_THREAD_ID_D}'"
pass "(d): Codex thread remains owner metadata while target session id is unique"

# A successful finalize event is the explicit run boundary for claimless
# free-text targets. Re-enter the SAME worktree/thread to prove the prior
# manifest rotates before the next run id is minted.
mkdir -p "${TMP_D}/.fno"
printf '%s\n' \
  "{\"type\":\"session_finalized\",\"data\":{\"session_id\":\"${SESSION_ID_D}\",\"termination_reason\":\"NoWork\",\"ship\":false}}" \
  > "${TMP_D}/.fno/events.jsonl"
(cd "$TMP_D" && \
  HOME="${TMP_D}/home" \
  TARGET_START=1 \
  TARGET_INPUT="test-codex-thread-id-second-run" \
  CODEX_THREAD_ID="019f48e4-codex-thread" \
  TARGET_SESSION_ID= \
  TARGET_LOCATION_OK="main-acknowledged" \
  bash "$INIT" >/dev/null 2>&1) \
  || fail "(d): second target init exited non-zero"
SESSION_ID_E=$(grep '^session_id:' "${TMP_D}/.fno/target-state.md" | sed 's/^session_id:[[:space:]]*//' | tr -d '\r')
[[ "$SESSION_ID_E" != "$SESSION_ID_D" ]] \
  || fail "(d): two completed targets in one worktree/thread reused session_id '${SESSION_ID_D}'"
compgen -G "${TMP_D}/.fno/target-state.terminal.*.md" >/dev/null \
  || fail "(d): completed claimless target manifest was not archived"
pass "(d): completed targets in one worktree/thread receive distinct session ids"

# A shipped terminal is also a run boundary (the normal delivery path).
printf '%s\n' \
  "{\"type\":\"session_finalized\",\"data\":{\"session_id\":\"${SESSION_ID_E}\",\"termination_reason\":\"DonePRGreen\",\"ship\":true}}" \
  >> "${TMP_D}/.fno/events.jsonl"
(cd "$TMP_D" && \
  HOME="${TMP_D}/home" \
  TARGET_START=1 \
  TARGET_INPUT="test-codex-thread-id-third-run" \
  CODEX_THREAD_ID="019f48e4-codex-thread" \
  TARGET_SESSION_ID= \
  TARGET_LOCATION_OK="main-acknowledged" \
  bash "$INIT" >/dev/null 2>&1) \
  || fail "(d): third target init exited non-zero"
SESSION_ID_F=$(grep '^session_id:' "${TMP_D}/.fno/target-state.md" | sed 's/^session_id:[[:space:]]*//' | tr -d '\r')
[[ "$SESSION_ID_F" != "$SESSION_ID_E" ]] \
  || fail "(d): shipped target reused session_id '${SESSION_ID_E}'"
pass "(d): NoWork and shipped terminal boundaries both rotate claimless runs"

# ── (e) AC1-HP + AC3-ERR: foreign codex id (owned by a live row) rejected ─
# A claude session that also sees a CODEX_THREAD_ID owned by another live worker
# must record CLAUDE as its identity, never codex / the foreign id. Seeded with
# a live registry row owning the foreign id, so the collision-elimination path
# resolves claude regardless of which harness runs THIS test (CI-robust).
log "(e): foreign CODEX_THREAD_ID owned by a live row => claude identity, id refused"

make_repo TMP_E
_ALL_TMPS+=("$TMP_E")
FOREIGN_E="019fc87d-ddff-7c90-926a-6bdd7ebb186c"
CLAUDE_SID_E="aaaa1111-mine-mine-mine-aaaaaaaaaaaa"
plant_owner "${TMP_E}/home" "$FOREIGN_E" || fail "(e): could not seed registry owner"

STDERR_E="${TMP_E}/init-stderr.txt"
# Source fno via PYTHONPATH: the inherited `fno` launcher execs fno-py, which
# imports THIS checkout's source, so the verb runs without a deployed/stale fno
# or a venv-path shim (the smoke runner already exports this PYTHONPATH).
(cd "$TMP_E" && \
  HOME="${TMP_E}/home" FNO_CONFIG="${TMP_E}/home/.fno/settings.yaml" \
  PYTHONPATH="$REPO_ROOT/cli/src${PYTHONPATH:+:$PYTHONPATH}" \
  TARGET_START=1 TARGET_INPUT="test-ac1-hp-collision" \
  CODEX_THREAD_ID="$FOREIGN_E" \
  CLAUDE_CODE_SESSION_ID="$CLAUDE_SID_E" \
  TARGET_LOCATION_OK="main-acknowledged" \
  bash "$INIT" >/dev/null 2>"$STDERR_E") \
  || fail "(e): init exited non-zero"

STATE_E="${TMP_E}/.fno/target-state.md"
HARNESS_E=$(grep '^harness:' "$STATE_E" | sed 's/^harness:[[:space:]]*//' | tr -d '\r')
HSID_E=$(grep '^harness_session_id:' "$STATE_E" | sed 's/^harness_session_id:[[:space:]]*//' | tr -d '\r')
SID_E=$(grep '^session_id:' "$STATE_E" | sed 's/^session_id:[[:space:]]*//' | tr -d '\r')
CTID_E=$(grep '^codex_thread_id:' "$STATE_E" | sed 's/^codex_thread_id:[[:space:]]*//' | tr -d '\r')
[[ "$HARNESS_E" == "claude" ]] || fail "(e): expected harness claude, got '${HARNESS_E}'"
[[ "$HSID_E" == "$CLAUDE_SID_E" ]] \
  || fail "(e): harness_session_id must be the claude id '${CLAUDE_SID_E}', got '${HSID_E}'"
[[ "$SID_E" != *"$FOREIGN_E"* ]] || fail "(e): foreign codex id leaked into session_id"
echo "$SID_E" | grep -qE '^[0-9]{8}T[0-9]{6}Z-cl[0-9]+-' \
  || fail "(e): session_id '${SID_E}' must carry the cl infix, not cx"
# The foreign id is still recorded as additive codex metadata (diagnosis), just
# not as the identity.
[[ "$CTID_E" == "$FOREIGN_E" ]] \
  || fail "(e): codex_thread_id metadata should still be recorded, got '${CTID_E}'"
# The collision refusal left a durable stderr trace.
grep -q "refused harness_session_id owned by live row" "$STDERR_E" \
  || fail "(e): expected collision-refusal trace on stderr; got:
$(cat "$STDERR_E")"
pass "(e): foreign codex id refused; claude identity recorded instead"

# ── (f) real codex session keeps codex identity (no regression) ───────
log "(f): CODEX_THREAD_ID only => harness codex, cx infix (source fno, no regression)"

make_repo TMP_F
_ALL_TMPS+=("$TMP_F")
(cd "$TMP_F" && \
  HOME="${TMP_F}/home" \
  PYTHONPATH="$REPO_ROOT/cli/src${PYTHONPATH:+:$PYTHONPATH}" \
  TARGET_START=1 TARGET_INPUT="test-codex-only-sourcefno" \
  CODEX_THREAD_ID="019f48e4-codex-thread" \
  CLAUDE_CODE_SESSION_ID= CLAUDECODE_SESSION_ID= CODEX_SESSION_ID= \
  GEMINI_SESSION_ID= OPENCODE_SESSION_ID= \
  TARGET_LOCATION_OK="main-acknowledged" \
  bash "$INIT" >/dev/null 2>&1) \
  || fail "(f): init exited non-zero"

STATE_F="${TMP_F}/.fno/target-state.md"
HARNESS_F=$(grep '^harness:' "$STATE_F" | sed 's/^harness:[[:space:]]*//' | tr -d '\r')
HSID_F=$(grep '^harness_session_id:' "$STATE_F" | sed 's/^harness_session_id:[[:space:]]*//' | tr -d '\r')
SID_F=$(grep '^session_id:' "$STATE_F" | sed 's/^session_id:[[:space:]]*//' | tr -d '\r')
[[ "$HARNESS_F" == "codex" ]] || fail "(f): expected harness codex, got '${HARNESS_F}'"
[[ "$HSID_F" == "019f48e4-codex-thread" ]] \
  || fail "(f): harness_session_id should be the codex thread, got '${HSID_F}'"
echo "$SID_F" | grep -qE '^[0-9]{8}T[0-9]{6}Z-cx[0-9]+-' \
  || fail "(f): codex session_id '${SID_F}' must carry the cx infix"
pass "(f): real codex session keeps codex identity and cx infix"

log "All session_id scenarios passed"
