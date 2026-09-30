#!/usr/bin/env bash
# test_init_target_session_id.sh -- verify TARGET_SESSION_ID handling in
# init-target-state.sh (ab-7303e5d7, GAP-3).
#
# Covers:
#   (a) TARGET_SESSION_ID=preset-key-123 wins over CODEX_THREAD_ID and the
#       Codex thread is recorded additively in the manifest
#   (b) No TARGET_SESSION_ID or CODEX_THREAD_ID => generated session_id is a
#       random v4 UUID from the one mint
#   (d) No TARGET_SESSION_ID + CODEX_THREAD_ID => per-target session id is
#       unique while the thread id remains owner metadata
#   (e) The mint door answers nothing => init refuses naming fno doctor update
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

# Scrub inherited harness markers so each scenario's env is exactly what it
# sets: the invoking session's CLAUDE_CODE_SESSION_ID etc. would otherwise leak
# into every subshell and flip the disagreement detection. A no-op in CI (which
# sets none of these) and a scrub under an interactive harness.
unset CLAUDE_CODE_SESSION_ID CLAUDECODE_SESSION_ID CODEX_THREAD_ID \
      CODEX_SESSION_ID GEMINI_SESSION_ID OPENCODE_SESSION_ID TARGET_TRANSCRIPT_ID 2>/dev/null || true

# This harness exercises identity fields after wrapper policy checks have
# passed. Direct fallback gate behavior has dedicated review, containment, and
# dispatch-hold tests.
export FNO_TARGET_INIT_GATED=1

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
  # State-path stub: pins init's manifest location to the scenario space dir.
  mkdir -p "${_dir}/bin" "${_dir}/space"
  cp "${SCRIPT_DIR}/../../tests/helpers/fno-agents-state-path-stub.sh" "${_dir}/bin/fno-agents"
  chmod 755 "${_dir}/bin/fno-agents"
}

# ── (a) TARGET_SESSION_ID preset is written verbatim ─────────────────
log "(a): TARGET_SESSION_ID=preset-key-123 => manifest session_id matches verbatim"

make_repo TMP_A
_ALL_TMPS+=("$TMP_A")

(cd "$TMP_A" && \
  HOME="${TMP_A}/home" \
  PATH="${TMP_A}/bin:$PATH" \
  FNO_TEST_SPACE="${TMP_A}/space" \
  TARGET_START=1 \
  TARGET_INPUT="test-session-id-preset" \
  TARGET_SESSION_ID="preset-key-123" \
  CODEX_THREAD_ID="codex-thread-loses-to-explicit" \
  TARGET_LOCATION_OK="main-acknowledged" \
  bash "$INIT" >/dev/null 2>&1) \
  || fail "(a): init exited non-zero"

STATE_A="${TMP_A}/space/target-state.md"
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

# ── (b) No TARGET_SESSION_ID => the run id is a random UUID from the mint ─
log "(b): no TARGET_SESSION_ID => generated id is a v4 UUID from the one mint"

make_repo TMP_B
_ALL_TMPS+=("$TMP_B")

(cd "$TMP_B" && \
  HOME="${TMP_B}/home" \
  PATH="${TMP_B}/bin:$PATH" \
  FNO_TEST_SPACE="${TMP_B}/space" \
  TARGET_START=1 \
  TARGET_INPUT="test-session-id-generated" \
  CODEX_THREAD_ID= \
  TARGET_SESSION_ID= \
  TARGET_LOCATION_OK="main-acknowledged" \
  bash "$INIT" >/dev/null 2>&1) \
  || fail "(b): init exited non-zero"

STATE_B="${TMP_B}/space/target-state.md"
[[ -f "$STATE_B" ]] || fail "(b): target-state.md was not created"

SESSION_ID_B=$(grep '^session_id:' "$STATE_B" | sed 's/^session_id:[[:space:]]*//' | tr -d '\r')
[[ -n "$SESSION_ID_B" ]] || fail "(b): session_id is empty"

# The mint answers a random v4 UUID in 8-4-4-4-12 form.
if ! echo "$SESSION_ID_B" | grep -qE '^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$'; then
  fail "(b): generated session_id '${SESSION_ID_B}' is not a v4 UUID"
fi
pass "(b): generated session_id '${SESSION_ID_B}' is a v4 UUID"

# ── (c) manifest heredoc must not run command substitution ───────────
# Regression: the manifest heredoc is unquoted (`<< EOF`) so it can expand
# $vars, which means an unescaped backtick in a comment line executes as a
# command. The dispatch-pins comment mentions `fno do target start`/`init`; if
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
  PATH="${TMP_C}/bin:$PATH" \
  FNO_TEST_SPACE="${TMP_C}/space" \
  TARGET_START=1 \
  TARGET_INPUT="test-heredoc-no-subst" \
  CODEX_THREAD_ID= \
  TARGET_SESSION_ID= \
  TARGET_LOCATION_OK="main-acknowledged" \
  bash "$INIT" >/dev/null 2>"$STDERR_C") \
  || fail "(c): init exited non-zero"

STATE_C="${TMP_C}/space/target-state.md"
[[ -f "$STATE_C" ]] || fail "(c): target-state.md was not created"

# The literal comment (backticks intact) must be present verbatim.
grep -qF 'chosen at `fno do target start`/`init`, carried' "$STATE_C" \
  || fail "(c): dispatch-pins comment was mangled (backticks ran as command substitution)"
pass "(c): dispatch-pins comment kept its backticks literal"

# stderr must be free of the substitution symptoms.
if grep -qE "No such command 'start'|init: command not found" "$STDERR_C"; then
  fail "(c): init stderr shows command-substitution errors:
$(cat "$STDERR_C")"
fi
pass "(c): init stderr free of command-substitution errors"

# Orphan-run the init: reparent it to PID 1 so its owned-identity verb sees no
# harness ancestor. The verb proves the harness by walking the process tree, so
# under a live harness session (e.g. this test run from inside claude) it would
# prove THAT harness and override the CODEX_THREAD_ID marker - the documented
# design at init-target-state.sh:924-927 ("a claude session carrying a foreign
# CODEX_THREAD_ID resolves to claude as the owned provider, so its session_id reads cl,
# never cx"). Reparenting to init gives the verb no harness ancestor - the same
# condition a CI runner runs under - so detect_provider honors CODEX_THREAD_ID
# -> codex -> cx, deterministically, regardless of who launched the test. A test
# whose verdict depends on who ran it is the bug this pins.
_orphan_init() {  # $1 = failure label, $2 = TARGET_INPUT
  local _label="$1" _input="$2" _rc
  _rc="${TMP_D}/.init_rc.$(printf '%s' "$_input" | tr -c '[:alnum:]' '_')"
  (
    cd "$TMP_D" || exit 99
    {
      HOME="${TMP_D}/home" \
  PATH="${TMP_D}/bin:$PATH" \
  FNO_TEST_SPACE="${TMP_D}/space" \
        TARGET_START=1 \
        TARGET_INPUT="$_input" \
        CODEX_THREAD_ID="019f48e4-codex-thread" \
        TARGET_SESSION_ID= \
        TARGET_LOCATION_OK="main-acknowledged" \
        bash "$INIT" >/dev/null 2>&1
      echo "$?" > "$_rc"
    } &
  )
  local _i
  for _i in $(seq 1 200); do [[ -f "$_rc" ]] && break; sleep 0.1; done
  [[ -f "$_rc" ]] || fail "${_label}: orphaned init did not complete (20s timeout)"
  [[ "$(cat "$_rc" 2>/dev/null)" == "0" ]] || fail "${_label}: orphaned init exited non-zero"
}

# ── (d) CODEX_THREAD_ID owns claims; target session ids stay unique ───
log "(d): no TARGET_SESSION_ID + CODEX_THREAD_ID => unique target session id"

make_repo TMP_D
_ALL_TMPS+=("$TMP_D")

_orphan_init "(d)" "test-codex-thread-id"

STATE_D="${TMP_D}/space/target-state.md"
[[ -f "$STATE_D" ]] || fail "(d): target-state.md was not created"

SESSION_ID_D=$(grep '^session_id:' "$STATE_D" | sed 's/^session_id:[[:space:]]*//' | tr -d '\r')
# Precondition for the orphan-run fix: the init must have resolved with NO
# ambient harness ancestor visible, else the cx tag below fails for an
# environmental reason, not a regression. Reparenting to PID 1 is the
# macOS/CI-docker behaviour; on Linux a subreaper (systemd user session,
# some container inits) can adopt the orphan and the verb's walk may still
# find an ancestor. Asserting the resolved harness here turns a future ubuntu
# red into a diagnosis instead of a mystery.
HARNESS_D=$(grep '^harness:' "$STATE_D" | sed 's/^harness:[[:space:]]*//' | tr -d '\r')
[[ "$HARNESS_D" == "codex" ]] \
  || fail "(d): orphaned init resolved harness='${HARNESS_D}' (expected codex) - an ambient harness ancestor was visible despite reparenting to PID 1. On Linux a subreaper can adopt the orphan instead of PID 1; if this fires on ubuntu, suspect process reparenting first, not codex resolution."
CODEX_THREAD_ID_D=$(grep '^codex_thread_id:' "$STATE_D" | sed 's/^codex_thread_id:[[:space:]]*//' | tr -d '\r')
echo "$SESSION_ID_D" | grep -qE '^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$' \
  || fail "(d): expected a v4 UUID run id, got '${SESSION_ID_D}'"
[[ "$SESSION_ID_D" != "019f48e4-codex-thread" ]] \
  || fail "(d): stable Codex thread was reused as the target session_id"
[[ "$CODEX_THREAD_ID_D" == "019f48e4-codex-thread" ]] \
  || fail "(d): expected codex_thread_id in manifest, got '${CODEX_THREAD_ID_D}'"
pass "(d): Codex thread remains owner metadata while target session id is unique"

# A successful finalize event is the explicit run boundary for claimless
# free-text targets. Re-enter the SAME worktree/thread to prove the prior
# manifest rotates before the next run id is minted. The finalize probe reads
# the event log from the resolved space dir (SPACE_DIR under the stub).
printf '%s\n' \
  "{\"type\":\"session_finalized\",\"data\":{\"session_id\":\"${SESSION_ID_D}\",\"termination_reason\":\"NoWork\",\"ship\":false}}" \
  > "${TMP_D}/space/events.jsonl"
_orphan_init "(d)" "test-codex-thread-id-second-run"
SESSION_ID_E=$(grep '^session_id:' "${TMP_D}/space/target-state.md" | sed 's/^session_id:[[:space:]]*//' | tr -d '\r')
[[ "$SESSION_ID_E" != "$SESSION_ID_D" ]] \
  || fail "(d): two completed targets in one worktree/thread reused session_id '${SESSION_ID_D}'"
compgen -G "${TMP_D}/space/target-state.terminal.*.md" >/dev/null \
  || fail "(d): completed claimless target manifest was not archived"
pass "(d): completed targets in one worktree/thread receive distinct session ids"

# A shipped terminal is also a run boundary (the normal delivery path).
printf '%s\n' \
  "{\"type\":\"session_finalized\",\"data\":{\"session_id\":\"${SESSION_ID_E}\",\"termination_reason\":\"DonePRGreen\",\"ship\":true}}" \
  >> "${TMP_D}/space/events.jsonl"
_orphan_init "(d)" "test-codex-thread-id-third-run"
SESSION_ID_F=$(grep '^session_id:' "${TMP_D}/space/target-state.md" | sed 's/^session_id:[[:space:]]*//' | tr -d '\r')
[[ "$SESSION_ID_F" != "$SESSION_ID_E" ]] \
  || fail "(d): shipped target reused session_id '${SESSION_ID_E}'"
pass "(d): NoWork and shipped terminal boundaries both rotate claimless runs"

# ── (e) The mint door answers nothing => init refuses, writes no manifest ──
log "(e): FNO_TEST_MINT_FAIL=1 => init exits nonzero naming fno doctor update"

make_repo TMP_E
_ALL_TMPS+=("$TMP_E")

(cd "$TMP_E" && \
  HOME="${TMP_E}/home" \
  PATH="${TMP_E}/bin:$PATH" \
  FNO_TEST_SPACE="${TMP_E}/space" \
  FNO_TEST_MINT_FAIL=1 \
  TARGET_START=1 \
  TARGET_INPUT="test-mint-refusal" \
  CODEX_THREAD_ID= \
  TARGET_SESSION_ID= \
  TARGET_LOCATION_OK="main-acknowledged" \
  bash "$INIT" >"$TMP_E/init-stderr.txt" 2>&1
) && fail "(e): init exited zero despite a failed mint"

[[ ! -f "${TMP_E}/space/target-state.md" ]] \
  || fail "(e): target-state.md was written despite a failed mint"
grep -q "fno doctor update" "${TMP_E}/init-stderr.txt" \
  || fail "(e): refusal stderr does not name fno doctor update"
pass "(e): a mint failure refuses init and names the remedy"

# AC1-HP (claude wins over an inherited foreign codex id) and AC3-ERR (the id a
# live row owns is refused) are proven at the verb level in Python
# (test_target_cli.py::test_resolve_owned_identity_verb_refuses_collision):
# the bash hook cannot reliably invoke the source verb during THIS PR's CI,
# because the PATH-resolved `fno` predates the verb. Post-merge the deployed
# fno carries it and the hook exercises it identically to the local e2e.

log "All session_id scenarios passed"
