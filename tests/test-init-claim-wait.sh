#!/usr/bin/env bash
# test-init-claim-wait.sh - hermetic tests for
# hooks/helpers/init-target-state.sh child claim-wait (AC2-FR + AC4-FR):
#   4. Claim-wait positive: init-target-state.sh + delegated event + acquire retries -> success,
#      .target-cancelled NEVER created.
#   5. Claim-wait timeout: acquire always rc=1 + delegated event -> RESULT: BLOCKED printed,
#      blocked_reason=handoff_claim_wait_timeout, NO cancel sentinel.
#   6. True duplicate (no delegated event): acquire rc=1 -> cancel sentinel + claim_held_by_other.


set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INIT_SCRIPT="$REPO_ROOT/hooks/helpers/init-target-state.sh"

pass=0
fail=0

check_eq() {
  local desc="$1" expected="$2" actual="$3"
  if [ "$expected" = "$actual" ]; then
    echo "PASS: $desc"
    pass=$((pass+1))
  else
    echo "FAIL: $desc (expected='$expected' actual='$actual')"
    fail=$((fail+1))
  fi
}

check_contains() {
  local desc="$1" needle="$2" haystack="$3"
  if printf '%s' "$haystack" | grep -qF "$needle"; then
    echo "PASS: $desc"
    pass=$((pass+1))
  else
    echo "FAIL: $desc (needle='$needle' not found in: $haystack)"
    fail=$((fail+1))
  fi
}

check_not_contains() {
  local desc="$1" needle="$2" haystack="$3"
  if printf '%s' "$haystack" | grep -qF "$needle"; then
    echo "FAIL: $desc (needle='$needle' unexpectedly found in output)"
    fail=$((fail+1))
  else
    echo "PASS: $desc"
    pass=$((pass+1))
  fi
}

check_file_exists() {
  local desc="$1" path="$2"
  if [ -e "$path" ]; then
    echo "PASS: $desc"
    pass=$((pass+1))
  else
    echo "FAIL: $desc (file does not exist: $path)"
    fail=$((fail+1))
  fi
}

check_file_absent() {
  local desc="$1" path="$2"
  if [ ! -e "$path" ]; then
    echo "PASS: $desc"
    pass=$((pass+1))
  else
    echo "FAIL: $desc (file should not exist but does: $path)"
    fail=$((fail+1))
  fi
}

check_log_absent() {
  local desc="$1" log="$2" pat="$3"
  set +e
  grep -qF "$pat" "$log" 2>/dev/null
  local _rc=$?
  set -e
  if [ "$_rc" -eq 0 ]; then
    echo "FAIL: $desc (pattern '$pat' unexpectedly found in call log)"
    fail=$((fail+1))
  else
    echo "PASS: $desc"
    pass=$((pass+1))
  fi
}

check_log_present() {
  local desc="$1" log="$2" pat="$3"
  set +e
  grep -qF "$pat" "$log" 2>/dev/null
  local _rc=$?
  set -e
  if [ "$_rc" -eq 0 ]; then
    echo "PASS: $desc"
    pass=$((pass+1))
  else
    echo "FAIL: $desc (pattern '$pat' not found in call log)"
    fail=$((fail+1))
  fi
}

TMPDIR_BASE="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_BASE"' EXIT

# ---------------------------------------------------------------------------
# Init sandbox factory
# ---------------------------------------------------------------------------


make_init_sandbox() {
  local name="$1"
  local sbx="$TMPDIR_BASE/init-$name"
  mkdir -p "$sbx"

  # Create a real git repo on a feature branch (location gate requires non-main)
  (
    cd "$sbx"
    git init -q 2>/dev/null
    git config user.email "test@test.com"
    git config user.name "Test"
    # Rename default branch away from main/master
    git checkout -q -b "feature/handoff-test" 2>/dev/null || true
    echo "# test" > README.md
    git add README.md
    git commit -q -m "init" 2>/dev/null
  )

  # .fno dir
  mkdir -p "$sbx/.fno"

  # Stub fno binary
  mkdir -p "$sbx/stub-bin"
  cat > "$sbx/stub-bin/fno" <<'ABIEOF'
#!/usr/bin/env bash
CALL_LOG="${CALL_LOG:-/dev/null}"
ACQUIRE_RC_FILE="${ACQUIRE_RC_FILE:-}"
ACQUIRE_RC_COUNTER_FILE="${ACQUIRE_RC_COUNTER_FILE:-}"
echo "fno $*" >> "$CALL_LOG"

subcmd1="${1:-}"
subcmd2="${2:-}"
if [ "$subcmd1 $subcmd2" = "agents claim" ]; then
  shift
  subcmd1="${1:-}"
  subcmd2="${2:-}"
fi

case "$subcmd1 $subcmd2" in
  "claim acquire")
    # Support: ACQUIRE_RC_COUNTER_FILE holds "N:M" meaning fail for first N calls, then succeed
    if [ -n "$ACQUIRE_RC_COUNTER_FILE" ] && [ -f "$ACQUIRE_RC_COUNTER_FILE" ]; then
      data="$(cat "$ACQUIRE_RC_COUNTER_FILE")"
      calls="${data%%:*}"
      max_fail="${data##*:}"
      calls=$((calls + 1))
      printf '%s:%s' "$calls" "$max_fail" > "$ACQUIRE_RC_COUNTER_FILE"
      if [ "$calls" -le "$max_fail" ]; then
        exit 1
      fi
      exit 0
    fi
    # Static rc file
    if [ -n "$ACQUIRE_RC_FILE" ] && [ -f "$ACQUIRE_RC_FILE" ]; then
      rc="$(cat "$ACQUIRE_RC_FILE")"
      exit "$rc"
    fi
    exit 0
    ;;
  "backlog get")
    printf '{"status":"ready","id":"%s"}\n' "${3:-unknown}"
    exit 0
    ;;
  *)
    exit 0
    ;;
esac
ABIEOF
  chmod +x "$sbx/stub-bin/fno"

  echo "$sbx"
}

# Run init-target-state.sh in a sandbox.
# Args: sandbox_path node_id session_id [extra env vars as KEY=VAL ...]
run_init() {
  local sbx="$1" node_id="$2" session_id="$3"
  shift 3
  set +e
  output=$(
    cd "$sbx"
    env TARGET_START=1 \
        TARGET_INPUT="$node_id" \
        CLAUDE_CODE_SESSION_ID="${session_id}" \
        TARGET_TRANSCRIPT_ID="${session_id}" \
        CLAUDE_PLUGIN_ROOT="$REPO_ROOT" \
        PATH="$sbx/stub-bin:$PATH" \
        CALL_LOG="$sbx/call-log" \
        "$@" \
        bash "$INIT_SCRIPT" 2>&1
  )
  set -e
  echo "$output"
}

# ---------------------------------------------------------------------------
# Test 4: Claim-wait positive - delegated event + acquire retries -> success
# ---------------------------------------------------------------------------
echo ""
echo "--- Test 4: claim-wait positive (delegated event + retry -> success) ---"
INIT_NODE="ab-00000004"
# Session ID must look like a real CLAUDE_CODE_SESSION_ID UUID for prefix matching
INIT_SID="aabb1234-0000-0000-0000-000000000004"
# The delegated event child_session field uses the first 6-8 hex chars (no dashes)
CHILD_HEX="aabb1234"

SBX4="$(make_init_sandbox t4)"
touch "$SBX4/call-log"

# Write a delegated event that names this session as the child
cat > "$SBX4/.fno/events.jsonl" <<EOF
{"ts":"2026-06-05T12:00:00Z","type":"delegated","source":"target","data":{"node_id":"${INIT_NODE}","from_session":"20260605T110000Z-11111-ffffff","child_session":"${CHILD_HEX}","generation":2,"boundary":"blueprint-do"}}
EOF

# acquire: fail for first 2 calls then succeed (counter: calls:max_fail)
COUNTER_FILE="$SBX4/acquire-counter"
printf '0:2' > "$COUNTER_FILE"

# Run with tiny wait interval so test is fast
OUT4="$(
  run_init "$SBX4" "$INIT_NODE" "$INIT_SID" \
    ACQUIRE_RC_COUNTER_FILE="$COUNTER_FILE" \
    TARGET_CLAIM_WAIT_TIMEOUT=30 \
    TARGET_CLAIM_WAIT_INTERVAL=0
)"

check_file_absent "T4: .target-cancelled NOT created" "$SBX4/.fno/.target-cancelled"
# State file should have been written with claim fields (not just blocked)
if grep -q "target_claim_key" "$SBX4/.fno/target-state.md" 2>/dev/null; then
  echo "PASS: T4: target_claim_key written on eventual success"
  pass=$((pass+1))
else
  echo "FAIL: T4: target_claim_key not found in target-state.md"
  fail=$((fail+1))
fi
check_not_contains "T4: no RESULT BLOCKED" "RESULT: BLOCKED" "$OUT4"

# ---------------------------------------------------------------------------
# Test 5: Claim-wait timeout - acquire always rc=1 + delegated event -> BLOCKED
# ---------------------------------------------------------------------------
echo ""
echo "--- Test 5: claim-wait timeout -> RESULT: BLOCKED ---"
INIT_NODE5="ab-00000005"
INIT_SID5="ccdd5678-0000-0000-0000-000000000005"
CHILD_HEX5="ccdd5678"

SBX5="$(make_init_sandbox t5)"
touch "$SBX5/call-log"

cat > "$SBX5/.fno/events.jsonl" <<EOF
{"ts":"2026-06-05T12:00:00Z","type":"delegated","source":"target","data":{"node_id":"${INIT_NODE5}","from_session":"20260605T110000Z-11111-ffffff","child_session":"${CHILD_HEX5}","generation":2,"boundary":"blueprint-do"}}
EOF

# acquire always fails (rc=1)
ALWAYS_FAIL_FILE="$SBX5/always-fail"
echo "1" > "$ALWAYS_FAIL_FILE"

OUT5="$(
  run_init "$SBX5" "$INIT_NODE5" "$INIT_SID5" \
    ACQUIRE_RC_FILE="$ALWAYS_FAIL_FILE" \
    TARGET_CLAIM_WAIT_TIMEOUT=2 \
    TARGET_CLAIM_WAIT_INTERVAL=0
)"

check_file_absent "T5: .target-cancelled NOT created" "$SBX5/.fno/.target-cancelled"
check_contains "T5: RESULT: BLOCKED printed" "RESULT: BLOCKED" "$OUT5"
if grep -q "handoff_claim_wait_timeout" "$SBX5/.fno/target-state.md" 2>/dev/null; then
  echo "PASS: T5: blocked_reason=handoff_claim_wait_timeout"
  pass=$((pass+1))
else
  echo "FAIL: T5: handoff_claim_wait_timeout not found in target-state.md"
  fail=$((fail+1))
fi

# ---------------------------------------------------------------------------
# Test 6: True duplicate (no delegated event) -> cancel sentinel + claim_held_by_other
# ---------------------------------------------------------------------------
echo ""
echo "--- Test 6: true duplicate (no delegated event) -> cancel sentinel ---"
INIT_NODE6="ab-00000006"
INIT_SID6="eeff9012-0000-0000-0000-000000000006"

SBX6="$(make_init_sandbox t6)"
touch "$SBX6/call-log"

# No events.jsonl (empty)
touch "$SBX6/.fno/events.jsonl"

# acquire always fails rc=1
ALWAYS_FAIL6="$SBX6/always-fail"
echo "1" > "$ALWAYS_FAIL6"

OUT6="$(
  run_init "$SBX6" "$INIT_NODE6" "$INIT_SID6" \
    ACQUIRE_RC_FILE="$ALWAYS_FAIL6"
)"

check_file_exists "T6: .target-cancelled created (true duplicate)" "$SBX6/.fno/.target-cancelled"
if grep -q "claim_held_by_other" "$SBX6/.fno/target-state.md" 2>/dev/null; then
  echo "PASS: T6: blocked_reason=claim_held_by_other"
  pass=$((pass+1))
else
  echo "FAIL: T6: claim_held_by_other not found in target-state.md"
  fail=$((fail+1))
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
# ---------------------------------------------------------------------------
echo ""
echo "Results: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
