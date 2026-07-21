#!/usr/bin/env bash
# test-handoff.sh - TDD harness for skills/target/scripts/handoff.sh
#
# Covers:
#  1. AC1-HP  happy path - ordering, manifest archived, sentinel, exit 0, delegated line
#  2. AC1-ERR spawn failure - unwind order, manifest restored, handoff_failed, exit 10
#  3. verify timeout - ask ok but list never shows live; same unwind as spawn failure
#  4. AC1-EDGE missing plan_path - parked, zero claim mutations
#  5. double handoff - sentinel pre-exists, idempotent parked
#  6. generation cap - 3 delegated events pre-seeded -> parked chain-exhausted
#  7. no-pressure park - --boundary wave, probe reports used_pct 30 -> parked
#  8. probe unreadable - probe exits 3 -> handoff_probe_unreadable emitted + parked
#  9. restore_failed - verify fails, archive restore impossible -> exit 12
#
# Poll timeouts are made tiny via env overrides:
#   HANDOFF_VERIFY_TIMEOUT / HANDOFF_VERIFY_INTERVAL
#
# The fake `fno` stub in STUB_BIN logs every invocation to CALL_LOG and
# is scriptable per-scenario via marker files in SCENARIO_DIR:
#   $SCENARIO_DIR/abi-ask-rc      -> numeric rc for `fno agents spawn`
#   $SCENARIO_DIR/abi-ask-out     -> stdout for `fno agents spawn`
#   $SCENARIO_DIR/abi-list-out    -> stdout for `fno agents list` (JSON)
#   $SCENARIO_DIR/abi-claim-rc    -> rc for every `fno claim` invocation
#                                    (default 0; set to non-zero to fail selectively)
#   $SCENARIO_DIR/abi-claim-acquire-rc -> rc for claim acquire only
#   $SCENARIO_DIR/abi-claim-release-rc -> rc for claim release only
#   $SCENARIO_DIR/abi-event-emit-rc    -> rc for fno event emit

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPT="$REPO_ROOT/skills/target/scripts/handoff.sh"
CONTEXT_PROBE="$REPO_ROOT/skills/target/scripts/context-probe.sh"

pass=0
fail=0

# ---------------------------------------------------------------------------
# Assertion helpers
# ---------------------------------------------------------------------------
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

check_exit() {
  local desc="$1" expected="$2" actual="$3"
  if [ "$expected" = "$actual" ]; then
    echo "PASS: $desc"
    pass=$((pass+1))
  else
    echo "FAIL: $desc (expected exit=$expected actual exit=$actual)"
    fail=$((fail+1))
  fi
}

check_contains() {
  local desc="$1" needle="$2" haystack="$3"
  if printf '%s' "$haystack" | grep -qF -- "$needle"; then
    echo "PASS: $desc"
    pass=$((pass+1))
  else
    echo "FAIL: $desc (needle='$needle' not found in output)"
    fail=$((fail+1))
  fi
}

check_not_contains() {
  local desc="$1" needle="$2" haystack="$3"
  # Guard against an empty needle: `grep -F ''` matches every line, which would
  # wrongly report "present". An empty needle here means "nothing to find".
  if [ -n "$needle" ] && printf '%s\n' "$haystack" | grep -qF -- "$needle"; then
    echo "FAIL: $desc (needle='$needle' unexpectedly present in output)"
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

check_log_order() {
  # check_log_order "desc" "CALL_LOG" "first_pattern" "second_pattern"
  # asserts first_pattern's FIRST occurrence precedes second_pattern's FIRST occurrence
  local desc="$1" log="$2" pat1="$3" pat2="$4"
  local line1 line2
  set +e
  line1=$(grep -n "$pat1" "$log" 2>/dev/null | head -1 | cut -d: -f1)
  line2=$(grep -n "$pat2" "$log" 2>/dev/null | head -1 | cut -d: -f1)
  set -e
  if [ -z "$line1" ] || [ -z "$line2" ]; then
    echo "FAIL: $desc (missing pattern - line1='${line1:-ABSENT}' for '$pat1', line2='${line2:-ABSENT}' for '$pat2')"
    fail=$((fail+1))
    return
  fi
  if [ "$line1" -lt "$line2" ]; then
    echo "PASS: $desc"
    pass=$((pass+1))
  else
    echo "FAIL: $desc (ordering violated: '$pat1' at line $line1, '$pat2' at line $line2)"
    fail=$((fail+1))
  fi
}

check_log_absent() {
  local desc="$1" log="$2" pat="$3"
  set +e
  grep -q "$pat" "$log" 2>/dev/null
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

# ---------------------------------------------------------------------------
# Sandbox factory
# Creates a hermetic temp dir with:
#   $SANDBOX/.fno/target-state.md    (fixture manifest)
#   $SANDBOX/plan.md                        (fixture plan file, status: ready)
#   $SANDBOX/.fno/events.jsonl        (empty)
#   $SANDBOX/stub-bin/fno                   (stub binary)
#   $SANDBOX/call-log                       (written by stub)
#   $SANDBOX/scenario/                      (per-scenario marker files)
# ---------------------------------------------------------------------------
TMPDIR_BASE="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_BASE"' EXIT

NODE_ID="ab-12345678"
SESSION_ID="20260605T120000Z-12345-abc"
PLAN_REL="plan.md"
# x-3e70: the successor name is tgt-<node>-<harness>-gN. run_handoff pins the
# harness env deterministically to this value so the expected names below are
# stable regardless of the harness the test itself runs under.
TEST_HARNESS="claude"

make_sandbox() {
  local name="$1"
  local sbx="$TMPDIR_BASE/$name"
  mkdir -p "$sbx/.fno/artifacts/handoff" \
           "$sbx/stub-bin" \
           "$sbx/scenario"

  # Fixture plan file
  cat > "$sbx/plan.md" <<'PLANEOF'
---
title: Test plan
status: ready
---
# Test Plan
PLANEOF

  # Fixture target-state.md
  cat > "$sbx/.fno/target-state.md" <<EOF
---
session_id: ${SESSION_ID}
created_at: 2026-06-05T12:00:00Z
plan_path: "${PLAN_REL}"
target_size: M
auto_merge_approved: false
attended: false
---
# Target Session State
graph_node_id: ${NODE_ID}
target_claim_key: "node:${NODE_ID}"
target_claim_holder: "target-session:${SESSION_ID}"
target_claim_ttl: "2h"
EOF

  # Empty events.jsonl
  touch "$sbx/.fno/events.jsonl"

  CALL_LOG="$sbx/call-log"
  touch "$CALL_LOG"

  # Default stub responses
  echo "0"  > "$sbx/scenario/abi-ask-rc"
  # Group 1 (ab-8b3e4fe0): the claude create is `agents spawn`, whose receipt
  # is one compact JSON line carrying .short_id (handoff.sh parses it via jq).
  printf '{"name": "tgt-x", "short_id": "abc123", "provider": "claude", "status": "live"}\n' > "$sbx/scenario/abi-ask-out"
  # Default list output: shows the agent as live after spawn
  # Will be overridden per scenario
  printf '{"agents":[{"name":"tgt-%s-%s-g2","status":"live"}]}\n' "${NODE_ID:3:8}" "$TEST_HARNESS" > "$sbx/scenario/abi-list-out"

  # Write the expected holder into scenario dir so the stub can read it
  echo "target-session:${SESSION_ID}" > "$sbx/scenario/expected-holder"

  # Write stub fno binary
  cat > "$sbx/stub-bin/fno" <<'STUBEOF'
#!/usr/bin/env bash
# Stub fno - logs every invocation and returns scriptable responses
SCENARIO_DIR="${SCENARIO_DIR:-}"
CALL_LOG="${CALL_LOG:-/dev/null}"

# Log this invocation
echo "fno $*" >> "$CALL_LOG"

# Route by subcommand
subcmd1="${1:-}"
subcmd2="${2:-}"

case "$subcmd1 $subcmd2" in
  "agents spawn")
    rc_file="$SCENARIO_DIR/abi-ask-rc"
    out_file="$SCENARIO_DIR/abi-ask-out"
    rc=0; [ -f "$rc_file" ] && rc=$(cat "$rc_file")
    [ -f "$out_file" ] && cat "$out_file"
    exit "$rc"
    ;;
  "agents list")
    out_file="$SCENARIO_DIR/abi-list-out"
    [ -f "$out_file" ] && cat "$out_file" || echo '{"agents":[]}'
    exit 0
    ;;
  "claim acquire")
    # Check for selective override.
    # abi-claim-acquire-node-rc applies only to node: key acquires;
    # abi-claim-acquire-rc applies to all acquires (fallback).
    _acq_key="${3:-}"
    case "$_acq_key" in
      node:*)
        node_rc_file="$SCENARIO_DIR/abi-claim-acquire-node-rc"
        rc_file="$SCENARIO_DIR/abi-claim-acquire-rc"
        if [ -f "$node_rc_file" ]; then
          rc=$(cat "$node_rc_file")
        elif [ -f "$rc_file" ]; then
          rc=$(cat "$rc_file")
        else
          rc=0
        fi
        ;;
      *)
        rc_file="$SCENARIO_DIR/abi-claim-acquire-rc"
        [ -f "$rc_file" ] && rc=$(cat "$rc_file") || rc=0
        ;;
    esac
    exit "$rc"
    ;;
  "claim release")
    rc_file="$SCENARIO_DIR/abi-claim-release-rc"
    [ -f "$rc_file" ] && rc=$(cat "$rc_file") || rc=0
    exit "$rc"
    ;;
  "claim status")
    # Return live status holding our session's claim
    # The argument after "status" is the claim key
    key="${3:-}"
    expected_holder_file="$SCENARIO_DIR/expected-holder"
    expected_holder=""
    [ -f "$expected_holder_file" ] && expected_holder=$(cat "$expected_holder_file")
    case "$key" in
      node:*)
        # Return that our expected holder holds the node claim
        printf '{"key":"%s","status":"live","holder":"%s"}\n' "$key" "$expected_holder"
        ;;
      dispatch:*)
        # Dispatch reservation: default not held
        printf '{"key":"%s","status":"free"}\n' "$key"
        ;;
    esac
    exit 0
    ;;
  "event emit")
    rc_file="$SCENARIO_DIR/abi-event-emit-rc"
    [ -f "$rc_file" ] && rc=$(cat "$rc_file") || rc=0
    if [ "$rc" -eq 0 ]; then
      # Parse --type, --data, --events, --source from args (simulate real fno writer)
      _type=""; _data=""; _evfile=""; _source="unknown"
      while [ $# -gt 0 ]; do
        case "$1" in
          --type)   _type="${2:-}";   shift 2;;
          --data)   _data="${2:-}";   shift 2;;
          --events) _evfile="${2:-}"; shift 2;;
          --source) _source="${2:-}"; shift 2;;
          *)        shift;;
        esac
      done
      if [ -n "$_evfile" ] && [ -n "$_type" ]; then
        _ts="$(date -u '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null)"
        printf '{"ts":"%s","type":"%s","source":"%s","data":%s}\n' \
          "$_ts" "$_type" "$_source" "${_data:-{}}" >> "$_evfile" 2>/dev/null || true
      fi
    fi
    exit "$rc"
    ;;
  "agents rm")
    exit 0
    ;;
  *)
    # Any other fno command: succeed silently
    exit 0
    ;;
esac
STUBEOF
  chmod +x "$sbx/stub-bin/fno"

  echo "$sbx"
}

run_handoff() {
  # run_handoff <sandbox> <boundary> [extra-args...]
  # Runs script from sandbox cwd so relative PLAN_PATH resolves correctly.
  local sbx="$1" boundary="$2"
  shift 2

  set +e
  if [ $# -gt 0 ]; then
    output=$(
      cd "$sbx" && \
      SCENARIO_DIR="$sbx/scenario" \
      CALL_LOG="$sbx/call-log" \
      FNO_DIR=".fno" \
      HANDOFF_VERIFY_TIMEOUT="${HANDOFF_VERIFY_TIMEOUT:-10}" \
      HANDOFF_VERIFY_INTERVAL="${HANDOFF_VERIFY_INTERVAL:-1}" \
      CLAUDE_CODE_SESSION_ID="test-claude-sid" \
      CODEX_THREAD_ID="" CODEX_SESSION_ID="" GEMINI_SESSION_ID="" \
      PATH="$sbx/stub-bin:$PATH" \
      bash "$SCRIPT" --boundary "$boundary" "$@" 2>&1
    )
  else
    output=$(
      cd "$sbx" && \
      SCENARIO_DIR="$sbx/scenario" \
      CALL_LOG="$sbx/call-log" \
      FNO_DIR=".fno" \
      HANDOFF_VERIFY_TIMEOUT="${HANDOFF_VERIFY_TIMEOUT:-10}" \
      HANDOFF_VERIFY_INTERVAL="${HANDOFF_VERIFY_INTERVAL:-1}" \
      CLAUDE_CODE_SESSION_ID="test-claude-sid" \
      CODEX_THREAD_ID="" CODEX_SESSION_ID="" GEMINI_SESSION_ID="" \
      PATH="$sbx/stub-bin:$PATH" \
      bash "$SCRIPT" --boundary "$boundary" 2>&1
    )
  fi
  handoff_rc=$?
  set -e
}

# ---------------------------------------------------------------------------
# Scenario 1: AC1-HP - happy path
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 1: AC1-HP happy path ==="
SBX="$(make_sandbox s1)"

CALL_LOG="$SBX/call-log"
HANDOFF_VERIFY_TIMEOUT=10 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "blueprint-do"

check_exit "AC1-HP: exits 0" "0" "$handoff_rc"
check_contains "AC1-HP: output contains 'delegated'" "delegated" "$output"
check_contains "AC1-HP: output contains node id" "$NODE_ID" "$output"
check_contains "AC1-HP: output contains generation=2" "generation=2" "$output"

# Ordering assertions from call log
check_log_order "AC1-HP: dispatch acquire BEFORE release" \
  "$CALL_LOG" "claim acquire dispatch:" "claim release node:"
check_log_order "AC1-HP: release BEFORE spawn" \
  "$CALL_LOG" "claim release node:" "agents spawn"
check_log_order "AC1-HP: spawn BEFORE list" \
  "$CALL_LOG" "agents spawn" "agents list"

# Manifest archived, NOT in .fno/
check_file_absent "AC1-HP: target-state.md absent from .fno/" \
  "$SBX/.fno/target-state.md"
check_file_exists "AC1-HP: archived manifest exists" \
  "$SBX/${PLAN_REL}.artifacts/target-state-${SESSION_ID}.md"

# Sentinel exists
check_file_exists "AC1-HP: per-session sentinel exists" \
  "$SBX/.fno/.handoff-done-${SESSION_ID}"

# Handoff brief artifact
check_file_exists "AC1-HP: handoff brief artifact exists" \
  "$SBX/.fno/artifacts/handoff/blueprint-do-${SESSION_ID}.md"

# events.jsonl contains delegated event
set +e
delegated_events=$(grep '"type":"delegated"' "$SBX/.fno/events.jsonl" 2>/dev/null | wc -l | tr -d ' ')
set -e
check_eq "AC1-HP: exactly one delegated event emitted" "1" "$delegated_events"

# session_satisfied event emitted
set +e
satisfied_events=$(grep '"type":"session_satisfied"' "$SBX/.fno/events.jsonl" 2>/dev/null | wc -l | tr -d ' ')
set -e
check_eq "AC1-HP: session_satisfied event emitted" "1" "$satisfied_events"

# H1: events must carry source="target" (not "unknown" or "test")
set +e
delegated_source=$(grep '"type":"delegated"' "$SBX/.fno/events.jsonl" 2>/dev/null | grep -o '"source":"[^"]*"' | head -1)
set -e
check_contains "H1-HP: delegated event has source=target" '"source":"target"' "$delegated_source"

# ---------------------------------------------------------------------------
# Scenario 2: AC1-ERR - spawn failure (ask returns rc=1)
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 2: AC1-ERR spawn failure ==="
SBX="$(make_sandbox s2)"
echo "1" > "$SBX/scenario/abi-ask-rc"
echo "" > "$SBX/scenario/abi-ask-out"

CALL_LOG="$SBX/call-log"
HANDOFF_VERIFY_TIMEOUT=10 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "blueprint-do"

check_exit "AC1-ERR: exits 10 (parked)" "10" "$handoff_rc"
check_contains "AC1-ERR: output contains 'parked'" "parked" "$output"

# Unwind order: node re-acquire BEFORE manifest restore
# The log must show: claim acquire node: AFTER agents spawn
# and manifest must be back in .fno/
check_log_order "AC1-ERR: re-acquire node claim AFTER spawn (unwind order)" \
  "$CALL_LOG" "agents spawn" "claim acquire node:"

# Manifest restored to .fno/
check_file_exists "AC1-ERR: target-state.md restored to .fno/" \
  "$SBX/.fno/target-state.md"

# handoff_failed event emitted
set +e
failed_events=$(grep '"type":"handoff_failed"' "$SBX/.fno/events.jsonl" 2>/dev/null | wc -l | tr -d ' ')
set -e
check_eq "AC1-ERR: handoff_failed event emitted" "1" "$failed_events"

# No delegated event
set +e
delegated_events=$(grep '"type":"delegated"' "$SBX/.fno/events.jsonl" 2>/dev/null | wc -l | tr -d ' ')
set -e
check_eq "AC1-ERR: no delegated event" "0" "$delegated_events"

# BLOCKING-1: re-acquire call in spawn-fail unwind must include --ttl
# The fixture manifest has target_claim_ttl: "2h"; the re-acquire must carry it.
set +e
reacq_ttl_line=$(grep "claim acquire node:" "$CALL_LOG" 2>/dev/null | head -1)
set -e
check_contains "BLOCKING-1: spawn-fail re-acquire includes --ttl" "--ttl" "$reacq_ttl_line"

# ---------------------------------------------------------------------------
# Scenario 3: verify timeout (ask ok, list never shows live agent)
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 3: verify timeout ==="
SBX="$(make_sandbox s3)"
# ask succeeds but list returns empty agents
echo '{"agents":[]}' > "$SBX/scenario/abi-list-out"

CALL_LOG="$SBX/call-log"
HANDOFF_VERIFY_TIMEOUT=3 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "blueprint-do"

check_exit "verify-timeout: exits 10 (parked)" "10" "$handoff_rc"
check_contains "verify-timeout: output contains 'parked'" "parked" "$output"

# Manifest must be restored
check_file_exists "verify-timeout: target-state.md restored" \
  "$SBX/.fno/target-state.md"

# handoff_failed event
set +e
failed_events=$(grep '"type":"handoff_failed"' "$SBX/.fno/events.jsonl" 2>/dev/null | wc -l | tr -d ' ')
set -e
check_eq "verify-timeout: handoff_failed event emitted" "1" "$failed_events"

# Re-acquire claim happens in log
check_log_order "verify-timeout: re-acquire claim AFTER spawn" \
  "$CALL_LOG" "agents spawn" "claim acquire node:"

# ---------------------------------------------------------------------------
# Scenario 4: AC1-EDGE - manifest without plan_path
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 4: AC1-EDGE missing plan_path ==="
SBX="$(make_sandbox s4)"
# Overwrite target-state.md to have empty plan_path
cat > "$SBX/.fno/target-state.md" <<EOF
---
session_id: ${SESSION_ID}
created_at: 2026-06-05T12:00:00Z
plan_path: ""
target_size: M
auto_merge_approved: false
attended: false
---
# Target Session State
graph_node_id: ${NODE_ID}
target_claim_key: "node:${NODE_ID}"
target_claim_holder: "target-session:${SESSION_ID}"
EOF

CALL_LOG="$SBX/call-log"
run_handoff "$SBX" "blueprint-do"

check_exit "AC1-EDGE: exits 10 (parked)" "10" "$handoff_rc"
check_contains "AC1-EDGE: output contains 'parked'" "parked" "$output"

# Zero claim mutations: no claim acquire/release in log
check_log_absent "AC1-EDGE: no claim acquire" "$CALL_LOG" "claim acquire"
check_log_absent "AC1-EDGE: no claim release" "$CALL_LOG" "claim release"

# Manifest untouched
check_file_exists "AC1-EDGE: target-state.md still in .fno/" \
  "$SBX/.fno/target-state.md"

# ---------------------------------------------------------------------------
# Scenario 5: double handoff - sentinel pre-exists
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 5: double handoff - idempotent refusal ==="
SBX="$(make_sandbox s5)"
# Pre-create sentinel
touch "$SBX/.fno/.handoff-done-${SESSION_ID}"

CALL_LOG="$SBX/call-log"
run_handoff "$SBX" "blueprint-do"

check_exit "double-handoff: exits 10 (parked)" "10" "$handoff_rc"
check_contains "double-handoff: output contains 'parked'" "parked" "$output"
check_log_absent "double-handoff: no claim acquire" "$CALL_LOG" "claim acquire"

# ---------------------------------------------------------------------------
# Scenario 6: generation cap
# Pre-seed events.jsonl with 3 delegated events for the node (cap=4 -> child_gen=2+3=5 > 4)
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 6: generation cap ==="
SBX="$(make_sandbox s6)"
# 3 delegated events -> child_gen = 2 + 3 = 5; cap=4; refuse
for i in 1 2 3; do
  printf '{"ts":"2026-06-05T12:0%d:00Z","type":"delegated","source":"target","data":{"node_id":"%s","from_session":"sess%d","to_session":"tgt-%s-%s-g%d","boundary":"blueprint-do","generation":%d,"harness":"%s"}}\n' \
    "$i" "$NODE_ID" "$i" "${NODE_ID:3:8}" "$TEST_HARNESS" "$((i+1))" "$((i+1))" "$TEST_HARNESS" \
    >> "$SBX/.fno/events.jsonl"
done

CALL_LOG="$SBX/call-log"
run_handoff "$SBX" "blueprint-do"

check_exit "gen-cap: exits 10 (parked)" "10" "$handoff_rc"
check_contains "gen-cap: output contains 'parked'" "parked" "$output"
check_contains "gen-cap: reason mentions chain-exhausted" "chain-exhausted" "$output"
check_log_absent "gen-cap: no claim acquire" "$CALL_LOG" "claim acquire"

# ---------------------------------------------------------------------------
# Scenario 7: no-pressure park (--boundary wave, probe reports used_pct 30)
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 7: no-pressure park ==="
SBX="$(make_sandbox s7)"

# Create a fake context-probe.sh in stub-bin that returns low pressure
cat > "$SBX/stub-bin/context-probe.sh" <<'PROBEEOF'
#!/usr/bin/env bash
printf '{"used_tokens":60000,"window_tokens":200000,"used_pct":30,"model":"claude-sonnet-4-6"}\n'
exit 0
PROBEEOF
chmod +x "$SBX/stub-bin/context-probe.sh"

# We need the manifest to have a transcript id for the probe path
# (the stub returns immediately regardless)
CALL_LOG="$SBX/call-log"
run_handoff "$SBX" "wave"

check_exit "no-pressure: exits 10 (parked)" "10" "$handoff_rc"
check_contains "no-pressure: output contains 'parked'" "parked" "$output"
check_contains "no-pressure: reason mentions no-pressure" "no-pressure" "$output"
check_log_absent "no-pressure: no claim acquire" "$CALL_LOG" "claim acquire"

# ---------------------------------------------------------------------------
# Scenario 8: probe unreadable (probe exits 3)
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 8: probe unreadable ==="
SBX="$(make_sandbox s8)"

# Create a context-probe stub that exits 3
cat > "$SBX/stub-bin/context-probe.sh" <<'PROBEEOF'
#!/usr/bin/env bash
exit 3
PROBEEOF
chmod +x "$SBX/stub-bin/context-probe.sh"

CALL_LOG="$SBX/call-log"
run_handoff "$SBX" "wave"

check_exit "probe-unreadable: exits 10 (parked)" "10" "$handoff_rc"
check_contains "probe-unreadable: output contains 'parked'" "parked" "$output"

# handoff_probe_unreadable event must be in events.jsonl
set +e
probe_events=$(grep '"type":"handoff_probe_unreadable"' "$SBX/.fno/events.jsonl" 2>/dev/null | wc -l | tr -d ' ')
set -e
check_eq "probe-unreadable: handoff_probe_unreadable event emitted" "1" "$probe_events"
check_log_absent "probe-unreadable: no claim acquire" "$CALL_LOG" "claim acquire"

# ---------------------------------------------------------------------------
# Scenario 9: restore_failed
# ask succeeds, list never returns live (timeout), mv restore is blocked.
# We simulate restore failure by making .fno/ a read-only dir after
# archive so mv back cannot write.
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 9: restore_failed ==="
SBX="$(make_sandbox s9)"
# ask succeeds; list returns empty so verify times out
echo '{"agents":[]}' > "$SBX/scenario/abi-list-out"

# We need to intercept AFTER the archive mv succeeds but BEFORE restore.
# Strategy: put a shadow `mv` in stub-bin that fails only when the
# destination is .fno/target-state.md (the restore direction).
# First run: archive works (src=.fno/target-state.md -> dst in artifacts)
# Second run (restore): src=artifacts/... -> dst=.fno/target-state.md -> fail
cat > "$SBX/stub-bin/mv" <<'MVSTUB'
#!/usr/bin/env bash
# Shadow mv: fail only when restoring target-state.md (dst ends in target-state.md
# but src does NOT start with .fno/target-state.md).
# Bash 3.2 compat: use for loop to get last arg; use /bin/mv for the real move.
first_arg="$1"
last_arg=""
for _a in "$@"; do last_arg="$_a"; done
case "$last_arg" in
  *target-state.md)
    case "$first_arg" in
      *target-state.md) /bin/mv "$@" ;;  # archive: src IS state file -> allow
      *)                exit 1 ;;         # restore: dst is state file, src is not -> block
    esac
    ;;
  *) /bin/mv "$@" ;;
esac
MVSTUB
chmod +x "$SBX/stub-bin/mv"

CALL_LOG="$SBX/call-log"
HANDOFF_VERIFY_TIMEOUT=3 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "blueprint-do"

check_exit "restore-failed: exits 12" "12" "$handoff_rc"
check_contains "restore-failed: output contains 'handoff-restore-failed'" "handoff-restore-failed" "$output"

# Archive must still be present (helper keeps it in place per spec)
check_file_exists "restore-failed: archived manifest still present" \
  "$SBX/${PLAN_REL}.artifacts/target-state-${SESSION_ID}.md"

# handoff_failed event emitted (with reason=restore_failed)
set +e
failed_events=$(grep '"type":"handoff_failed"' "$SBX/.fno/events.jsonl" 2>/dev/null | wc -l | tr -d ' ')
restore_reason=$(grep '"type":"handoff_failed"' "$SBX/.fno/events.jsonl" 2>/dev/null | grep -o '"reason":"[^"]*"' | head -1)
set -e
check_eq "restore-failed: handoff_failed event emitted" "1" "$failed_events"
check_contains "restore-failed: reason is restore_failed" "restore_failed" "$restore_reason"

# ---------------------------------------------------------------------------
# Scenario 10: C1 - claim-lost on spawn-fail unwind (re-acquire fails)
# ask rc=1 (spawn fails), re-acquire node:X also fails -> exit 12, claim-lost
# line, manifest NOT restored (stays archived), handoff_failed reason=reacquire_failed
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 10: C1 claim-lost on spawn-fail ==="
SBX="$(make_sandbox s10)"
# ask fails
echo "1" > "$SBX/scenario/abi-ask-rc"
echo "" > "$SBX/scenario/abi-ask-out"
# node: acquire fails (simulates another worker grabbed it in the gap)
echo "1" > "$SBX/scenario/abi-claim-acquire-node-rc"

CALL_LOG="$SBX/call-log"
HANDOFF_VERIFY_TIMEOUT=10 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "blueprint-do"

check_exit "C1-spawn: exits 12" "12" "$handoff_rc"
check_contains "C1-spawn: output contains 'handoff-claim-lost'" "handoff-claim-lost" "$output"
check_contains "C1-spawn: reason mentions reacquire" "re-acquire failed" "$output"

# Manifest must NOT be restored (stays archived - parent must not continue)
check_file_absent "C1-spawn: target-state.md NOT restored" \
  "$SBX/.fno/target-state.md"
check_file_exists "C1-spawn: archived manifest still present" \
  "$SBX/${PLAN_REL}.artifacts/target-state-${SESSION_ID}.md"

# handoff_failed event emitted with reason=reacquire_failed
set +e
failed_events=$(grep '"type":"handoff_failed"' "$SBX/.fno/events.jsonl" 2>/dev/null | wc -l | tr -d ' ')
reacq_reason=$(grep '"type":"handoff_failed"' "$SBX/.fno/events.jsonl" 2>/dev/null | grep -o '"reason":"[^"]*"' | head -1)
set -e
check_eq "C1-spawn: handoff_failed event emitted" "1" "$failed_events"
check_contains "C1-spawn: handoff_failed reason=reacquire_failed" "reacquire_failed" "$reacq_reason"

# ---------------------------------------------------------------------------
# Scenario 11: C1 - claim-lost on verify-fail unwind (re-acquire fails)
# ask succeeds, list never live (timeout), re-acquire node:X fails -> exit 12
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 11: C1 claim-lost on verify-fail ==="
SBX="$(make_sandbox s11)"
# ask succeeds but list returns empty so verify times out
echo '{"agents":[]}' > "$SBX/scenario/abi-list-out"
# node: acquire fails during re-acquire
echo "1" > "$SBX/scenario/abi-claim-acquire-node-rc"

CALL_LOG="$SBX/call-log"
HANDOFF_VERIFY_TIMEOUT=3 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "blueprint-do"

check_exit "C1-verify: exits 12" "12" "$handoff_rc"
check_contains "C1-verify: output contains 'handoff-claim-lost'" "handoff-claim-lost" "$output"
check_contains "C1-verify: reason mentions reacquire" "re-acquire failed" "$output"

# Manifest NOT restored
check_file_absent "C1-verify: target-state.md NOT restored" \
  "$SBX/.fno/target-state.md"
check_file_exists "C1-verify: archived manifest still present" \
  "$SBX/${PLAN_REL}.artifacts/target-state-${SESSION_ID}.md"

# handoff_failed reason=reacquire_failed
set +e
failed_events=$(grep '"type":"handoff_failed"' "$SBX/.fno/events.jsonl" 2>/dev/null | wc -l | tr -d ' ')
reacq_reason=$(grep '"type":"handoff_failed"' "$SBX/.fno/events.jsonl" 2>/dev/null | grep -o '"reason":"[^"]*"' | head -1)
set -e
check_eq "C1-verify: handoff_failed event emitted" "1" "$failed_events"
check_contains "C1-verify: handoff_failed reason=reacquire_failed" "reacquire_failed" "$reacq_reason"

# ===========================================================================
# graph_node_id reader robustness (ab-c2edd785)
#
# The wave-boundary handoff falsely parked with "manifest missing
# graph_node_id" when the body reader's fence-counting state machine diverged
# (stray ^---, unterminated frontmatter, CRLF). These scenarios pin the robust
# placement-independent + shape-validated reader. Present-id fixtures must get
# PAST Step 0 (proven by a no-pressure park under --boundary wave, which is
# reached only after the graph_node_id read and the claim-holder check);
# genuine-missing fixtures must STILL park with the missing reason.
# ===========================================================================

# Overwrite a sandbox's manifest with raw bytes (supports CRLF via printf).
# Usage: write_manifest <sandbox> <<'EOF' ... EOF   (LF body)
#        or call write_manifest_crlf for CR injection.
reader_manifest_lf() {
  # reader_manifest_lf <sandbox> <body-after-frontmatter>
  local sbx="$1" body="$2"
  cat > "$sbx/.fno/target-state.md" <<EOF
---
session_id: ${SESSION_ID}
created_at: 2026-06-05T12:00:00Z
plan_path: "${PLAN_REL}"
target_size: M
auto_merge_approved: false
attended: false
---
# Target Session State
${body}
EOF
}

# ---------------------------------------------------------------------------
# Scenario 10: AC1-EDGE - stray ^--- in body before graph_node_id
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 10: stray ^--- before graph_node_id (AC1-EDGE) ==="
SBX="$(make_sandbox s10)"
reader_manifest_lf "$SBX" "Some prose with an embedded YAML excerpt:
---
foo: bar
---
graph_node_id: ${NODE_ID}
target_claim_key: \"node:${NODE_ID}\"
target_claim_holder: \"target-session:${SESSION_ID}\"
target_claim_ttl: \"2h\""
run_handoff "$SBX" "wave"
check_not_contains "stray-fence: NOT parked as missing graph_node_id" \
  "manifest missing graph_node_id" "$output"
check_contains "stray-fence: reached pressure check (no-pressure park)" \
  "no-pressure" "$output"

# ---------------------------------------------------------------------------
# Scenario 11: AC1-EDGE - unterminated frontmatter (single ---)
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 11: unterminated frontmatter (AC1-EDGE) ==="
SBX="$(make_sandbox s11)"
cat > "$SBX/.fno/target-state.md" <<EOF
---
session_id: ${SESSION_ID}
plan_path: "${PLAN_REL}"
graph_node_id: ${NODE_ID}
target_claim_key: "node:${NODE_ID}"
target_claim_holder: "target-session:${SESSION_ID}"
target_claim_ttl: "2h"
EOF
run_handoff "$SBX" "wave"
check_not_contains "unterminated-fm: NOT parked as missing graph_node_id" \
  "manifest missing graph_node_id" "$output"
check_contains "unterminated-fm: reached pressure check (no-pressure park)" \
  "no-pressure" "$output"

# ---------------------------------------------------------------------------
# Scenario 12: AC4-EDGE - leading whitespace on the field line
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 12: leading-whitespace graph_node_id (AC4-EDGE) ==="
SBX="$(make_sandbox s12)"
reader_manifest_lf "$SBX" "  graph_node_id: ${NODE_ID}
target_claim_key: \"node:${NODE_ID}\"
target_claim_holder: \"target-session:${SESSION_ID}\"
target_claim_ttl: \"2h\""
run_handoff "$SBX" "wave"
check_not_contains "leading-ws: NOT parked as missing graph_node_id" \
  "manifest missing graph_node_id" "$output"
check_contains "leading-ws: reached pressure check (no-pressure park)" \
  "no-pressure" "$output"

# ---------------------------------------------------------------------------
# Scenario 13: AC3-HP - CRLF line endings; id read CR-free
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 13: CRLF manifest, clean id (AC3-HP) ==="
SBX="$(make_sandbox s13)"
# Build a fully-CRLF manifest so both frontmatter (session_id) and body
# (graph_node_id) carry trailing \r. A CR-poisoned id would park at the
# holder-mismatch guard (and a CR-poisoned session_id would too); the robust
# readers strip CR so the run reaches the no-pressure park instead.
{
  printf -- '---\r\n'
  printf 'session_id: %s\r\n' "${SESSION_ID}"
  printf 'plan_path: "%s"\r\n' "${PLAN_REL}"
  printf -- '---\r\n'
  printf '# Target Session State\r\n'
  printf 'graph_node_id: %s\r\n' "${NODE_ID}"
  printf 'target_claim_key: "node:%s"\r\n' "${NODE_ID}"
  printf 'target_claim_holder: "target-session:%s"\r\n' "${SESSION_ID}"
  printf 'target_claim_ttl: "2h"\r\n'
} > "$SBX/.fno/target-state.md"
run_handoff "$SBX" "wave"
check_not_contains "crlf: NOT parked as missing graph_node_id" \
  "manifest missing graph_node_id" "$output"
check_not_contains "crlf: NOT parked as holder-mismatch (CR stripped)" \
  "session does not hold" "$output"
check_contains "crlf: reached pressure check (no-pressure park)" \
  "no-pressure" "$output"
# The node:<id> claim-status lookup must use the CR-free key (anchored grep:
# a trailing \r would push the line past the $ end-of-line anchor).
if grep -Eq 'claim status node:'"${NODE_ID}"'$' "$SBX/call-log"; then
  echo "PASS: crlf: claim status called with CR-free node:<id> key"
  pass=$((pass+1))
else
  echo "FAIL: crlf: claim status node:<id> key not CR-free in call log"
  fail=$((fail+1))
fi

# ---------------------------------------------------------------------------
# Scenario 14: AC2-HP - graph_node_id: null still parks missing
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 14: graph_node_id null parks missing (AC2-HP) ==="
SBX="$(make_sandbox s14)"
reader_manifest_lf "$SBX" "graph_node_id: null
target_claim_key: \"node:null\"
target_claim_holder: \"target-session:${SESSION_ID}\"
target_claim_ttl: \"2h\""
run_handoff "$SBX" "wave"
check_exit "null: exits 10 (parked)" "10" "$handoff_rc"
check_contains "null: parks with missing graph_node_id" \
  "manifest missing graph_node_id" "$output"

# ---------------------------------------------------------------------------
# Scenario 15: AC2-ERR - no graph_node_id line at all parks missing
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 15: absent graph_node_id parks missing (AC2-ERR) ==="
SBX="$(make_sandbox s15)"
reader_manifest_lf "$SBX" "target_claim_key: \"node:none\"
target_claim_holder: \"target-session:${SESSION_ID}\"
target_claim_ttl: \"2h\""
run_handoff "$SBX" "wave"
check_exit "absent: exits 10 (parked)" "10" "$handoff_rc"
check_contains "absent: parks with missing graph_node_id" \
  "manifest missing graph_node_id" "$output"

# ---------------------------------------------------------------------------
# Scenario 16: AC2-EDGE - empty value (graph_node_id:) parks missing
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 16: empty graph_node_id value parks missing (AC2-EDGE) ==="
SBX="$(make_sandbox s16)"
reader_manifest_lf "$SBX" "graph_node_id:
target_claim_key: \"node:none\"
target_claim_holder: \"target-session:${SESSION_ID}\"
target_claim_ttl: \"2h\""
run_handoff "$SBX" "wave"
check_exit "empty-value: exits 10 (parked)" "10" "$handoff_rc"
check_contains "empty-value: parks with missing graph_node_id" \
  "manifest missing graph_node_id" "$output"

# ---------------------------------------------------------------------------
# Scenario 17: AC2-FR - prose mention with parenthetical is rejected by shape
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 17: prose graph_node_id rejected by shape (AC2-FR) ==="
SBX="$(make_sandbox s17)"
reader_manifest_lf "$SBX" "graph_node_id: ab-old (deprecated)
target_claim_key: \"node:none\"
target_claim_holder: \"target-session:${SESSION_ID}\"
target_claim_ttl: \"2h\""
run_handoff "$SBX" "wave"
check_exit "prose: exits 10 (parked)" "10" "$handoff_rc"
check_contains "prose: parks with missing graph_node_id (shape rejects parenthetical)" \
  "manifest missing graph_node_id" "$output"

# ---------------------------------------------------------------------------
# Scenario 18: frontmatter graph_node_id (multiline input leak) must NOT shadow
# the body field (codex PR #531 P2). init-target-state.sh escapes only quotes,
# so a multiline /target input carrying a `graph_node_id:` line lands inside the
# frontmatter `input:` value. The body-first reader must still pick the real
# body node, proven by the node:<id> the claim-status check uses.
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 18: frontmatter graph_node_id does not shadow body (codex P2) ==="
SBX="$(make_sandbox s18)"
cat > "$SBX/.fno/target-state.md" <<EOF
---
session_id: ${SESSION_ID}
created_at: 2026-06-05T12:00:00Z
plan_path: "${PLAN_REL}"
input: "rework this
graph_node_id: ab-99999999"
target_size: M
auto_merge_approved: false
attended: false
---
# Target Session State
graph_node_id: ${NODE_ID}
target_claim_key: "node:${NODE_ID}"
target_claim_holder: "target-session:${SESSION_ID}"
target_claim_ttl: "2h"
EOF
run_handoff "$SBX" "wave"
check_not_contains "fm-shadow: NOT parked as missing graph_node_id" \
  "manifest missing graph_node_id" "$output"
# The claim-status lookup must use the BODY node, not the frontmatter leak.
if grep -Eq 'claim status node:'"${NODE_ID}"'($|[^0-9a-f])' "$SBX/call-log"; then
  echo "PASS: fm-shadow: claim status used the body node:${NODE_ID}"
  pass=$((pass+1))
else
  echo "FAIL: fm-shadow: claim status did not use body node:${NODE_ID}"
  fail=$((fail+1))
fi
if grep -Eq 'claim status node:ab-99999999' "$SBX/call-log"; then
  echo "FAIL: fm-shadow: claim status used the frontmatter-leak node:ab-99999999"
  fail=$((fail+1))
else
  echo "PASS: fm-shadow: frontmatter-leak node:ab-99999999 never used"
  pass=$((pass+1))
fi

# ---------------------------------------------------------------------------
# Scenario 19: Codex claim owner differs from the unique target-run session id
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 19: Codex thread-owned claim handoff ==="
SBX="$(make_sandbox s19)"
CODEX_HOLDER="target-session:019f48e1-e641-7170-9ea9-921f07021967"
sed -i.bak \
  "s|target_claim_holder: \"target-session:${SESSION_ID}\"|target_claim_holder: \"${CODEX_HOLDER}\"|" \
  "$SBX/.fno/target-state.md"
rm -f "$SBX/.fno/target-state.md.bak"
printf '%s\n' "$CODEX_HOLDER" > "$SBX/scenario/expected-holder"
run_handoff "$SBX" "blueprint-do"
check_exit "codex-holder: exits 0" "0" "$handoff_rc"
check_contains "codex-holder: delegates successfully" "delegated" "$output"
check_contains "codex-holder: release uses recorded thread owner" \
  "--holder ${CODEX_HOLDER}" "$(cat "$SBX/call-log")"
check_not_contains "codex-holder: release never substitutes run id" \
  "claim release node:${NODE_ID} --holder target-session:${SESSION_ID}" \
  "$(cat "$SBX/call-log")"

# ---------------------------------------------------------------------------
# Scenario 20: AC1-HP - clean-rc spawn, receipt has NO short_id, child registers
# live WITH a short_id in the registry row -> delegated, exit 0, CHILD_SID
# backfilled from the row (x-1adb: receiptless-but-live child is a real
# delegation, not a park).
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 20: AC1-HP live receiptless child (backfill) ==="
SBX="$(make_sandbox s20)"
# Spawn exits 0 but the receipt carries no short_id key.
printf '{"name": "tgt-%s-%s-g2", "provider": "claude", "status": "live"}\n' \
  "${NODE_ID:3:8}" "$TEST_HARNESS" > "$SBX/scenario/abi-ask-out"
# Registry row IS live and DOES carry a short_id to backfill from.
printf '{"agents":[{"name":"tgt-%s-%s-g2","status":"live","short_id":"reg789"}]}\n' \
  "${NODE_ID:3:8}" "$TEST_HARNESS" > "$SBX/scenario/abi-list-out"

CALL_LOG="$SBX/call-log"
HANDOFF_VERIFY_TIMEOUT=10 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "blueprint-do"

check_exit "AC1-HP-backfill: exits 0" "0" "$handoff_rc"
check_contains "AC1-HP-backfill: output contains 'delegated'" "delegated" "$output"
check_contains "AC1-HP-backfill: session backfilled from registry row" "session=reg789" "$output"
# Parent's node claim stays released: no re-acquire on the delegated path.
check_not_contains "AC1-HP-backfill: node claim NOT re-acquired" \
  "claim acquire node:" "$(cat "$CALL_LOG")"
set +e
delegated_child=$(grep '"type":"delegated"' "$SBX/.fno/events.jsonl" 2>/dev/null | grep -o '"child_session":"[^"]*"' | head -1)
set -e
check_contains "AC1-HP-backfill: delegated event child_session=reg789" '"child_session":"reg789"' "$delegated_child"

# ---------------------------------------------------------------------------
# Scenario 21: AC3-FR - phantom spawn: clean rc, no receipt short_id, and NO
# child ever registers -> Step 7 poll times out -> parked, exit 10 (x-1adb: the
# clean-rc/empty-receipt no-child case reuses the existing verify-timeout unwind).
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 21: AC3-FR phantom spawn parks after poll ==="
SBX="$(make_sandbox s21)"
printf '{"name": "tgt-%s-%s-g2", "provider": "claude", "status": "live"}\n' \
  "${NODE_ID:3:8}" "$TEST_HARNESS" > "$SBX/scenario/abi-ask-out"
# No child ever appears in the registry.
echo '{"agents":[]}' > "$SBX/scenario/abi-list-out"

CALL_LOG="$SBX/call-log"
HANDOFF_VERIFY_TIMEOUT=3 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "blueprint-do"

check_exit "AC3-FR: exits 10 (parked)" "10" "$handoff_rc"
check_contains "AC3-FR: output contains 'parked'" "parked" "$output"
check_contains "AC3-FR: reason is verify timeout, not spawn failure" "verify timeout" "$output"
# Reuses the existing no-child unwind: re-acquire node AFTER spawn, manifest restored.
check_log_order "AC3-FR: re-acquire node claim AFTER spawn (existing unwind)" \
  "$CALL_LOG" "agents spawn" "claim acquire node:"
check_file_exists "AC3-FR: target-state.md restored" \
  "$SBX/.fno/target-state.md"
set +e
phantom_delegated=$(grep '"type":"delegated"' "$SBX/.fno/events.jsonl" 2>/dev/null | wc -l | tr -d ' ')
set -e
check_eq "AC3-FR: no delegated event emitted" "0" "$phantom_delegated"

# ---------------------------------------------------------------------------
# Scenario 22: AC4-EDGE - clean-rc/empty-receipt, child live but the registry
# row's short_id is EMPTY ("", registry.py's non-stream default) AND no
# session_id -> delegation still commits (exit 0) with child_session degraded to
# "unknown"; to_session stays CHILD_NAME. Uses short_id:"" not an absent key,
# because "" is the real registry shape (jq // treats "" as truthy).
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 22: AC4-EDGE live child, empty short_id, no session_id ==="
SBX="$(make_sandbox s22)"
printf '{"name": "tgt-%s-%s-g2", "provider": "claude", "status": "live"}\n' \
  "${NODE_ID:3:8}" "$TEST_HARNESS" > "$SBX/scenario/abi-ask-out"
# Live row with empty short_id and empty session_id -> nothing to backfill.
printf '{"agents":[{"name":"tgt-%s-%s-g2","status":"live","short_id":"","session_id":""}]}\n' \
  "${NODE_ID:3:8}" "$TEST_HARNESS" > "$SBX/scenario/abi-list-out"

CALL_LOG="$SBX/call-log"
HANDOFF_VERIFY_TIMEOUT=10 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "blueprint-do"

check_exit "AC4-EDGE: exits 0" "0" "$handoff_rc"
check_contains "AC4-EDGE: session degrades to unknown" "session=unknown" "$output"
set +e
edge_child=$(grep '"type":"delegated"' "$SBX/.fno/events.jsonl" 2>/dev/null | grep -o '"child_session":"[^"]*"' | head -1)
edge_to=$(grep '"type":"delegated"' "$SBX/.fno/events.jsonl" 2>/dev/null | grep -o "\"to_session\":\"tgt-${NODE_ID:3:8}-${TEST_HARNESS}-g2\"" | head -1)
set -e
check_contains "AC4-EDGE: child_session is unknown" '"child_session":"unknown"' "$edge_child"
check_contains "AC4-EDGE: to_session stays CHILD_NAME" \
  "\"to_session\":\"tgt-${NODE_ID:3:8}-${TEST_HARNESS}-g2\"" "$edge_to"

# ---------------------------------------------------------------------------
# Scenario 22b: AC4-EDGE - empty short_id but a present session_id -> the jq
# fallback must fire (select(. != "") drops the empty short_id) and backfill
# from session_id, NOT degrade to "unknown" (gemini finding, PR #378).
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 22b: AC4-EDGE empty short_id falls back to session_id ==="
SBX="$(make_sandbox s22b)"
printf '{"name": "tgt-%s-%s-g2", "provider": "claude", "status": "live"}\n' \
  "${NODE_ID:3:8}" "$TEST_HARNESS" > "$SBX/scenario/abi-ask-out"
printf '{"agents":[{"name":"tgt-%s-%s-g2","status":"live","short_id":"","session_id":"sess456"}]}\n' \
  "${NODE_ID:3:8}" "$TEST_HARNESS" > "$SBX/scenario/abi-list-out"

CALL_LOG="$SBX/call-log"
HANDOFF_VERIFY_TIMEOUT=10 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "blueprint-do"

check_exit "AC4-EDGE-fallback: exits 0" "0" "$handoff_rc"
check_contains "AC4-EDGE-fallback: backfills from session_id, not unknown" "session=sess456" "$output"
set +e
fb_child=$(grep '"type":"delegated"' "$SBX/.fno/events.jsonl" 2>/dev/null | grep -o '"child_session":"[^"]*"' | head -1)
set -e
check_contains "AC4-EDGE-fallback: child_session=sess456" '"child_session":"sess456"' "$fb_child"

# ---------------------------------------------------------------------------
# Scenario 23: AC5-EDGE - clean-rc/empty-receipt, child never goes live AND the
# post-timeout re-acquire fails (a lagging child won the claim) -> exit 12
# handoff-claim-lost; parent must NOT continue, manifest stays archived.
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 23: AC5-EDGE timeout re-acquire loses race ==="
SBX="$(make_sandbox s23)"
printf '{"name": "tgt-%s-%s-g2", "provider": "claude", "status": "live"}\n' \
  "${NODE_ID:3:8}" "$TEST_HARNESS" > "$SBX/scenario/abi-ask-out"
echo '{"agents":[]}' > "$SBX/scenario/abi-list-out"
# Re-acquire of node:<id> fails because a lagging child already claimed it.
echo "1" > "$SBX/scenario/abi-claim-acquire-node-rc"

CALL_LOG="$SBX/call-log"
HANDOFF_VERIFY_TIMEOUT=3 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "blueprint-do"

check_exit "AC5-EDGE: exits 12 (claim-lost)" "12" "$handoff_rc"
check_contains "AC5-EDGE: output is handoff-claim-lost" "handoff-claim-lost" "$output"
check_file_absent "AC5-EDGE: manifest stays archived (not restored)" \
  "$SBX/.fno/target-state.md"
set +e
lost_reason=$(grep '"type":"handoff_failed"' "$SBX/.fno/events.jsonl" 2>/dev/null | grep -o '"reason":"[^"]*"' | head -1)
set -e
check_contains "AC5-EDGE: reason is reacquire_failed" "reacquire_failed" "$lost_reason"

# ---------------------------------------------------------------------------
# Scenario 6: x-3ad5 - the plan-status gate accepts the canonical in_review
#
# The ship gate stamps `in_review`. If this gate still listed only
# ready|in_progress|shipped, a high-context target that opened its PR would be
# parked as "unknown plan status" instead of spawning its successor - a silent
# branch, since parking is a legal outcome and nothing errors.
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 6: x-3ad5 plan-status gate accepts in_review + retired spelling ==="

for st in in_review shipped; do
  SBX="$(make_sandbox "s6-$st")"
  sed -i.bak "s/^status: ready$/status: $st/" "$SBX/$PLAN_REL"
  rm -f "$SBX/$PLAN_REL.bak"
  CALL_LOG="$SBX/call-log"
  HANDOFF_VERIFY_TIMEOUT=10 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "blueprint-do"

  check_exit "x-3ad5: status=$st exits 0 (not parked)" "0" "$handoff_rc"
  check_contains "x-3ad5: status=$st delegates" "delegated" "$output"
done

# ...and a genuinely unknown status is still refused.
SBX="$(make_sandbox s6-bogus)"
sed -i.bak "s/^status: ready$/status: not_a_status/" "$SBX/$PLAN_REL"
rm -f "$SBX/$PLAN_REL.bak"
CALL_LOG="$SBX/call-log"
HANDOFF_VERIFY_TIMEOUT=10 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "blueprint-do"
check_contains "x-3ad5: an unknown status is still parked" "parked" "$output"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "================================"
echo "Results: $pass passed, $fail failed"
echo "================================"

if [ "$fail" -gt 0 ]; then
  exit 1
fi
exit 0
