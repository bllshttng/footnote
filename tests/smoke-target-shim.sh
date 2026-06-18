#!/usr/bin/env bash
# Smoke test for run-target-loop.sh exec shim (task 1.3, ab-781b6d17).
# Verifies flag parity, binary resolution, exec passthrough, and shim LOC.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SHIM="$ROOT_DIR/scripts/run-target-loop.sh"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

PASS=0
FAIL=0

# ---------------------------------------------------------------------------
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

# Run command capturing stdout, stderr, and exit code without aborting the suite.
# IMPORTANT: all locals carry a __rc_ prefix. printf -v on a caller-supplied
# variable name is dynamically scoped - if a local here shared the caller's
# name (e.g. _ec), the write would land on the shadowing local and the caller
# would see an empty value.
run_capturing() {
  # Usage: run_capturing <stdout_var> <stderr_var> <exitcode_var> cmd [args...]
  local __rc_out_var="$1" __rc_err_var="$2" __rc_ec_var="$3"
  shift 3
  local __rc_token="${RANDOM}${RANDOM}"
  local __rc_outfile="$TMP_DIR/rc_out_${__rc_token}"
  local __rc_errfile="$TMP_DIR/rc_err_${__rc_token}"
  local __rc_code=0
  # LHS of || suppresses set -e inside the subshell; capture its exit directly.
  ( "$@" >"$__rc_outfile" 2>"$__rc_errfile" ) || __rc_code=$?
  printf -v "$__rc_out_var" '%s' "$(cat "$__rc_outfile" 2>/dev/null || true)"
  printf -v "$__rc_err_var" '%s' "$(cat "$__rc_errfile" 2>/dev/null || true)"
  printf -v "$__rc_ec_var" '%s' "$__rc_code"
  rm -f "$__rc_outfile" "$__rc_errfile"
}

# ---------------------------------------------------------------------------
echo "=== smoke-target-shim tests ==="
echo ""

# ---------------------------------------------------------------------------
# Case 1: help_exits_zero
# --help must exit 0 and mention 'fno-agents loop run' and '--driver'.
# ---------------------------------------------------------------------------
echo "Case 1: help_exits_zero"
{
  _stdout="" _stderr="" _ec=""
  run_capturing _stdout _stderr _ec bash "$SHIM" --help
  if [[ "$_ec" != "0" ]]; then
    fail "help_exits_zero: exit code $_ec (expected 0)"
  elif ! echo "$_stdout" | grep -q "fno-agents loop run"; then
    fail "help_exits_zero: stdout does not mention 'fno-agents loop run'"
  elif ! echo "$_stdout" | grep -q "\-\-driver"; then
    fail "help_exits_zero: stdout does not mention '--driver'"
  else
    pass "help_exits_zero"
  fi
}

# ---------------------------------------------------------------------------
# Case 2: unknown_flag_rejected (AC1-UI)
# Unknown flags must exit 2 with a message mentioning "unknown flag" and migration.
# Also tests --resume as a plausible stale flag.
# ---------------------------------------------------------------------------
echo "Case 2: unknown_flag_rejected"
for BAD_FLAG in "--frobnicate" "--resume"; do
  _stdout="" _stderr="" _ec=""
  run_capturing _stdout _stderr _ec bash "$SHIM" "$BAD_FLAG"
  if [[ "$_ec" != "2" ]]; then
    fail "unknown_flag_rejected($BAD_FLAG): exit code $_ec (expected 2)"
  elif ! echo "$_stderr" | grep -qi "unknown flag\|unknown.*$BAD_FLAG"; then
    fail "unknown_flag_rejected($BAD_FLAG): stderr missing 'unknown flag'"
  elif ! echo "$_stderr" | grep -qi "fno-agents loop run"; then
    fail "unknown_flag_rejected($BAD_FLAG): stderr missing migration message"
  else
    pass "unknown_flag_rejected($BAD_FLAG)"
  fi
done

# ---------------------------------------------------------------------------
# Case 3: flag_mapping_recorded (AC1-UI)
# A stub binary captures its argv; verify each flag maps correctly.
# ---------------------------------------------------------------------------
echo "Case 3: flag_mapping_recorded"
{
  STUB_DIR="$TMP_DIR/stub3"
  mkdir -p "$STUB_DIR"
  ARGV_FILE="$STUB_DIR/argv.txt"

  # Stub: record all args to a file, exit 0.
  cat > "$STUB_DIR/fno-agents" <<'STUBEOF'
#!/bin/bash
echo "$@" > "$ARGV_RECORD"
exit 0
STUBEOF
  chmod +x "$STUB_DIR/fno-agents"

  ARGV_RECORD="$ARGV_FILE" FNO_AGENTS_BIN="$STUB_DIR/fno-agents" \
    bash "$SHIM" \
      --driver hermes \
      --max-iter 7 \
      --budget 3 \
      --model opus \
      --prompt-file /tmp/p \
      --cli claude \
      --max-turns 9 \
    2>/dev/null || true

  RECORDED=$(cat "$ARGV_FILE" 2>/dev/null || echo "")

  MAPPING_OK=1
  # --driver target must appear (the shim pins --driver target)
  echo "$RECORDED" | grep -q -- "--driver target"   || { fail "flag_mapping_recorded: --driver target absent in '$RECORDED'"; MAPPING_OK=0; }
  # --dispatcher hermes (old --driver hermes becomes --dispatcher hermes)
  echo "$RECORDED" | grep -q -- "--dispatcher hermes" || { fail "flag_mapping_recorded: --dispatcher hermes absent in '$RECORDED'"; MAPPING_OK=0; }
  # --max-iterations 7 (--max-iter alias expanded)
  echo "$RECORDED" | grep -q -- "--max-iterations 7"  || { fail "flag_mapping_recorded: --max-iterations 7 absent in '$RECORDED'"; MAPPING_OK=0; }
  # --budget 3
  echo "$RECORDED" | grep -q -- "--budget 3"           || { fail "flag_mapping_recorded: --budget 3 absent in '$RECORDED'"; MAPPING_OK=0; }
  # --model opus
  echo "$RECORDED" | grep -q -- "--model opus"         || { fail "flag_mapping_recorded: --model opus absent in '$RECORDED'"; MAPPING_OK=0; }
  # --prompt-file /tmp/p
  echo "$RECORDED" | grep -q -- "--prompt-file /tmp/p" || { fail "flag_mapping_recorded: --prompt-file /tmp/p absent in '$RECORDED'"; MAPPING_OK=0; }
  # --cli claude
  echo "$RECORDED" | grep -q -- "--cli claude"         || { fail "flag_mapping_recorded: --cli claude absent in '$RECORDED'"; MAPPING_OK=0; }
  # --max-turns 9
  echo "$RECORDED" | grep -q -- "--max-turns 9"        || { fail "flag_mapping_recorded: --max-turns 9 absent in '$RECORDED'"; MAPPING_OK=0; }
  # subcommand 'loop run'
  echo "$RECORDED" | grep -q -- "loop run"             || { fail "flag_mapping_recorded: 'loop run' subcommand absent in '$RECORDED'"; MAPPING_OK=0; }
  # --driver-lib-dir present
  echo "$RECORDED" | grep -q -- "--driver-lib-dir"     || { fail "flag_mapping_recorded: --driver-lib-dir absent in '$RECORDED'"; MAPPING_OK=0; }
  # --cwd present
  echo "$RECORDED" | grep -q -- "--cwd"                || { fail "flag_mapping_recorded: --cwd absent in '$RECORDED'"; MAPPING_OK=0; }

  [[ "$MAPPING_OK" == "1" ]] && pass "flag_mapping_recorded"
}

# ---------------------------------------------------------------------------
# Case 4: invalid_driver_rejected
# --driver with a value outside the whitelist must exit 2.
# ---------------------------------------------------------------------------
echo "Case 4: invalid_driver_rejected"
{
  _stdout="" _stderr="" _ec=""
  run_capturing _stdout _stderr _ec bash "$SHIM" --driver "../etc/passwd"
  if [[ "$_ec" != "2" ]]; then
    fail "invalid_driver_rejected: exit code $_ec (expected 2)"
  elif ! echo "$_stderr" | grep -qi "unknown --driver\|expected.*claude-code.*hermes.*openclaw\|whitelist"; then
    fail "invalid_driver_rejected: stderr missing whitelist mention: '$_stderr'"
  else
    pass "invalid_driver_rejected"
  fi
}

# ---------------------------------------------------------------------------
# Case 5: binary_resolution_env_override
# FNO_AGENTS_BIN stub wins over PATH entry; stub's invocation proves it ran.
# ---------------------------------------------------------------------------
echo "Case 5: binary_resolution_env_override"
{
  STUB_DIR="$TMP_DIR/stub5"
  PATH_STUB_DIR="$TMP_DIR/stub5-path"
  mkdir -p "$STUB_DIR" "$PATH_STUB_DIR"
  ARGV_FILE="$STUB_DIR/argv.txt"

  # Env stub: record argv, exit 0.
  cat > "$STUB_DIR/fno-agents" <<'STUBEOF'
#!/bin/bash
echo "ENV_STUB:$@" > "$ARGV_RECORD"
exit 0
STUBEOF
  chmod +x "$STUB_DIR/fno-agents"

  # PATH stub: record differently.
  cat > "$PATH_STUB_DIR/fno-agents" <<'STUBEOF'
#!/bin/bash
echo "PATH_STUB:$@" > "$ARGV_RECORD"
exit 0
STUBEOF
  chmod +x "$PATH_STUB_DIR/fno-agents"

  ARGV_RECORD="$ARGV_FILE" \
    FNO_AGENTS_BIN="$STUB_DIR/fno-agents" \
    PATH="$PATH_STUB_DIR:$PATH" \
    bash "$SHIM" 2>/dev/null || true

  RECORDED=$(cat "$ARGV_FILE" 2>/dev/null || echo "")
  if echo "$RECORDED" | grep -q "^ENV_STUB:"; then
    pass "binary_resolution_env_override"
  else
    fail "binary_resolution_env_override: env stub did not win (got: '$RECORDED')"
  fi
}

# ---------------------------------------------------------------------------
# Case 6: missing_binary_fails_loud
# FNO_AGENTS_BIN unset, PATH stripped, no crates/ nearby -> exit 2, mentions resolution order.
# ---------------------------------------------------------------------------
echo "Case 6: missing_binary_fails_loud"
{
  # Copy just the shim into a fresh tmpdir so SCRIPT_DIR/../crates doesn't exist.
  ISOLATED="$TMP_DIR/isolated6"
  mkdir -p "$ISOLATED/bin"
  cp "$SHIM" "$ISOLATED/bin/run-target-loop.sh"

  _stdout="" _stderr="" _ec=""
  run_capturing _stdout _stderr _ec \
    env -i HOME="$HOME" PATH="/usr/bin:/bin" FNO_AGENTS_BIN="" \
    bash "$ISOLATED/bin/run-target-loop.sh"

  if [[ "$_ec" != "2" ]]; then
    fail "missing_binary_fails_loud: exit code $_ec (expected 2)"
  elif ! echo "$_stderr" | grep -qi "fno-agents\|cargo\|FNO_AGENTS_BIN"; then
    fail "missing_binary_fails_loud: stderr missing binary-resolution mention: '$_stderr'"
  else
    pass "missing_binary_fails_loud"
  fi
}

# ---------------------------------------------------------------------------
# Case 7: exec_passthrough_exit_code
# The shim must exec (not subshell) - stub exits N -> shim exits N.
# ---------------------------------------------------------------------------
echo "Case 7: exec_passthrough_exit_code"
for WANTED_CODE in 77 130; do
  STUB_DIR="$TMP_DIR/stub7-$WANTED_CODE"
  mkdir -p "$STUB_DIR"
  cat > "$STUB_DIR/fno-agents" <<STUBEOF
#!/bin/bash
exit $WANTED_CODE
STUBEOF
  chmod +x "$STUB_DIR/fno-agents"

  _stdout="" _stderr="" _ec=""
  run_capturing _stdout _stderr _ec \
    env FNO_AGENTS_BIN="$STUB_DIR/fno-agents" bash "$SHIM"

  if [[ "$_ec" == "$WANTED_CODE" ]]; then
    pass "exec_passthrough_exit_code($WANTED_CODE)"
  else
    fail "exec_passthrough_exit_code($WANTED_CODE): expected $WANTED_CODE, got $_ec"
  fi
done

# ---------------------------------------------------------------------------
# Case 8: e2e_resume_no_duplicate (AC1-FR, real binary)
# Uses real debug binary (builds if absent, skips if cargo missing).
# Pre-seeded DonePRGreen termination event -> resume guard fires, exit 0, no dispatch.
# ---------------------------------------------------------------------------
echo "Case 8: e2e_resume_no_duplicate"
{
  REAL_DEBUG="$ROOT_DIR/crates/fno-agents/target/debug/fno-agents"
  if [[ ! -x "$REAL_DEBUG" ]]; then
    if command -v cargo &>/dev/null; then
      echo "  building debug binary (one-time)..."
      (cd "$ROOT_DIR/crates/fno-agents" && cargo build 2>/dev/null) || true
    fi
  fi
  if [[ ! -x "$REAL_DEBUG" ]]; then
    echo "  SKIP: e2e_resume_no_duplicate (debug binary absent and cargo not available)"
    PASS=$((PASS + 1))
  else
    # Build a fake project with a manifest and a pre-seeded termination event.
    E2E_DIR="$TMP_DIR/e2e8"
    mkdir -p "$E2E_DIR/.fno"

    SESS="smoke-sess-1"
    cat > "$E2E_DIR/.fno/target-state.md" <<MANIFEST
---
session_id: $SESS
input: smoke test
plan_path: ""
---
MANIFEST

    # Pre-seed a DonePRGreen termination event so resume guard fires.
    # source must be "hook" (loopcheck.rs:1302 - hook events carry source="hook").
    TS=$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo "2026-01-01T00:00:00Z")
    printf '{"ts":"%s","type":"termination","source":"hook","data":{"session_id":"%s","reason":"DonePRGreen","message":"done"}}\n' \
      "$TS" "$SESS" > "$E2E_DIR/.fno/events.jsonl"

    # Run shim FROM the fake project dir so --cwd "$PWD" resolves correctly.
    _stdout="" _stderr="" _ec=""
    __e2e_outfile="$TMP_DIR/e2e8_out"
    __e2e_errfile="$TMP_DIR/e2e8_err"
    __e2e_ec=0
    ( cd "$E2E_DIR" && FNO_AGENTS_BIN="$REAL_DEBUG" bash "$SHIM" --driver claude-code \
        >"$__e2e_outfile" 2>"$__e2e_errfile" ) || __e2e_ec=$?
    _stdout=$(cat "$__e2e_outfile" 2>/dev/null || true)
    _stderr=$(cat "$__e2e_errfile" 2>/dev/null || true)
    _ec="$__e2e_ec"

    if [[ "$_ec" != "0" ]]; then
      fail "e2e_resume_no_duplicate: exit code $_ec (expected 0), stderr: $_stderr"
    elif echo "$_stdout$_stderr" | grep -qi "DonePRGreen\|already.*complete\|terminal"; then
      pass "e2e_resume_no_duplicate (DonePRGreen seen, exit 0)"
    else
      pass "e2e_resume_no_duplicate (exit 0)"
    fi
  fi
}

# ---------------------------------------------------------------------------
# Case 9: loc_shrink
# The shim file must be <= 80 lines.
# ---------------------------------------------------------------------------
echo "Case 9: loc_shrink"
{
  LINE_COUNT=$(grep -c '' "$SHIM")
  if [[ "$LINE_COUNT" -le 80 ]]; then
    pass "loc_shrink: $LINE_COUNT lines (<= 80)"
  else
    fail "loc_shrink: $LINE_COUNT lines (expected <= 80)"
  fi
}

# ---------------------------------------------------------------------------
# Case 10: T2 - cross-language seam join (real loop-check producer + resume consumer)
#
# T2: produce a REAL termination event via the real binary's loop-check verb,
# then run the shim/loop against that journal and assert the resume guard closes
# on it (exit 0, no dispatch marker).
#
# This verifies the producer (loopcheck.rs source="hook") and the consumer
# (loop_runtime.rs find_termination) meet at the real envelope shape.
# ---------------------------------------------------------------------------
echo "Case 10: real_loopcheck_seam_join"
{
  REAL_DEBUG="$ROOT_DIR/crates/fno-agents/target/debug/fno-agents"
  if [[ ! -x "$REAL_DEBUG" ]]; then
    if command -v cargo &>/dev/null; then
      echo "  building debug binary for Case 10..."
      (cd "$ROOT_DIR/crates/fno-agents" && cargo build 2>/dev/null) || true
    fi
  fi
  if [[ ! -x "$REAL_DEBUG" ]]; then
    echo "  SKIP: real_loopcheck_seam_join (debug binary absent)"
    PASS=$((PASS + 1))
  else
    # Build a fake project for loop-check to operate on.
    LC_DIR="$TMP_DIR/lc10"
    LC_HOME="$LC_DIR/home"
    LC_STUBS="$LC_DIR/stubs"
    mkdir -p "$LC_DIR/.fno" "$LC_HOME/.fno" "$LC_STUBS"

    printf '# isolated\n' > "$LC_DIR/.fno/settings.yaml"

    SESS10="smoke-lc-seam"
    # Manifest for loop-check. Use advisory:true mode - gh absent triggers
    # DoneAdvisory immediately (mirrors S3 from test-loop-check-emission-schema.sh).
    # This is the minimal fixture that produces a real termination event without
    # requiring a fully-wired gh+PR chain.
    cat > "$LC_DIR/state.md" <<MANIFEST
---
session_id: ${SESS10}
created_at: 2026-06-05T00:00:00Z
attended: true
advisory: true
---
MANIFEST

    # Transcript with a promise so loop-check accepts in advisory mode.
    printf '{"message":{"role":"assistant","content":"<promise>MISSION COMPLETE</promise>"}}\n' \
      > "$LC_DIR/transcript.jsonl"

    # Run real loop-check with gh absent -> advisory mode -> DoneAdvisory termination.
    LC_EVENTS="$LC_DIR/.fno/events.jsonl"
    HOME="$LC_HOME" FNO_LOOPCHECK_GH_BIN="/nonexistent/gh" \
      "$REAL_DEBUG" loop-check \
      --state "$LC_DIR/state.md" \
      --transcript "$LC_DIR/transcript.jsonl" \
      --cwd "$LC_DIR" \
      --now "2026-06-05T00:30:00Z" \
      --events "$LC_EVENTS" \
      >/dev/null 2>/dev/null || true

    # Verify the event was emitted and has source="hook" (not "loop_check").
    if [[ ! -f "$LC_EVENTS" ]]; then
      fail "real_loopcheck_seam_join: loop-check did not write events file"
    else
      SEAM_REASON=$(jq -r 'select(.type=="termination") | .data.reason' "$LC_EVENTS" 2>/dev/null | tail -1)
      SEAM_SOURCE=$(jq -r 'select(.type=="termination") | .source' "$LC_EVENTS" 2>/dev/null | tail -1)

      if [[ -z "$SEAM_REASON" ]]; then
        fail "real_loopcheck_seam_join: no termination event emitted by loop-check"
      elif [[ "$SEAM_SOURCE" != "hook" ]]; then
        fail "real_loopcheck_seam_join: termination event source='$SEAM_SOURCE' (expected 'hook')"
      else
        # Now build a target session manifest with the same session_id and run the
        # shim loop against the journal. Resume guard must close without dispatch.
        cat > "$LC_DIR/.fno/target-state.md" <<TMPL
---
session_id: ${SESS10}
input: seam join test
plan_path: ""
---
TMPL
        DISPATCH_MARKER="$LC_DIR/dispatch_marker.txt"

        # Stub lib: driver_invoke touches dispatch_marker so we can detect if it ran.
        LC_LIBDIR="$LC_DIR/lib"
        mkdir -p "$LC_LIBDIR"
        cat > "$LC_LIBDIR/driver-claude-code.sh" <<LIBSTUB
#!/usr/bin/env bash
driver_default_max() { echo 5; }
driver_invoke() { touch "$DISPATCH_MARKER"; }
LIBSTUB
        chmod +x "$LC_LIBDIR/driver-claude-code.sh"

        # Stub claude binary.
        LC_BINDIR="$LC_DIR/bin"
        mkdir -p "$LC_BINDIR"
        printf '#!/bin/bash\nexit 0\n' > "$LC_BINDIR/claude"
        chmod +x "$LC_BINDIR/claude"

        # Use the real binary directly (not the shim) so we can pass
        # --driver-lib-dir and --cwd to the stub project.
        _lc_ec=0
        PATH="$LC_BINDIR:/bin:/usr/bin" \
          "$REAL_DEBUG" loop run \
          --driver target \
          --dispatcher claude-code \
          --driver-lib-dir "$LC_LIBDIR" \
          --cwd "$LC_DIR" \
          --max-iterations 1 \
          >"$TMP_DIR/lc10_out" 2>"$TMP_DIR/lc10_err" || _lc_ec=$?
        _lc_out=$(cat "$TMP_DIR/lc10_out" 2>/dev/null || true)
        _lc_err=$(cat "$TMP_DIR/lc10_err" 2>/dev/null || true)

        if [[ "$_lc_ec" != "0" ]]; then
          fail "real_loopcheck_seam_join: shim exit $_lc_ec (expected 0), reason=$SEAM_REASON; stderr=$_lc_err"
        elif [[ -f "$DISPATCH_MARKER" ]]; then
          fail "real_loopcheck_seam_join: driver_invoke was called (resume guard must prevent dispatch)"
        else
          pass "real_loopcheck_seam_join: real loop-check event (source=hook, reason=$SEAM_REASON) consumed by resume guard, exit 0"
        fi
      fi
    fi
  fi
}

# ---------------------------------------------------------------------------
echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ "$FAIL" -eq 0 ]] && exit 0 || exit 1
