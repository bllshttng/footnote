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
#   $SCENARIO_DIR/fno-ask-rc      -> numeric rc for `fno agents spawn`
#   $SCENARIO_DIR/fno-ask-out     -> stdout for `fno agents spawn`
#   $SCENARIO_DIR/fno-list-out    -> stdout for `fno agents list` (JSON)
#   $SCENARIO_DIR/fno-claim-rc    -> rc for every `fno agents claim` invocation
#                                    (default 0; set to non-zero to fail selectively)
#   $SCENARIO_DIR/fno-claim-acquire-rc -> rc for claim acquire only
#   $SCENARIO_DIR/fno-claim-release-rc -> rc for claim release only
#   $SCENARIO_DIR/fno-event-emit-rc    -> rc for fno doctor event emit

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPT="$REPO_ROOT/skills/target/scripts/handoff.sh"
CONTEXT_PROBE="$REPO_ROOT/skills/target/scripts/context-probe.sh"

# The `fno` stub delegates `do plan rung` to the real CLI (see the stub body), so
# it needs this checkout's sources and an interpreter that can import them.
# Prefer the worktree venv; a bare python3 works when the deps are present.
export FNO_SRC="$REPO_ROOT/cli/src"
# Pick an interpreter that can actually IMPORT the CLI, and refuse to run if
# none can. Choosing one that merely exists is how this suite silently went
# ~60% vacuous: a linked worktree has no cli/.venv, the bare-python3 fallback
# lacked typer, `fno do plan rung` died, and every scenario parked at that gate
# while still matching its loose "parked" assertion (x-f804).
export FNO_PYTHON=""
for _cand in \
  "$REPO_ROOT/cli/.venv/bin/python" \
  "$(dirname "$(git -C "$REPO_ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)")/cli/.venv/bin/python" \
  "$(command -v python3 || true)" \
  "$(command -v python || true)"
do
  [ -n "$_cand" ] && [ -x "$_cand" ] || continue
  if PYTHONPATH="$FNO_SRC" "$_cand" -c 'import fno.cli' >/dev/null 2>&1; then
    export FNO_PYTHON="$_cand"
    break
  fi
done
if [ -z "$FNO_PYTHON" ]; then
  echo "test-handoff: no interpreter can import fno.cli (tried the worktree venv," >&2
  echo "  the canonical checkout's venv, and python3/python on PATH)." >&2
  echo "  Fix: create cli/.venv (cd cli && uv sync) - refusing to run vacuously." >&2
  exit 1
fi

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
TEST_NODE_SLUG="test-node"
TEST_CHILD_SESSION="00000000-0000-0000-0000-000000abc123"
TEST_CAPABILITY_NONCE="capability-nonce-for-tests"
expected_child_name() {
  printf 'target-%s-%s-g2' "$NODE_ID" "$TEST_NODE_SLUG"
}
capability_digest() {
  local sbx="$1"
  printf '%s\n%s\n%s' "$TEST_CAPABILITY_NONCE" "$sbx" "$sbx" \
    | shasum -a 256 | awk '{print $1}'
}

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

  # The explicit transaction is opt-in, but tests must not inherit the
  # operator's machine-wide off switch.
  cat > "$sbx/.fno/config.toml" <<'CONFIGEOF'
[autonomy]
enabled = true

[target.handoff]
enabled = true
generation_cap = 4
used_pct_trigger = 50
CONFIGEOF

  CALL_LOG="$sbx/call-log"
  touch "$CALL_LOG"

  # Default stub responses
  echo "0"  > "$sbx/scenario/fno-ask-rc"
  # Group 1 (ab-8b3e4fe0): the claude create is `agents spawn`, whose receipt
  # is one compact JSON line carrying .short_id (handoff.sh parses it via jq).
  printf '{"name":"%s","short_id":"abc123","session_id":"%s","harness":"claude","status":"live","bound":true,"readiness":"ready","model":"opus"}\n' \
    "$(expected_child_name)" "$TEST_CHILD_SESSION" > "$sbx/scenario/fno-ask-out"
  printf '{"state":"your-move","last_message":"FNO_CAPABILITY_READY:%s","observed_model":{"kind":"observed","model":"opus","samples":1}}\n' \
    "$(capability_digest "$sbx")" > "$sbx/scenario/fno-truth-out"
  # Default list output: shows the agent as live after spawn
  # Will be overridden per scenario
  printf '{"agents":[{"name":"%s","status":"live","session_id":"%s"}]}\n' \
    "$(expected_child_name)" "$TEST_CHILD_SESSION" > "$sbx/scenario/fno-list-out"
  echo "$NODE_ID" > "$sbx/scenario/node-id"
  echo "$TEST_NODE_SLUG" > "$sbx/scenario/node-slug"
  echo "$TEST_CHILD_SESSION" > "$sbx/scenario/child-session"
  touch "$sbx/scenario/activate-child"

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

if [ "$subcmd1 $subcmd2" = "agents claim" ]; then
  shift
  subcmd1="${1:-}"
  subcmd2="${2:-}"
fi

if [ "$subcmd1 $subcmd2 ${3:-}" = "do plan rung" ]; then
  # Delegate to the REAL implementation instead of stubbing a verdict. This
  # gate's entire contract IS the rung vocabulary, so a stub that re-derived
  # it would test the stub - and a second copy of the vocabulary is the exact
  # thing `fno do plan rung` exists to delete.
  exec env PYTHONPATH="$FNO_SRC" "$FNO_PYTHON" -m fno.cli do plan rung "${4:-}"
fi

if [ "$subcmd1 $subcmd2" = "plan rung" ]; then
  echo "deprecated plan root reached" >&2
  exit 2
fi

case "$subcmd1 $subcmd2" in
  "agents spawn")
    rc_file="$SCENARIO_DIR/fno-ask-rc"
    out_file="$SCENARIO_DIR/fno-ask-out"
    rc=0; [ -f "$rc_file" ] && rc=$(cat "$rc_file")
    [ -f "$out_file" ] && cat "$out_file"
    exit "$rc"
    ;;
  "agents list")
    out_file="$SCENARIO_DIR/fno-list-out"
    [ -f "$out_file" ] && cat "$out_file" || echo '{"agents":[]}'
    exit 0
    ;;
  "agents truth")
    out_file="$SCENARIO_DIR/fno-truth-out"
    rc_file="$SCENARIO_DIR/fno-truth-rc"
    rc=0; [ -f "$rc_file" ] && rc=$(cat "$rc_file")
    [ -f "$out_file" ] && cat "$out_file"
    exit "$rc"
    ;;
  "agents mail")
    rc_file="$SCENARIO_DIR/fno-mail-rc"
    rc=0; [ -f "$rc_file" ] && rc=$(cat "$rc_file")
    if [ "$rc" -eq 0 ] && [ -f "$SCENARIO_DIR/activate-child" ]; then
      child=$(cat "$SCENARIO_DIR/child-session")
      node=$(cat "$SCENARIO_DIR/node-id")
      echo "target-session:$child" > "$SCENARIO_DIR/expected-holder"
      manifest_child="$child"
      [ -f "$SCENARIO_DIR/wrong-child-manifest" ] && manifest_child="wrong-session"
      printf '%s\n' \
        '---' \
        "session_id: child-run" \
        "harness_session_id: $manifest_child" \
        '---' \
        '# Target Session State' \
        "graph_node_id: $node" \
        "target_claim_key: \"node:$node\"" \
        "target_claim_holder: \"target-session:$child\"" \
        > .fno/target-state.md
      printf 'delivered (hosted) to %s\n' "$child"
    fi
    exit "$rc"
    ;;
  "backlog get")
    printf '{"id":"%s","slug":"%s"}\n' \
      "$(cat "$SCENARIO_DIR/node-id")" "$(cat "$SCENARIO_DIR/node-slug")"
    exit 0
    ;;
  "claim acquire")
    # Check for selective override.
    # fno-claim-acquire-node-rc applies only to node: key acquires;
    # fno-claim-acquire-rc applies to all acquires (fallback).
    _acq_key="${3:-}"
    case "$_acq_key" in
      node:*)
        node_rc_file="$SCENARIO_DIR/fno-claim-acquire-node-rc"
        rc_file="$SCENARIO_DIR/fno-claim-acquire-rc"
        if [ -f "$node_rc_file" ]; then
          rc=$(cat "$node_rc_file")
        elif [ -f "$rc_file" ]; then
          rc=$(cat "$rc_file")
        else
          rc=0
        fi
        ;;
      *)
        rc_file="$SCENARIO_DIR/fno-claim-acquire-rc"
        [ -f "$rc_file" ] && rc=$(cat "$rc_file") || rc=0
        ;;
    esac
    exit "$rc"
    ;;
  "claim release")
    rc_file="$SCENARIO_DIR/fno-claim-release-rc"
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
  "doctor event")
    rc_file="$SCENARIO_DIR/fno-event-emit-rc"
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
  whoami*)
    # Delegate to the REAL implementation. The context-probe shim resolves to
    # `fno whoami context`, so a stub that silently no-ops it via the *) catch-all
    # would hand every pressure scenario an empty reading and let the suite
    # pass vacuously - the x-f804 hazard through a different door. The probe's
    # contract IS reading real context, so a stub that fakes a reading tests the
    # stub. Same precedent and reasoning as `do plan rung` above.
    exec env PYTHONPATH="$FNO_SRC" "$FNO_PYTHON" -m fno.cli "$@"
    ;;
  *)
    # Any other fno command: succeed silently
    exit 0
    ;;
esac
STUBEOF
  chmod +x "$sbx/stub-bin/fno"

  # The probe prefers `fno-py`, and an ambient deployed fno-py predating the
  # verb fold does not know `fno whoami context`, so scenarios 7/7b would read
  # empty and park without the measurement. Delegate to the same FNO_PYTHON
  # the stub's doors use, keeping the suite hermetic against the machine.
  printf '#!/usr/bin/env bash\nexec env PYTHONPATH="%s" "%s" -m fno.cli "$@"\n' \
    "$FNO_SRC" "$FNO_PYTHON" > "$sbx/stub-bin/fno-py"
  chmod +x "$sbx/stub-bin/fno-py"

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
      HOME="${HANDOFF_TEST_HOME:-$HOME}" \
      HANDOFF_CAPABILITY_NONCE="$TEST_CAPABILITY_NONCE" \
      HANDOFF_CAPABILITY_EXPECTED_CWD="$sbx" \
      HANDOFF_CAPABILITY_EXPECTED_ROOT="$sbx" \
      CLAUDE_CODE_SESSION_ID="test-claude-sid" \
      CODEX_THREAD_ID="" CODEX_SESSION_ID="" GEMINI_SESSION_ID="" \
      PATH="$sbx/stub-bin:$PATH" \
      bash "$SCRIPT" --harness claude --model opus "$@" 2>&1
    )
  else
    output=$(
      cd "$sbx" && \
      SCENARIO_DIR="$sbx/scenario" \
      CALL_LOG="$sbx/call-log" \
      FNO_DIR=".fno" \
      HANDOFF_VERIFY_TIMEOUT="${HANDOFF_VERIFY_TIMEOUT:-10}" \
      HANDOFF_VERIFY_INTERVAL="${HANDOFF_VERIFY_INTERVAL:-1}" \
      HOME="${HANDOFF_TEST_HOME:-$HOME}" \
      HANDOFF_CAPABILITY_NONCE="$TEST_CAPABILITY_NONCE" \
      HANDOFF_CAPABILITY_EXPECTED_CWD="$sbx" \
      HANDOFF_CAPABILITY_EXPECTED_ROOT="$sbx" \
      CLAUDE_CODE_SESSION_ID="test-claude-sid" \
      CODEX_THREAD_ID="" CODEX_SESSION_ID="" GEMINI_SESSION_ID="" \
      PATH="$sbx/stub-bin:$PATH" \
      bash "$SCRIPT" --harness claude --model opus 2>&1
    )
  fi
  handoff_rc=$?
  set -e
}

# ---------------------------------------------------------------------------
# Scenario 0: legacy boundary pressure is no longer a handoff trigger
# ---------------------------------------------------------------------------
set +e
legacy_output="$(bash "$SCRIPT" --boundary wave 2>&1)"
legacy_rc=$?
set -e
check_exit "explicit escalation: legacy --boundary invocation refuses" "2" "$legacy_rc"
check_contains "explicit escalation: refusal names required destination" \
  "capability escalation requires --harness and --model" "$legacy_output"

# ---------------------------------------------------------------------------
# Scenario 1: AC1-HP - happy path
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 1: AC1-HP happy path ==="
SBX="$(make_sandbox s1)"

printf '%s\n' \
  '{"ts":"2026-06-05T11:58:00Z","type":"builder_step","source":"target","data":{"node_id":"ab-deadbeef","tried":"foreign attempt","outcome":"failed"}}' \
  '{"ts":"2026-06-05T11:59:00Z","type":"builder_step","source":"target","data":{"node_id":"ab-12345678","tried":"current attempt","outcome":"worked"}}' \
  >> "$SBX/.fno/events.jsonl"

CALL_LOG="$SBX/call-log"
HANDOFF_VERIFY_TIMEOUT=10 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "blueprint-do"

check_exit "AC1-HP: exits 0" "0" "$handoff_rc"
check_contains "AC1-HP: output contains 'delegated'" "delegated" "$output"
check_contains "AC1-HP: output contains node id" "$NODE_ID" "$output"
check_contains "AC1-HP: output contains generation=2" "generation=2" "$output"

# Ordering assertions from call log
check_log_order "AC1-HP: dispatch acquire BEFORE release" \
  "$CALL_LOG" "claim acquire dispatch:" "claim release node:"
check_log_order "AC1-HP: spawn BEFORE release" \
  "$CALL_LOG" "agents spawn" "claim release node:"
check_log_order "AC1-HP: spawn BEFORE target seed" \
  "$CALL_LOG" "agents spawn" "agents mail"

# Parent manifest archived and replaced by the child-bound target manifest.
check_file_exists "AC1-HP: child target-state.md present" \
  "$SBX/.fno/target-state.md"
check_contains "AC1-HP: child manifest names child harness session" \
  "harness_session_id: $TEST_CHILD_SESSION" "$(cat "$SBX/.fno/target-state.md")"
check_file_exists "AC1-HP: archived manifest exists" \
  "$SBX/${PLAN_REL}.artifacts/target-state-${SESSION_ID}.md"

# Sentinel exists
check_file_exists "AC1-HP: per-session sentinel exists" \
  "$SBX/.fno/.handoff-done-${SESSION_ID}"

# Handoff brief artifact
check_file_exists "AC1-HP: handoff brief artifact exists" \
  "$SBX/.fno/artifacts/handoff/capability-${SESSION_ID}.md"
brief=$(cat "$SBX/.fno/artifacts/handoff/capability-${SESSION_ID}.md")
check_contains "AC1-HP: current-node builder crumb reaches successor" "current attempt" "$brief"
check_not_contains "AC1-HP: foreign-node builder crumb stays isolated" "foreign attempt" "$brief"

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
check_log_order "capability: spawn BEFORE truth" \
  "$CALL_LOG" "agents spawn" "agents truth"
check_log_order "capability: truth BEFORE parent claim release" \
  "$CALL_LOG" "agents truth" "claim release node:"
check_log_order "target execution: parent release BEFORE raw seed" \
  "$CALL_LOG" "claim release node:" "agents mail"
check_contains "destination name uses full node id and slug" \
  "child=$(expected_child_name)" "$output"
delegated_row=$(grep '"type":"delegated"' "$SBX/.fno/events.jsonl" 2>/dev/null | tail -1)
check_contains "delegated event records destination harness" \
  '"harness":"claude"' "$delegated_row"
check_contains "delegated event records destination model" \
  '"model":"opus"' "$delegated_row"
check_contains "delegated event records capability kind" \
  '"handoff_kind":"capability_escalation"' "$delegated_row"

# ---------------------------------------------------------------------------
# Scenario 1b: exact nonce response is required before parent mutation
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 1b: wrong capability nonce response ==="
SBX="$(make_sandbox s1b)"
printf '{"state":"your-move","last_message":"FNO_CAPABILITY_READY:wrong","observed_model":{"kind":"observed","model":"opus","samples":1}}\n' \
  > "$SBX/scenario/fno-truth-out"

CALL_LOG="$SBX/call-log"
run_handoff "$SBX" "capability"

check_exit "capability nonce mismatch parks" "10" "$handoff_rc"
check_contains "capability nonce mismatch names probe stage" "capability_probe" "$output"
check_file_exists "capability nonce mismatch keeps parent manifest" "$SBX/.fno/target-state.md"
check_log_absent "capability nonce mismatch keeps parent claim" "$CALL_LOG" "claim release node:"

# ---------------------------------------------------------------------------
# Scenario 1c: transcript-observed model must match the configured model
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 1c: observed model mismatch ==="
SBX="$(make_sandbox s1c)"
printf '{"state":"your-move","last_message":"FNO_CAPABILITY_READY:%s","observed_model":{"kind":"observed","model":"claude-sonnet-5","samples":1}}\n' \
  "$(capability_digest "$SBX")" > "$SBX/scenario/fno-truth-out"

CALL_LOG="$SBX/call-log"
run_handoff "$SBX" "capability"

check_exit "observed model mismatch parks" "10" "$handoff_rc"
check_contains "observed model mismatch names probe stage" "capability_probe" "$output"
check_file_exists "observed model mismatch keeps parent manifest" "$SBX/.fno/target-state.md"
check_log_absent "observed model mismatch keeps parent claim" "$CALL_LOG" "claim release node:"

# ---------------------------------------------------------------------------
# Scenario 1d: registration and prompt liveness do not replace readiness
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 1d: prompt-ready proof missing ==="
SBX="$(make_sandbox s1d)"
printf '{"name":"%s","short_id":"abc123","session_id":"%s","harness":"claude","status":"live","bound":true,"readiness":"live","model":"opus"}\n' \
  "$(expected_child_name)" "$TEST_CHILD_SESSION" > "$SBX/scenario/fno-ask-out"

CALL_LOG="$SBX/call-log"
run_handoff "$SBX" "capability"

check_exit "readiness without positive marker parks" "10" "$handoff_rc"
check_contains "readiness mismatch names capability stage" "capability_probe" "$output"
check_file_exists "readiness mismatch keeps parent manifest" "$SBX/.fno/target-state.md"
check_log_absent "readiness mismatch keeps parent claim" "$CALL_LOG" "claim release node:"

# ---------------------------------------------------------------------------
# Scenario 1e: delivered seed without child claim/manifest proof rolls back
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 1e: target seed is delivered but not executed ==="
SBX="$(make_sandbox s1e)"
rm -f "$SBX/scenario/activate-child"

CALL_LOG="$SBX/call-log"
HANDOFF_VERIFY_TIMEOUT=1 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "capability"

check_exit "missing child execution proof parks" "10" "$handoff_rc"
check_contains "missing child execution proof names target stage" "target_execution" "$output"
check_file_exists "missing child execution proof restores parent manifest" "$SBX/.fno/target-state.md"
check_contains "missing child execution proof stops child" "agents stop" "$(cat "$CALL_LOG")"

# ---------------------------------------------------------------------------
# Scenario 1f: child claim with a foreign manifest session fails closed
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 1f: child manifest identity mismatch ==="
SBX="$(make_sandbox s1f)"
touch "$SBX/scenario/wrong-child-manifest"

CALL_LOG="$SBX/call-log"
HANDOFF_VERIFY_TIMEOUT=1 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "capability"

check_exit "child manifest mismatch parks" "10" "$handoff_rc"
check_contains "child manifest mismatch names target stage" "target_execution" "$output"
check_file_exists "child manifest mismatch restores parent manifest" "$SBX/.fno/target-state.md"
check_contains "child manifest mismatch stops child" "agents stop" "$(cat "$CALL_LOG")"

# ---------------------------------------------------------------------------
# Scenario 1g: raw target delivery failure restores parent ownership
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 1g: target seed delivery fails ==="
SBX="$(make_sandbox s1g)"
echo "1" > "$SBX/scenario/fno-mail-rc"

CALL_LOG="$SBX/call-log"
run_handoff "$SBX" "capability"

check_exit "target seed delivery failure parks" "10" "$handoff_rc"
check_contains "target seed delivery failure names target stage" "target_seed" "$output"
check_file_exists "target seed delivery failure restores parent manifest" "$SBX/.fno/target-state.md"
check_contains "target seed delivery failure stops child" "agents stop" "$(cat "$CALL_LOG")"

# ---------------------------------------------------------------------------
# Scenario 2: AC1-ERR - spawn failure (ask returns rc=1)
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 2: AC1-ERR spawn failure ==="
SBX="$(make_sandbox s2)"
echo "1" > "$SBX/scenario/fno-ask-rc"
echo "" > "$SBX/scenario/fno-ask-out"

CALL_LOG="$SBX/call-log"
HANDOFF_VERIFY_TIMEOUT=10 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "blueprint-do"

check_exit "AC1-ERR: exits 10 (parked)" "10" "$handoff_rc"
check_contains "AC1-ERR: output contains 'parked'" "parked" "$output"

# Capability failure occurs before any parent ownership mutation.
check_log_absent "AC1-ERR: spawn failure does not release parent claim" \
  "$CALL_LOG" "claim release node:"

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


# ---------------------------------------------------------------------------
# Scenario 3: verify timeout (ask ok, list never shows live agent)
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 3: verify timeout ==="
SBX="$(make_sandbox s3)"
rm -f "$SBX/scenario/activate-child"

CALL_LOG="$SBX/call-log"
HANDOFF_VERIFY_TIMEOUT=3 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "blueprint-do"

check_exit "verify-timeout: exits 10 (parked)" "10" "$handoff_rc"
check_contains "verify-timeout: output names target execution" "target_execution" "$output"

# Manifest must be restored
check_file_exists "verify-timeout: target-state.md restored" \
  "$SBX/.fno/target-state.md"

# handoff_failed event
set +e
failed_events=$(grep '"type":"handoff_failed"' "$SBX/.fno/events.jsonl" 2>/dev/null | wc -l | tr -d ' ')
set -e
check_eq "verify-timeout: handoff_failed event emitted" "1" "$failed_events"

# Re-acquire claim happens in log
check_log_order "verify-timeout: re-acquire claim AFTER target seed" \
  "$CALL_LOG" "agents mail" "claim acquire node:"

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
# Scenario 6: the explicit one-rung escalation is already spent
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 6: capability escalation already spent ==="
SBX="$(make_sandbox s6)"
printf '{"ts":"2026-06-05T12:00:00Z","type":"delegated","source":"target","data":{"node_id":"%s","from_session":"sess","to_session":"child","boundary":"capability","generation":2,"harness":"claude","model":"opus","handoff_kind":"capability_escalation"}}\n' \
  "$NODE_ID" >> "$SBX/.fno/events.jsonl"

CALL_LOG="$SBX/call-log"
run_handoff "$SBX" "blueprint-do"

check_exit "gen-cap: exits 10 (parked)" "10" "$handoff_rc"
check_contains "gen-cap: output contains 'parked'" "parked" "$output"
check_contains "gen-cap: reason mentions chain-exhausted" "chain-exhausted" "$output"
check_log_absent "gen-cap: no claim acquire" "$CALL_LOG" "claim acquire"

# ---------------------------------------------------------------------------
# Context fixtures remain only as controls proving explicit escalation ignores
# context percentage and transcript availability.
# ---------------------------------------------------------------------------

# arm_probe <sandbox> [transcript-json-line]
# Gives the manifest a transcript id and, when a line is supplied, writes the
# transcript the real probe will read. Omit the line to leave it absent, which
# is what makes the probe exit 3.
arm_probe() {
  local sbx="$1" line="${2:-}"
  printf 'claude_session_id: probe-sid\n' >> "$sbx/.fno/target-state.md"
  if [ -n "$line" ]; then
    local enc="${sbx//[\/.]/-}"
    mkdir -p "$sbx/.claude/projects/$enc"
    printf '%s\n' "$line" > "$sbx/.claude/projects/$enc/probe-sid.jsonl"
  fi
}

# A transcript line the probe understands: usage sums to used_tokens. The window
# comes from the model family - claude-sonnet-4-6 is a 1M-context model, so the
# token counts below are percentages of 1,000,000, not of 200,000.
probe_line() {  # probe_line <input_tokens>
  printf '{"type":"assistant","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":%s,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}' "$1"
}

# ---------------------------------------------------------------------------
# Scenario 7: context readings do not decide explicit capability escalation
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 7: low context does not park explicit escalation ==="
SBX="$(make_sandbox s7)"
arm_probe "$SBX" "$(probe_line 300000)"

CALL_LOG="$SBX/call-log"
HANDOFF_TEST_HOME="$SBX" run_handoff "$SBX" "capability"

check_exit "explicit escalation ignores context percentage" "0" "$handoff_rc"
check_contains "explicit escalation reaches delegated transaction" "delegated" "$output"
check_contains "explicit escalation launches the selected child" "agents spawn" "$(cat "$CALL_LOG")"

# ---------------------------------------------------------------------------
# Scenario 8: no transcript remains a valid explicit escalation input
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 8: transcript is not an escalation prerequisite ==="
SBX="$(make_sandbox s8)"

CALL_LOG="$SBX/call-log"
HANDOFF_TEST_HOME="$SBX" run_handoff "$SBX" "capability"

check_exit "explicit escalation does not require a context transcript" "0" "$handoff_rc"
check_contains "transcript-independent escalation delegates" "delegated" "$output"

# ---------------------------------------------------------------------------
# Scenario 9: restore_failed
# ask succeeds, list never returns live (timeout), mv restore is blocked.
# We simulate restore failure by making .fno/ a read-only dir after
# archive so mv back cannot write.
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 9: restore_failed ==="
SBX="$(make_sandbox s9)"
rm -f "$SBX/scenario/activate-child"

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
# Scenario 11: C1 - claim-lost on verify-fail unwind (re-acquire fails)
# ask succeeds, list never live (timeout), re-acquire node:X fails -> exit 12
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 11: C1 claim-lost on verify-fail ==="
SBX="$(make_sandbox s11)"
rm -f "$SBX/scenario/activate-child"
# node: acquire fails during re-acquire
echo "1" > "$SBX/scenario/fno-claim-acquire-node-rc"

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
# PAST Step 0 (proven by reaching the explicit delegation transaction, which is
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
check_contains "stray-fence: reached explicit delegation" \
  "delegated" "$output"

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
check_contains "unterminated-fm: reached explicit delegation" \
  "delegated" "$output"

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
check_contains "leading-ws: reached explicit delegation" \
  "delegated" "$output"

# ---------------------------------------------------------------------------
# Scenario 13: AC3-HP - CRLF line endings; id read CR-free
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 13: CRLF manifest, clean id (AC3-HP) ==="
SBX="$(make_sandbox s13)"
# Build a fully-CRLF manifest so both frontmatter (session_id) and body
# (graph_node_id) carry trailing \r. A CR-poisoned id would park at the
# holder-mismatch guard (and a CR-poisoned session_id would too); the robust
# readers strip CR so the run reaches the explicit delegation instead.
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
check_contains "crlf: reached explicit delegation" \
  "delegated" "$output"
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
# Scenario 6b: x-3571 - the gate delegates to `fno do plan rung`
#
# The gate no longer parses `^status:` itself. Two things follow that the
# vocabulary scenarios above cannot show: a pre-design rung parks (it used to
# derive `ready` and dispatch), and a stale `fno` that lacks the verb parks with
# a reason naming the binary rather than blaming the plan.
# ---------------------------------------------------------------------------
echo ""
echo "=== Scenario 6b: x-3571 rung delegation ==="

for st in idea stub; do
  SBX="$(make_sandbox "s6b-$st")"
  sed -i.bak "s/^status: ready$/status: $st/" "$SBX/$PLAN_REL"
  rm -f "$SBX/$PLAN_REL.bak"
  CALL_LOG="$SBX/call-log"
  HANDOFF_VERIFY_TIMEOUT=10 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "blueprint-do"
  check_contains "x-3571: pre-design rung '$st' parks" "parked" "$output"
  check_contains "x-3571: '$st' parks naming the rung" "rung 'idea'" "$output"
  check_contains "x-3571: '$st' uses canonical do plan rung" "fno do plan rung" "$(cat "$CALL_LOG")"
done

# A stale fno (no `rung` verb) must park loudly, not pass silently and not
# blame the plan. Overwrite the stub so `do plan rung` fails the way an older
# installed binary does: exit 2, nothing on stdout.
SBX="$(make_sandbox s6b-stale)"
CALL_LOG="$SBX/call-log"
cat > "$SBX/stub-bin/fno" <<'STALEEOF'
#!/usr/bin/env bash
echo "fno $*" >> "${CALL_LOG:-/dev/null}"
if [ "${1:-} ${2:-} ${3:-}" = "do plan rung" ]; then
  echo "No such command 'rung'." >&2
  exit 2
fi
if [ "${1:-} ${2:-}" = "plan rung" ]; then
  echo "deprecated plan root reached" >&2
  exit 2
fi
exit 0
STALEEOF
chmod +x "$SBX/stub-bin/fno"
HANDOFF_VERIFY_TIMEOUT=10 HANDOFF_VERIFY_INTERVAL=1 run_handoff "$SBX" "blueprint-do"
check_contains "x-3571: a stale fno parks" "parked" "$output"
check_contains "x-3571: the park reason names the binary, not the plan" "predate" "$output"

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
