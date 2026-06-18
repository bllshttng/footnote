#!/usr/bin/env bash
# Wave 0 smoke prototype child (Phase 6, ab-a09e1eaf).
#
# Stands in for a long-running interactive CLI (codex/gemini). Appends a
# heartbeat line every second to $LOG so the harness can tell, after the
# supervisor is SIGKILLed, whether this process is still alive AND still
# doing work.
#
# Args:
#   $1  log path (required)
#   $2  sighup mode: "default" (let SIGHUP kill us) or "ignore" (trap '')
#
# A trap logs the moment SIGHUP arrives so the findings memo can attribute
# the cause of death precisely rather than guessing.

set -u
LOG="${1:?usage: heartbeat-child.sh <log> <default|ignore>}"
MODE="${2:-default}"

echo "child_start pid=$$ ppid=$PPID mode=$MODE ts=$(date +%s.%N)" >> "$LOG"

if [[ "$MODE" == "ignore" ]]; then
  # Survive the hang-up: this is the mitigation a worker-shim would apply.
  trap 'echo "child_sighup_ignored pid=$$ ts=$(date +%s.%N)" >> "'"$LOG"'"' SIGHUP
else
  # Log the hang-up, then perform the default action (terminate) by clearing
  # the trap and re-raising. This proves SIGHUP is the cause of death.
  trap 'echo "child_sighup_received pid=$$ ts=$(date +%s.%N)" >> "'"$LOG"'"; trap - SIGHUP; kill -SIGHUP $$' SIGHUP
fi

while true; do
  echo "heartbeat pid=$$ ts=$(date +%s.%N)" >> "$LOG"
  sleep 1
done
