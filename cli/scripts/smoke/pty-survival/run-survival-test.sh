#!/usr/bin/env bash
# Wave 0 smoke prototype harness (Phase 6, ab-a09e1eaf).
#
# Builds the Rust supervisor probe, then for each SIGHUP mode:
#   1. start the supervisor (it spawns the heartbeat child on a PTY)
#   2. read SUPERVISOR_PID / CHILD_PID from its stdout
#   3. let the child beat a few times
#   4. SIGKILL the supervisor (worst case: no graceful close)
#   5. wait 5s, then check: is the child still alive? did the heartbeat log
#      keep growing? did SIGHUP arrive?
#   6. report the per-mode verdict and reap any survivor
#
# Exit 0 always (this is a probe, not a gate); the verdict is in stdout and
# the findings memo. Run from anywhere; paths are resolved relative to this
# script.

set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHILD="$HERE/heartbeat-child.sh"
TARGET_DIR="${CARGO_TARGET_DIR:-${TMPDIR:-/tmp}/pty-survival-target}"
BIN="$TARGET_DIR/release/pty-survival-probe"

echo "== building supervisor probe (target=$TARGET_DIR) =="
( cd "$HERE" && CARGO_TARGET_DIR="$TARGET_DIR" cargo build --release ) || {
  echo "BUILD FAILED" >&2
  exit 0
}

run_mode() {
  local mode="$1"
  local log
  log="$(mktemp "${TMPDIR:-/tmp}/pty-survival-${mode}.XXXXXX.log")"

  echo ""
  echo "== mode=$mode (log=$log) =="
  # Start supervisor; capture its stdout to a temp file we can poll.
  local sup_out
  sup_out="$(mktemp)"
  # Supervisor forwards argv[1..] to bash: `bash <script> <log> <mode>`.
  "$BIN" "$CHILD" "$log" "$mode" >"$sup_out" 2>&1 &
  local sup_shell_pid=$!

  # Wait for the supervisor to print its PIDs.
  local sup_pid="" child_pid="" tries=0
  while [[ -z "$child_pid" && $tries -lt 50 ]]; do
    sup_pid="$(sed -n 's/^SUPERVISOR_PID=//p' "$sup_out" | head -1)"
    child_pid="$(sed -n 's/^CHILD_PID=//p' "$sup_out" | head -1)"
    sleep 0.1
    tries=$((tries + 1))
  done

  if [[ -z "$child_pid" || "$child_pid" == "unknown" ]]; then
    echo "  FAILED to obtain child pid; supervisor output:"
    sed 's/^/    /' "$sup_out"
    kill -9 "$sup_shell_pid" 2>/dev/null
    rm -f "$sup_out"
    return
  fi
  echo "  supervisor_pid=$sup_pid child_pid=$child_pid"

  # Let the child beat a few times.
  sleep 3
  local beats_before
  beats_before="$(grep -c '^heartbeat ' "$log" 2>/dev/null || true)"
  echo "  heartbeats before kill: $beats_before"

  # SIGKILL the supervisor (worst case: no chance to close master gracefully).
  echo "  SIGKILL supervisor ($sup_pid)..."
  kill -9 "$sup_pid" 2>/dev/null
  kill -9 "$sup_shell_pid" 2>/dev/null

  # Wait and observe.
  sleep 5
  local child_alive="no"
  if kill -0 "$child_pid" 2>/dev/null; then child_alive="yes"; fi
  local beats_after
  beats_after="$(grep -c '^heartbeat ' "$log" 2>/dev/null || true)"
  local got_sighup="no"
  if grep -q 'child_sighup_received\|child_sighup_ignored' "$log" 2>/dev/null; then got_sighup="yes"; fi
  local grew="no"
  if [[ "$beats_after" -gt "$beats_before" ]]; then grew="yes"; fi

  echo "  +5s: child_alive=$child_alive  heartbeats_after=$beats_after (grew=$grew)  sighup_observed=$got_sighup"
  echo "  VERDICT[$mode]: $(
    if [[ "$child_alive" == "yes" && "$grew" == "yes" ]]; then echo "SURVIVED (child outlived supervisor and kept working)";
    elif [[ "$child_alive" == "yes" ]]; then echo "ALIVE-BUT-IDLE (child process exists but stopped beating)";
    else echo "DIED (child did not survive supervisor SIGKILL)"; fi
  )"
  echo "  --- last 6 log lines ---"
  tail -6 "$log" 2>/dev/null | sed 's/^/    /'

  # Reap any survivor so we do not leak processes.
  kill -9 "$child_pid" 2>/dev/null
  rm -f "$sup_out"
}

run_mode default
run_mode ignore

echo ""
echo "== done. See verdicts above. =="
