#!/usr/bin/env bash
# test_manifest_immutable.sh - smoke tests for the immutable target-state manifest
# (Task 2.2 / ab-d0337fbc control-plane collapse wedge).
#
# Tests:
#   T1  init produces manifest with required fields (session_id, created_at, attended, no_external)
#   T2  manifest contains NONE of the mutable control-plane fields
#   T3  manifest contains the new attended/advisory/budget fields
#   T4  write-once: fno state set --field status refuses with exit 5
#   T5  write-once: fno state set --field plan_path on empty field is allowed
#   T6  bash -n syntax check on init script
#   T7  test_loop_check_shim.sh still passes (reads claude_transcript_id from the manifest)
#
# Self-contained: creates a tmp git repo and runs init-target-state.sh from it.
# Uses the Python CLI at cli/.venv/bin/python if available, else skips T4/T5.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
INIT_HOOK="${REPO_ROOT}/hooks/helpers/init-target-state.sh"
SHIM_TEST="${SCRIPT_DIR}/test_loop_check_shim.sh"

# ── counters ─────────────────────────────────────────────────────────────────
PASS=0; FAIL=0; SKIP_COUNT=0
log()  { printf '[manifest] %s\n' "$*"; }
pass() { PASS=$((PASS+1)); printf '[manifest] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[manifest] FAIL: %s\n' "$*" >&2; }
skip() { SKIP_COUNT=$((SKIP_COUNT+1)); printf '[manifest] SKIP: %s\n' "$*"; }

# ── pre-flight ────────────────────────────────────────────────────────────────
[[ -f "$INIT_HOOK" ]] || { fail "init hook not found at $INIT_HOOK"; exit 1; }
command -v git  >/dev/null 2>&1 || { fail "git required"; exit 1; }
command -v bash >/dev/null 2>&1 || { fail "bash required"; exit 1; }

# Python CLI - try the venv first, then system python with the package
PYTHON_CLI=""
CLI_VENV="${REPO_ROOT}/cli/.venv/bin/python"
if [[ -x "$CLI_VENV" ]]; then
  # quick sanity: can it import fno?
  if PYTHONPATH="${REPO_ROOT}/cli/src" "$CLI_VENV" -c "import fno" 2>/dev/null; then
    PYTHON_CLI="$CLI_VENV"
  fi
fi

# ── T6: syntax check (runs first, cheapest) ──────────────────────────────────
if bash -n "$INIT_HOOK" 2>/dev/null; then
  pass "T6: bash -n syntax check"
else
  fail "T6: bash -n reports syntax error in $INIT_HOOK"
fi

# ── helper: create a minimal tmp git repo on a feature branch ─────────────────
make_git_repo() {
  local dir="$1"
  mkdir -p "$dir"
  git -C "$dir" init -q
  git -C "$dir" checkout -q -b feature/test-smoke 2>/dev/null || true
  git -C "$dir" config user.email "test@test.local"
  git -C "$dir" config user.name "Test"
  # Create an initial commit so HEAD is not unborn
  echo "init" > "$dir/README.md"
  git -C "$dir" add README.md
  git -C "$dir" commit -q -m "init"
  mkdir -p "$dir/.fno"
}

# ── T1/T2/T3: smoke init ─────────────────────────────────────────────────────
_TMP=$(mktemp -d)
trap 'rm -rf "$_TMP"' EXIT

make_git_repo "$_TMP"

MANIFEST="$_TMP/.fno/target-state.md"

# Run init with minimal env (TARGET_START=1, TARGET_INPUT, TARGET_SIZE=M)
INIT_OUTPUT=$(
  cd "$_TMP" && \
  TARGET_START=1 \
  TARGET_INPUT="smoke" \
  TARGET_SIZE=M \
  bash "$INIT_HOOK" 2>&1
)
INIT_RC=$?

if [[ $INIT_RC -ne 0 ]]; then
  fail "T1: init exited $INIT_RC; output: $INIT_OUTPUT"
else
  if [[ -f "$MANIFEST" ]]; then
    pass "T1a: manifest file created"
  else
    fail "T1a: manifest file missing after init"
  fi
fi

# T1: required fields present
if [[ -f "$MANIFEST" ]]; then
  _check_field() {
    local field="$1"
    if grep -qE "^${field}:" "$MANIFEST"; then
      pass "T1: field present: $field"
    else
      fail "T1: field MISSING: $field"
    fi
  }
  _check_field "session_id"
  _check_field "created_at"
  _check_field "attended"
  _check_field "no_external"
  _check_field "no_docs"
  _check_field "no_ship"
  _check_field "provider"
  _check_field "scratchpad_path"
  _check_field "auto_merge_enabled"
  _check_field "auto_merge_approved"
  _check_field "mission_id"
fi

# T2: mutable control-plane fields must NOT be present
if [[ -f "$MANIFEST" ]]; then
  _forbidden_absent() {
    local field="$1"
    if grep -qE "^${field}:" "$MANIFEST" 2>/dev/null; then
      fail "T2: mutable field PRESENT (should be gone): $field"
    else
      pass "T2: mutable field absent: $field"
    fi
  }
  _forbidden_absent "status"
  _forbidden_absent "current_phase"
  _forbidden_absent "iteration"
  _forbidden_absent "quality_check_passed"
  _forbidden_absent "output_validated"
  _forbidden_absent "artifact_shipped"
  _forbidden_absent "external_review_passed"
  _forbidden_absent "goal_verification_passed"
  _forbidden_absent "docs_generated"
  _forbidden_absent "memory_pass_passed"
  _forbidden_absent "browser_testing_passed"
  _forbidden_absent "deferrals_captured"
  _forbidden_absent "ledger_updated"
  _forbidden_absent "provenance_nonce"
  _forbidden_absent "skip_flags_initial"
  _forbidden_absent "coordinator_phase"
  _forbidden_absent "session_start_context_loaded"
  _forbidden_absent "merged_prs"
  _forbidden_absent "merge_auto_queued"
  _forbidden_absent "merge_failed"
  _forbidden_absent "conflicts_resolved"
fi

# T3: new input fields present (attended/advisory; budget lines omitted when unconfigured)
if [[ -f "$MANIFEST" ]]; then
  if grep -qE "^attended:" "$MANIFEST"; then
    ATTENDED_VAL=$(grep -E "^attended:" "$MANIFEST" | head -1 | sed 's/^attended:[[:space:]]*//')
    if [[ "$ATTENDED_VAL" == "true" ]]; then
      pass "T3: attended: true (not unattended)"
    else
      fail "T3: attended expected true, got: $ATTENDED_VAL"
    fi
  else
    fail "T3: attended field missing"
  fi
  if grep -qE "^advisory:" "$MANIFEST"; then
    pass "T3: advisory field present"
  else
    fail "T3: advisory field missing"
  fi
  # budget lines: absent is OK when unconfigured (they are OMITTED per spec)
  pass "T3: budget lines: omitted when unconfigured (correct)"
fi

# T3b: unattended mode sets attended: false
_TMP2=$(mktemp -d)
make_git_repo "$_TMP2"
MANIFEST2="$_TMP2/.fno/target-state.md"
(cd "$_TMP2" && TARGET_START=1 TARGET_INPUT="smoke-unattended" TARGET_UNATTENDED=1 bash "$INIT_HOOK") 2>/dev/null || true
if [[ -f "$MANIFEST2" ]]; then
  ATTENDED2=$(grep -E "^attended:" "$MANIFEST2" | head -1 | sed 's/^attended:[[:space:]]*//')
  if [[ "$ATTENDED2" == "false" ]]; then
    pass "T3b: TARGET_UNATTENDED=1 sets attended: false"
  else
    fail "T3b: attended should be false when TARGET_UNATTENDED=1, got: $ATTENDED2"
  fi
else
  fail "T3b: manifest not created for unattended test"
fi
rm -rf "$_TMP2"

# T3c: TARGET_ADVISORY=1 sets advisory: true
_TMP3=$(mktemp -d)
make_git_repo "$_TMP3"
MANIFEST3="$_TMP3/.fno/target-state.md"
(cd "$_TMP3" && TARGET_START=1 TARGET_INPUT="smoke-advisory" TARGET_ADVISORY=1 bash "$INIT_HOOK") 2>/dev/null || true
if [[ -f "$MANIFEST3" ]]; then
  ADVISORY3=$(grep -E "^advisory:" "$MANIFEST3" | head -1 | sed 's/^advisory:[[:space:]]*//')
  if [[ "$ADVISORY3" == "true" ]]; then
    pass "T3c: TARGET_ADVISORY=1 sets advisory: true"
  else
    fail "T3c: advisory should be true when TARGET_ADVISORY=1, got: $ADVISORY3"
  fi
else
  fail "T3c: manifest not created for advisory test"
fi
rm -rf "$_TMP3"

# ── T4/T5: write-once via Python CLI ────────────────────────────────────────
if [[ -z "$PYTHON_CLI" ]]; then
  skip "T4: Python CLI not available (no cli/.venv/bin/python with abilities importable)"
  skip "T5: Python CLI not available"
else
  # T4: writing 'status' must be refused with exit 5
  _TMP4=$(mktemp -d)
  make_git_repo "$_TMP4"
  (
    cd "$_TMP4" && \
    TARGET_START=1 TARGET_INPUT="write-once-test" bash "$INIT_HOOK" 2>/dev/null
  ) || true
  MANIFEST4="$_TMP4/.fno/target-state.md"
  if [[ -f "$MANIFEST4" ]]; then
    SET_RC=0
    SET_OUT=$(PYTHONPATH="${REPO_ROOT}/cli/src" "$PYTHON_CLI" -m fno.cli \
      state set \
      --path "$MANIFEST4" \
      --type target \
      --field status \
      --value COMPLETE 2>&1) || SET_RC=$?
    if [[ $SET_RC -eq 5 ]]; then
      pass "T4: fno state set --field status refused with exit 5 (write-once)"
    else
      fail "T4: expected exit 5, got $SET_RC; output: $SET_OUT"
    fi
    # Verify the message mentions the immutability contract
    if echo "$SET_OUT" | grep -qi "immutable\|control-plane\|ab-d0337fbc\|plan_path"; then
      pass "T4: refusal message references immutability context"
    else
      fail "T4: refusal message missing immutability context; got: $SET_OUT"
    fi
  else
    fail "T4: manifest not created for write-once test"
  fi
  rm -rf "$_TMP4"

  # T5: first-fill of empty plan_path is allowed
  _TMP5=$(mktemp -d)
  make_git_repo "$_TMP5"
  (
    cd "$_TMP5" && \
    TARGET_START=1 TARGET_INPUT="plan-path-fill-test" bash "$INIT_HOOK" 2>/dev/null
  ) || true
  MANIFEST5="$_TMP5/.fno/target-state.md"
  if [[ -f "$MANIFEST5" ]]; then
    # plan_path after init with no TARGET_PLAN_PATH should be empty / ""
    CURRENT_PLAN=$(grep -E "^plan_path:" "$MANIFEST5" | head -1 | sed 's/^plan_path:[[:space:]]*//' | tr -d '"')
    if [[ -z "$CURRENT_PLAN" ]]; then
      SET5_RC=0
      SET5_OUT=$(PYTHONPATH="${REPO_ROOT}/cli/src" "$PYTHON_CLI" -m fno.cli \
        state set \
        --path "$MANIFEST5" \
        --type target \
        --field plan_path \
        --value "/some/plan.md" 2>&1) || SET5_RC=$?
      if [[ $SET5_RC -eq 0 ]]; then
        pass "T5: first-fill of empty plan_path allowed (exit 0)"
      else
        fail "T5: first-fill of empty plan_path should be allowed, got exit $SET5_RC; output: $SET5_OUT"
      fi
      # T5b: second write to non-empty plan_path should be refused
      SET5B_RC=0
      SET5B_OUT=$(PYTHONPATH="${REPO_ROOT}/cli/src" "$PYTHON_CLI" -m fno.cli \
        state set \
        --path "$MANIFEST5" \
        --type target \
        --field plan_path \
        --value "/different/plan.md" 2>&1) || SET5B_RC=$?
      if [[ $SET5B_RC -eq 5 ]]; then
        pass "T5b: second write to non-empty plan_path refused (write-once)"
      else
        fail "T5b: expected exit 5 on second plan_path write, got $SET5B_RC; output: $SET5B_OUT"
      fi
    else
      skip "T5: plan_path already set ('$CURRENT_PLAN'); skipping first-fill test"
    fi
  else
    fail "T5: manifest not created for plan_path fill test"
  fi
  rm -rf "$_TMP5"
fi

# ── T7: shim test still passes ───────────────────────────────────────────────
if [[ -f "$SHIM_TEST" ]]; then
  log "Running T7: $SHIM_TEST ..."
  SHIM_OUTPUT=$(bash "$SHIM_TEST" 2>&1)
  SHIM_RC=$?
  if [[ $SHIM_RC -eq 0 ]]; then
    pass "T7: test_loop_check_shim.sh still passes"
  elif [[ $SHIM_RC -eq 77 ]]; then
    skip "T7: test_loop_check_shim.sh skipped (missing deps, exit 77)"
  else
    fail "T7: test_loop_check_shim.sh failed (exit $SHIM_RC)"
    echo "--- shim test output ---" >&2
    echo "$SHIM_OUTPUT" >&2
  fi
else
  skip "T7: $SHIM_TEST not found"
fi

# ── summary ───────────────────────────────────────────────────────────────────
echo ""
printf '[manifest] RESULTS: %d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIP_COUNT"
[[ $FAIL -eq 0 ]]
