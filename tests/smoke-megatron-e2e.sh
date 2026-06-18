#!/usr/bin/env bash
# Smoke test for fno-agents loop run --driver megatron (task 3.1, ab-9fd662c6).
#
# This IS the megatron end-to-end smoke that backlog node ab-cf5a4233 asked
# for: full mission flow across two projects, the partial-failure pause path,
# and restart idempotency - rebased onto the unified Rust loop.
#
# The recursion is REAL: the debug fno-agents binary dispatches each project
# by re-invoking ITSELF with --driver megawalk --cwd <project> --mission <id>
# --termination-key <key>. Only `fno` (mission verbs + backlog + claims) and
# the driver lib are stubs.
#
# Scenarios:
#   1. happy_mission   - 2 projects via stub megatron next; each child walk
#                        drains 1 node (DonePRGreen); both complete --outcome
#                        done; commander exits 0 (NoWork) with 2 closed units;
#                        child termination events keyed mt* land in the
#                        (hermetic) global journal.
#   2. partial_failure - child walk for proj-a burns its budget without a
#                        node termination -> walk termination Budget ->
#                        complete --outcome failed -> stub next returns pause
#                        -> walk_paused journaled, commander exits 4.
#   3. already_complete- megatron next returns null immediately; commander
#                        exits 0 with 0 units (restart idempotency).
#   4. commander_held  - fleet claim acquire exits 1 -> commander exits 3
#                        (CommanderAlreadyRunning contract preserved).
#
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

PASS=0
FAIL=0

pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

run_capturing() {
  local __rc_out_var="$1" __rc_err_var="$2" __rc_ec_var="$3"
  shift 3
  local __rc_token="${RANDOM}${RANDOM}"
  local __rc_outfile="$TMP_DIR/rc_out_${__rc_token}"
  local __rc_errfile="$TMP_DIR/rc_err_${__rc_token}"
  local __rc_code=0
  ( "$@" >"$__rc_outfile" 2>"$__rc_errfile" ) || __rc_code=$?
  printf -v "$__rc_out_var" '%s' "$(cat "$__rc_outfile" 2>/dev/null || true)"
  printf -v "$__rc_err_var" '%s' "$(cat "$__rc_errfile" 2>/dev/null || true)"
  printf -v "$__rc_ec_var" '%s' "$__rc_code"
  rm -f "$__rc_outfile" "$__rc_errfile"
}

# Locate the real debug binary.
REAL_BIN="$ROOT_DIR/crates/fno-agents/target/debug/fno-agents"
if [[ ! -x "$REAL_BIN" ]]; then
  if command -v cargo &>/dev/null; then
    echo "  building debug binary (one-time)..."
    (cd "$ROOT_DIR/crates/fno-agents" && cargo build 2>/dev/null) || true
  fi
fi

if [[ ! -x "$REAL_BIN" ]]; then
  echo "SKIP: smoke-megatron-e2e.sh (debug binary absent and cargo not available)"
  echo "Build with: cd crates/fno-agents && cargo build"
  exit 0
fi

echo "=== smoke-megatron-e2e tests ==="
echo ""

# ---------------------------------------------------------------------------
# Helper: hermetic child project (the dirs megatron next points at).
#
# The Rust megawalk preflight needs scripts/lib/driver-claude-code.sh defining
# driver_invoke + the `claude` binary on PATH. The driver stub emits a
# DonePRGreen termination keyed by TARGET_SESSION_ID unless NO_TERMINATION=1,
# in which case it emits nothing (the budget-burn path for scenario 2).
# ---------------------------------------------------------------------------
mk_project() {
  local dir="$1"
  mkdir -p "$dir/.fno"
}

mk_driver_lib() {
  local dir="$1"
  mkdir -p "$dir"
  cat > "$dir/driver-claude-code.sh" <<'DRIVER_STUB'
#!/usr/bin/env bash
driver_default_max() { echo 4; }
driver_invoke() {
  if [[ "${NO_TERMINATION:-0}" == "1" ]]; then
    return 0
  fi
  local SESSION_ID="${TARGET_SESSION_ID:-stub-session}"
  local EVENTS_FILE="${FNO_CWD:-.}/.fno/events.jsonl"
  mkdir -p "$(dirname "$EVENTS_FILE")"
  local TS
  TS=$(date -u +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo "2026-01-01T00:00:00Z")
  printf '{"ts":"%s","type":"termination","source":"hook","data":{"session_id":"%s","reason":"DonePRGreen","message":"stub done"}}\n' \
    "$TS" "$SESSION_ID" >> "$EVENTS_FILE"
  return 0
}
driver_persist_history() { return 0; }
driver_check_promise() { return 0; }
DRIVER_STUB
  chmod +x "$dir/driver-claude-code.sh"
}

# ---------------------------------------------------------------------------
# Case 1: happy_mission
# ---------------------------------------------------------------------------
echo "Case 1: happy_mission"
{
  CASE_DIR="$TMP_DIR/case1"
  HOME_DIR="$CASE_DIR/home"           # hermetic HOME for the global journal
  CMD_DIR="$CASE_DIR/commander"       # commander cwd (its project journal)
  PROJ_A="$CASE_DIR/proj-a"
  PROJ_B="$CASE_DIR/proj-b"
  BIN_DIR="$CASE_DIR/bin"
  LIB_DIR="$CASE_DIR/lib"
  mkdir -p "$HOME_DIR/.fno" "$CMD_DIR/.fno" "$BIN_DIR"
  mk_project "$PROJ_A"; mk_project "$PROJ_B"
  mk_driver_lib "$LIB_DIR"

  NEXT_COUNT="$CASE_DIR/next_count"
  printf '0' > "$NEXT_COUNT"
  COMPLETE_LOG="$CASE_DIR/complete_calls.log"
  touch "$COMPLETE_LOG"
  CLAIM_LOG="$CASE_DIR/claim_calls.log"
  touch "$CLAIM_LOG"
  # Per-project backlog-next counters (1 node each, then null).
  printf '0' > "$CASE_DIR/bl_a"; printf '0' > "$CASE_DIR/bl_b"

  printf '#!/bin/sh\nexit 0\n' > "$BIN_DIR/claude"; chmod +x "$BIN_DIR/claude"

  cat > "$BIN_DIR/fno" <<STUBEOF
#!/usr/bin/env bash
if [[ "\$1" == "claim" ]]; then echo "\$@" >> "$CLAIM_LOG"; exit 0; fi
if [[ "\$1" == "megatron" && "\$2" == "next" ]]; then
  n=\$(cat "$NEXT_COUNT"); n=\$((n + 1)); echo "\$n" > "$NEXT_COUNT"
  if [[ "\$n" -eq 1 ]]; then
    echo '{"project": "proj-a", "wave": 1, "project_path": "$PROJ_A", "node_id": "ab-aaaa0001", "title": "wave 1 - proj-a", "mission_id": "ab-smoke001", "slug": "smoke"}'
  elif [[ "\$n" -eq 2 ]]; then
    echo '{"project": "proj-b", "wave": 1, "project_path": "$PROJ_B", "node_id": "ab-bbbb0001", "title": "wave 1 - proj-b", "mission_id": "ab-smoke001", "slug": "smoke"}'
  else
    echo 'null'
  fi
  exit 0
fi
if [[ "\$1" == "megatron" && "\$2" == "complete" ]]; then
  echo "\$@" >> "$COMPLETE_LOG"
  echo '{"result": "recorded"}'
  exit 0
fi
if [[ "\$1" == "backlog" && "\$2" == "next" ]]; then
  # Project-scoped 1-node backlog keyed off the cwd basename.
  base="\$(basename "\$PWD")"
  cf="$CASE_DIR/bl_a"; node_id="ab-node000a"
  [[ "\$base" == "proj-b" ]] && cf="$CASE_DIR/bl_b" && node_id="ab-node000b"
  c=\$(cat "\$cf"); c=\$((c + 1)); echo "\$c" > "\$cf"
  if [[ "\$c" -eq 1 ]]; then
    printf '{"id": "%s", "title": "Mission node", "priority": "p1", "domain": null, "project": "%s", "cwd": "%s", "size": null, "plan_path": null, "mission_id": null}\n' "\$node_id" "\$base" "\$PWD"
  else
    echo 'null'
  fi
  exit 0
fi
if [[ "\$1" == "backlog" && "\$2" == "done" ]]; then exit 0; fi
if [[ "\$1" == "claim" ]]; then exit 0; fi
exit 0
STUBEOF
  chmod +x "$BIN_DIR/fno"

  run_capturing OUT ERR EC env \
    HOME="$HOME_DIR" \
    PATH="$BIN_DIR:/bin:/usr/bin:/usr/local/bin" \
    FNO_BIN="$BIN_DIR/fno" \
    "$REAL_BIN" loop run --driver megatron --mission ab-smoke001 \
      --driver-lib-dir "$LIB_DIR" --cwd "$CMD_DIR" --max-iterations 10

  [[ "$EC" == "0" ]] && pass "commander exits 0" || fail "expected exit 0, got $EC (stderr: $ERR)"
  echo "$OUT" | grep -q "NoWork" && pass "walk terminates NoWork" || fail "NoWork missing in output: $OUT"
  echo "$OUT" | grep -q "2 project walks closed" && pass "2 project walks closed" || fail "unit count wrong: $OUT"

  done_count=$(grep -c -- "--outcome done" "$COMPLETE_LOG" || true)
  [[ "$done_count" == "2" ]] && pass "complete --outcome done called twice" || fail "expected 2 done completes, got $done_count: $(cat "$COMPLETE_LOG")"
  grep -q -- "--project proj-a" "$COMPLETE_LOG" && grep -q -- "--project proj-b" "$COMPLETE_LOG" \
    && pass "both projects recorded" || fail "missing project in completes: $(cat "$COMPLETE_LOG")"

  CMD_JOURNAL="$CMD_DIR/.fno/events.jsonl"
  closed_count=$(grep -c '"node_closed"' "$CMD_JOURNAL" || true)
  [[ "$closed_count" == "2" ]] && pass "commander journal has 2 node_closed" || fail "node_closed count: $closed_count"
  grep -q '"loop_terminated"' "$CMD_JOURNAL" && pass "loop_terminated journaled" || fail "no loop_terminated in commander journal"

  # Child walk termination events (keyed -mt) reach the hermetic global mirror.
  GLOBAL_JOURNAL="$HOME_DIR/.fno/events.jsonl"
  mt_terms=$(grep '"termination"' "$GLOBAL_JOURNAL" | grep -c -- '-mt' || true)
  [[ "$mt_terms" -ge 2 ]] && pass "child walk terminations in global journal ($mt_terms)" \
    || fail "expected >=2 mt-keyed terminations in global journal, got $mt_terms"

  # Fleet singleton claim is released on the normal exit path.
  grep "release fleet:ab-smoke001" "$CLAIM_LOG" >/dev/null \
    && pass "fleet claim released on success" \
    || fail "no fleet claim release logged: $(cat "$CLAIM_LOG")"
}
echo ""

# ---------------------------------------------------------------------------
# Case 2: partial_failure (the wave partial-failure pause path)
# ---------------------------------------------------------------------------
echo "Case 2: partial_failure"
{
  CASE_DIR="$TMP_DIR/case2"
  HOME_DIR="$CASE_DIR/home"
  CMD_DIR="$CASE_DIR/commander"
  PROJ_A="$CASE_DIR/proj-a"
  BIN_DIR="$CASE_DIR/bin"
  LIB_DIR="$CASE_DIR/lib"
  mkdir -p "$HOME_DIR/.fno" "$CMD_DIR/.fno" "$BIN_DIR"
  mk_project "$PROJ_A"
  mk_driver_lib "$LIB_DIR"

  NEXT_COUNT="$CASE_DIR/next_count"
  printf '0' > "$NEXT_COUNT"
  COMPLETE_LOG="$CASE_DIR/complete_calls.log"
  touch "$COMPLETE_LOG"
  CLAIM_LOG="$CASE_DIR/claim_calls.log"
  touch "$CLAIM_LOG"
  printf '0' > "$CASE_DIR/bl_a"

  printf '#!/bin/sh\nexit 0\n' > "$BIN_DIR/claude"; chmod +x "$BIN_DIR/claude"

  cat > "$BIN_DIR/fno" <<STUBEOF
#!/usr/bin/env bash
if [[ "\$1" == "claim" ]]; then echo "\$@" >> "$CLAIM_LOG"; exit 0; fi
if [[ "\$1" == "megatron" && "\$2" == "next" ]]; then
  n=\$(cat "$NEXT_COUNT"); n=\$((n + 1)); echo "\$n" > "$NEXT_COUNT"
  if [[ "\$n" -eq 1 ]]; then
    echo '{"project": "proj-a", "wave": 1, "project_path": "$PROJ_A", "node_id": "ab-aaaa0001", "title": "wave 1 - proj-a", "mission_id": "ab-smoke002", "slug": "smoke"}'
  else
    echo '{"pause": {"policy": "mission_paused", "detail": "project_failed: wave 1 project proj-a: Budget"}}'
  fi
  exit 0
fi
if [[ "\$1" == "megatron" && "\$2" == "complete" ]]; then
  echo "\$@" >> "$COMPLETE_LOG"
  echo '{"result": "paused"}'
  exit 0
fi
if [[ "\$1" == "backlog" && "\$2" == "next" ]]; then
  # Always one ready node: the walk re-dispatches until its budget burns.
  printf '{"id": "ab-stuck001", "title": "Never terminates", "priority": "p1", "domain": null, "project": "proj-a", "cwd": "%s", "size": null, "plan_path": null, "mission_id": null}\n' "\$PWD"
  exit 0
fi
if [[ "\$1" == "backlog" && "\$2" == "done" ]]; then exit 0; fi
if [[ "\$1" == "claim" ]]; then exit 0; fi
exit 0
STUBEOF
  chmod +x "$BIN_DIR/fno"

  run_capturing OUT ERR EC env \
    HOME="$HOME_DIR" \
    PATH="$BIN_DIR:/bin:/usr/bin:/usr/local/bin" \
    FNO_BIN="$BIN_DIR/fno" \
    NO_TERMINATION=1 \
    "$REAL_BIN" loop run --driver megatron --mission ab-smoke002 \
      --driver-lib-dir "$LIB_DIR" --cwd "$CMD_DIR" --max-iterations 10

  [[ "$EC" == "4" ]] && pass "commander exits 4 (paused)" || fail "expected exit 4, got $EC (stderr: $ERR)"
  grep -q -- "--outcome failed" "$COMPLETE_LOG" && pass "complete --outcome failed recorded" \
    || fail "no failed complete: $(cat "$COMPLETE_LOG")"
  CMD_JOURNAL="$CMD_DIR/.fno/events.jsonl"
  grep -q '"walk_paused"' "$CMD_JOURNAL" && pass "walk_paused journaled" || fail "no walk_paused event"
  grep -q '"NoProgress"' "$CMD_JOURNAL" && pass "NoProgress termination journaled" || fail "no NoProgress in journal"

  # Fleet singleton claim is released on the paused exit path too.
  grep "release fleet:ab-smoke002" "$CLAIM_LOG" >/dev/null \
    && pass "fleet claim released on pause" \
    || fail "no fleet claim release logged: $(cat "$CLAIM_LOG")"
}
echo ""

# ---------------------------------------------------------------------------
# Case 3: already_complete (restart idempotency)
# ---------------------------------------------------------------------------
echo "Case 3: already_complete"
{
  CASE_DIR="$TMP_DIR/case3"
  HOME_DIR="$CASE_DIR/home"
  CMD_DIR="$CASE_DIR/commander"
  BIN_DIR="$CASE_DIR/bin"
  LIB_DIR="$CASE_DIR/lib"
  mkdir -p "$HOME_DIR/.fno" "$CMD_DIR/.fno" "$BIN_DIR"
  mk_driver_lib "$LIB_DIR"

  printf '#!/bin/sh\nexit 0\n' > "$BIN_DIR/claude"; chmod +x "$BIN_DIR/claude"
  cat > "$BIN_DIR/fno" <<'STUBEOF'
#!/usr/bin/env bash
if [[ "$1" == "megatron" && "$2" == "next" ]]; then echo 'null'; exit 0; fi
if [[ "$1" == "claim" ]]; then exit 0; fi
exit 0
STUBEOF
  chmod +x "$BIN_DIR/fno"

  run_capturing OUT ERR EC env \
    HOME="$HOME_DIR" \
    PATH="$BIN_DIR:/bin:/usr/bin:/usr/local/bin" \
    FNO_BIN="$BIN_DIR/fno" \
    "$REAL_BIN" loop run --driver megatron --mission ab-smoke003 \
      --driver-lib-dir "$LIB_DIR" --cwd "$CMD_DIR"

  [[ "$EC" == "0" ]] && pass "re-run on complete mission exits 0" || fail "expected exit 0, got $EC (stderr: $ERR)"
  echo "$OUT" | grep -q "0 project walks closed" && pass "no walks dispatched" || fail "unexpected dispatches: $OUT"
}
echo ""

# ---------------------------------------------------------------------------
# Case 4: commander_held (fleet singleton claim)
# ---------------------------------------------------------------------------
echo "Case 4: commander_held"
{
  CASE_DIR="$TMP_DIR/case4"
  HOME_DIR="$CASE_DIR/home"
  CMD_DIR="$CASE_DIR/commander"
  BIN_DIR="$CASE_DIR/bin"
  LIB_DIR="$CASE_DIR/lib"
  mkdir -p "$HOME_DIR/.fno" "$CMD_DIR/.fno" "$BIN_DIR"
  mk_driver_lib "$LIB_DIR"

  printf '#!/bin/sh\nexit 0\n' > "$BIN_DIR/claude"; chmod +x "$BIN_DIR/claude"
  cat > "$BIN_DIR/fno" <<'STUBEOF'
#!/usr/bin/env bash
if [[ "$1" == "claim" && "$2" == "acquire" && "$3" == fleet:* ]]; then
  echo "held by megatron-loop:99999" >&2
  exit 1
fi
exit 0
STUBEOF
  chmod +x "$BIN_DIR/fno"

  run_capturing OUT ERR EC env \
    HOME="$HOME_DIR" \
    PATH="$BIN_DIR:/bin:/usr/bin:/usr/local/bin" \
    FNO_BIN="$BIN_DIR/fno" \
    "$REAL_BIN" loop run --driver megatron --mission ab-smoke004 \
      --driver-lib-dir "$LIB_DIR" --cwd "$CMD_DIR"

  [[ "$EC" == "3" ]] && pass "held fleet claim exits 3" || fail "expected exit 3, got $EC (stderr: $ERR)"
  echo "$ERR" | grep -q "another commander" && pass "error names the running commander" || fail "missing commander message: $ERR"
}
echo ""

# ---------------------------------------------------------------------------
echo "=== Results: $PASS passed, $FAIL failed ==="
[[ "$FAIL" -eq 0 ]] || exit 1
