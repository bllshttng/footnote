#!/usr/bin/env bash
# test-autolaunch-gate.sh - TDD harness for Task 3.1 changes:
#   A. autolaunch-on-ready.sh caller-is-holder gate (absorbs cv-60186ad3)
#   B. hooks/helpers/init-target-state.sh child claim-wait (AC2-FR + AC4-FR)
#
# Tests:
#  1. Caller-is-holder: sandbox with matching target-state.md -> parked caller-is-holder;
#     dispatch-node.sh NEVER invoked.
#  2. Not-holder (no manifest): gate ON + ready -> dispatch stub invoked (today's behavior).
#  3. Manifest present but DIFFERENT node -> blind spawn proceeds (not the holder of THIS node).
#  4. Claim-wait positive: init-target-state.sh + delegated event + acquire retries -> success,
#     .target-cancelled NEVER created.
#  5. Claim-wait timeout: acquire always rc=1 + delegated event -> RESULT: BLOCKED printed,
#     blocked_reason=handoff_claim_wait_timeout, NO cancel sentinel.
#  6. True duplicate (no delegated event): acquire rc=1 -> cancel sentinel + claim_held_by_other.
#
# Bash 3.2 compatible; hermetic mktemp sandboxes.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
AUTOLAUNCH="$REPO_ROOT/skills/blueprint/scripts/autolaunch-on-ready.sh"
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

# ---------------------------------------------------------------------------
# Autolaunch sandbox factory
# ---------------------------------------------------------------------------
TMPDIR_BASE="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_BASE"' EXIT

NODE_ID="ab-deadbeef"
SESSION_ID="20260605T120000Z-12345-aabbcc"

make_autolaunch_sandbox() {
  local name="$1"
  local sbx="$TMPDIR_BASE/al-$name"
  mkdir -p "$sbx/.fno" "$sbx/stub-bin"

  local CALL_LOG="$sbx/call-log"
  touch "$CALL_LOG"

  # Stub dispatch-node.sh (referenced via REPO_ROOT override)
  mkdir -p "$sbx/skills/target/scripts"
  cat > "$sbx/skills/target/scripts/dispatch-node.sh" <<'DISPEOF'
#!/usr/bin/env bash
CALL_LOG="${CALL_LOG:-/dev/null}"
echo "dispatch-node.sh $*" >> "$CALL_LOG"
echo "launched $1 name=tgt-$1 session=abc123"
exit 0
DISPEOF
  chmod +x "$sbx/skills/target/scripts/dispatch-node.sh"

  # Stub fno that returns ready status
  cat > "$sbx/stub-bin/fno" <<'ABIEOF'
#!/usr/bin/env bash
CALL_LOG="${CALL_LOG:-/dev/null}"
echo "fno $*" >> "$CALL_LOG"
subcmd1="${1:-}"
subcmd2="${2:-}"
case "$subcmd1 $subcmd2" in
  "backlog get")
    printf '{"_status":"ready","id":"%s"}\n' "${3:-unknown}"
    exit 0
    ;;
  *)
    exit 0
    ;;
esac
ABIEOF
  chmod +x "$sbx/stub-bin/fno"

  # Stub jq
  cat > "$sbx/stub-bin/jq" <<'JQEOF'
#!/usr/bin/env bash
# Minimal jq stub: handle -r '._status // "unknown"'
input="$(cat)"
if printf '%s' "$input" | grep -q '"_status":"ready"'; then
  echo "ready"
else
  echo "unknown"
fi
exit 0
JQEOF
  chmod +x "$sbx/stub-bin/jq"

  # Stub get_config exported function: returns "true" for auto_launch_on_blueprint
  # autolaunch-on-ready.sh checks: if ! declare -F get_config; then source config.sh
  # We provide a fake config.sh that exports get_config as a function.
  mkdir -p "$sbx/scripts/lib"
  cat > "$sbx/scripts/lib/config.sh" <<'CFGEOF'
get_config() {
  local key="${1:-}"
  local default="${2:-}"
  case "$key" in
    target.auto_launch_on_blueprint) echo "true" ;;
    *) echo "$default" ;;
  esac
}
export -f get_config
CFGEOF

  # Fixture plan. The frontmatter link style is parameterized so the harness can
  # cover all three node-resolution tiers (bug ab-6f93f87a):
  #   claims:        legacy quick/full plan claiming an existing node
  #   graph_node_id: lean single-doc blueprint (the default; never writes claims:)
  #   none:          fresh-intake plan resolved by plan_path against the graph
  local link_mode="${2:-claims}"
  local link_line=""
  case "$link_mode" in
    claims)        link_line="claims: ${NODE_ID}" ;;
    graph_node_id) link_line="graph_node_id: ${NODE_ID}" ;;
    none)          link_line="" ;;
  esac
  {
    echo "---"
    echo "title: Test plan"
    echo "status: ready"
    [ -n "$link_line" ] && echo "$link_line"
    echo "---"
    echo "# Test Plan"
  } > "$sbx/plan.md"

  echo "$sbx"
}

run_autolaunch() {
  local sbx="$1" plan="$2"
  local log="$sbx/call-log"
  set +e
  output=$(
    cd "$sbx" &&
    export CALL_LOG="$log" REPO_ROOT="$sbx" PATH="$sbx/stub-bin:$PATH"
    # Optional graph.json fixture for tier-c (plan_path) resolution tests.
    [ -n "${GRAPH_JSON_FIXTURE:-}" ] && export GRAPH_JSON="$GRAPH_JSON_FIXTURE"
    bash "$AUTOLAUNCH" "$plan" 2>&1
  )
  set -e
  echo "$output"
}

# ---------------------------------------------------------------------------
# Test 1: Caller-is-holder - matching target-state.md -> parked, no dispatch
# ---------------------------------------------------------------------------
echo ""
echo "--- Test 1: caller-is-holder gate ---"
SBX1="$(make_autolaunch_sandbox t1)"
LOG1="$SBX1/call-log"

# Write a target-state.md with graph_node_id matching NODE_ID
cat > "$SBX1/.fno/target-state.md" <<EOF
---
session_id: ${SESSION_ID}
created_at: 2026-06-05T12:00:00Z
plan_path: "plan.md"
---
# Target Session State
graph_node_id: ${NODE_ID}
target_claim_key: "node:${NODE_ID}"
target_claim_holder: "target-session:${SESSION_ID}"
EOF

OUT1="$(run_autolaunch "$SBX1" "$SBX1/plan.md")"
check_contains "T1: parked line present" "parked" "$OUT1"
check_contains "T1: caller-is-holder reason" "caller-is-holder" "$OUT1"
check_log_absent "T1: dispatch-node.sh NOT invoked" "$LOG1" "dispatch-node.sh"

# ---------------------------------------------------------------------------
# Test 2: Not-holder (no manifest) -> dispatch proceeds
# ---------------------------------------------------------------------------
echo ""
echo "--- Test 2: no manifest -> blind spawn ---"
SBX2="$(make_autolaunch_sandbox t2)"
LOG2="$SBX2/call-log"
# No target-state.md present

OUT2="$(run_autolaunch "$SBX2" "$SBX2/plan.md")"
check_contains "T2: auto-launched line present" "auto-launched" "$OUT2"
check_log_present "T2: dispatch-node.sh was invoked" "$LOG2" "dispatch-node.sh"

# ---------------------------------------------------------------------------
# Test 3: Manifest present but DIFFERENT node -> blind spawn proceeds
# ---------------------------------------------------------------------------
echo ""
echo "--- Test 3: manifest with different node -> blind spawn ---"
SBX3="$(make_autolaunch_sandbox t3)"
LOG3="$SBX3/call-log"

DIFFERENT_NODE="ab-ffffffff"
cat > "$SBX3/.fno/target-state.md" <<EOF
---
session_id: ${SESSION_ID}
created_at: 2026-06-05T12:00:00Z
plan_path: "plan.md"
---
# Target Session State
graph_node_id: ${DIFFERENT_NODE}
target_claim_key: "node:${DIFFERENT_NODE}"
target_claim_holder: "target-session:${SESSION_ID}"
EOF

OUT3="$(run_autolaunch "$SBX3" "$SBX3/plan.md")"
check_contains "T3: auto-launched line present" "auto-launched" "$OUT3"
check_log_present "T3: dispatch-node.sh was invoked" "$LOG3" "dispatch-node.sh"
check_not_contains "T3: no caller-is-holder parked" "caller-is-holder" "$OUT3"

# ---------------------------------------------------------------------------
# Test 7: graph_node_id resolution (lean single-doc blueprint, no claims:)
# (bug ab-6f93f87a tier b)
# ---------------------------------------------------------------------------
echo ""
echo "--- Test 7: graph_node_id frontmatter -> resolves + dispatches ---"
SBX7="$(make_autolaunch_sandbox t7 graph_node_id)"
LOG7="$SBX7/call-log"

OUT7="$(run_autolaunch "$SBX7" "$SBX7/plan.md")"
check_contains "T7: auto-launched line present" "auto-launched" "$OUT7"
check_contains "T7: resolved the graph_node_id node" "$NODE_ID" "$OUT7"
check_log_present "T7: dispatch-node.sh invoked with node" "$LOG7" "dispatch-node.sh $NODE_ID"

# ---------------------------------------------------------------------------
# Test 8: plan_path -> graph resolution (fresh-intake plan, no frontmatter link)
# (bug ab-6f93f87a tier c)
# ---------------------------------------------------------------------------
echo ""
echo "--- Test 8: no frontmatter link, resolve by plan_path on the graph ---"
SBX8="$(make_autolaunch_sandbox t8 none)"
LOG8="$SBX8/call-log"
# Graph fixture: a ready node whose plan_path is exactly this plan.
cat > "$SBX8/graph.json" <<EOF
{"entries":[{"id":"${NODE_ID}","_status":"ready","plan_path":"$SBX8/plan.md"}]}
EOF
OUT8="$(GRAPH_JSON_FIXTURE="$SBX8/graph.json" run_autolaunch "$SBX8" "$SBX8/plan.md")"
check_contains "T8: auto-launched via plan_path" "auto-launched" "$OUT8"
check_contains "T8: resolved the plan_path node" "$NODE_ID" "$OUT8"
check_log_present "T8: dispatch-node.sh invoked with node" "$LOG8" "dispatch-node.sh $NODE_ID"

# Test 8b: no link AND no graph match -> honest "nothing to launch" (not a crash)
echo ""
echo "--- Test 8b: no link, no graph match -> nothing to launch ---"
SBX8B="$(make_autolaunch_sandbox t8b none)"
cat > "$SBX8B/graph.json" <<EOF
{"entries":[{"id":"ab-99999999","_status":"ready","plan_path":"/some/other/plan.md"}]}
EOF
OUT8B="$(GRAPH_JSON_FIXTURE="$SBX8B/graph.json" run_autolaunch "$SBX8B" "$SBX8B/plan.md")"
check_contains "T8b: nothing to launch message" "nothing to launch" "$OUT8B"
check_log_absent "T8b: dispatch-node.sh NOT invoked" "$SBX8B/call-log" "dispatch-node.sh"

# ---------------------------------------------------------------------------
# Test 9: a graph_node_id line in the BODY (not frontmatter) is NOT authoritative
# (codex P2 on PR #492): a fresh-intake plan whose prose/fenced example mentions
# another node must still resolve by plan_path, never dispatch the body's node.
# ---------------------------------------------------------------------------
echo ""
echo "--- Test 9: body-only graph_node_id ignored; resolves by plan_path ---"
SBX9="$(make_autolaunch_sandbox t9 none)"
LOG9="$SBX9/call-log"
# Column-0 graph_node_id line AFTER the frontmatter - the old whole-file grep
# would have matched this and dispatched ab-bodyfake.
printf '\ngraph_node_id: ab-bodyfake\n' >> "$SBX9/plan.md"
cat > "$SBX9/graph.json" <<EOF
{"entries":[{"id":"${NODE_ID}","_status":"ready","plan_path":"$SBX9/plan.md"}]}
EOF
OUT9="$(GRAPH_JSON_FIXTURE="$SBX9/graph.json" run_autolaunch "$SBX9" "$SBX9/plan.md")"
check_contains "T9: resolved the plan_path node, not the body's" "$NODE_ID" "$OUT9"
check_not_contains "T9: body's ab-bodyfake NOT dispatched" "ab-bodyfake" "$OUT9"
check_log_present "T9: dispatch-node.sh invoked with plan_path node" "$LOG9" "dispatch-node.sh $NODE_ID"

# ---------------------------------------------------------------------------
# Init-target-state.sh tests (Tests 4, 5, 6)
# These run the REAL init script in a sandbox git repo on a feature branch.
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
    printf '{"_status":"ready","id":"%s"}\n' "${3:-unknown}"
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
echo ""
echo "Results: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
