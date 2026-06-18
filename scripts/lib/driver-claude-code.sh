#!/usr/bin/env bash
# Driver: Claude Code
# Contract: driver_invoke, driver_check_promise, driver_persist_history,
# driver_default_max. Sourced by scripts/run-target-loop.sh.

# Required env (set by the wrapper):
#   OUTPUT_FILE     path where driver stdout+stderr is captured
#   HISTORY_FILE    path to the running conversation history
#   SIGNAL_FILE     path to .fno/target-promise.signal
#   MAX_TURNS       per-session turn cap
#   BUDGET_USD      per-session dollar cap
#   MODEL_FLAG      optional "--model NAME" string
#   CONTINUE_PROMPT the slash command to resume (/target --resume etc.)
#   CLI             optional alias name from --cli flag ("claude" | "opencode");
#                   used to pick the binary when $CLAUDE_CLI is not set.

driver_default_max() {
  echo 40
}

driver_invoke() {
  # Claude Code invocation. Honors Claude Code's own session flags.
  # The Stop hook inside Claude Code also keeps the session alive, so this
  # wrapper is mostly belt-and-braces for compaction boundaries.
  #
  # Binary resolution order:
  #   1. $CLAUDE_CLI env var (explicit override)
  #   2. --cli flag value ("claude" or "opencode") via $CLI
  #   3. "claude" as the default
  local cli="${CLAUDE_CLI:-${CLI:-claude}}"
  if ! command -v "$cli" &>/dev/null; then
    return 77
  fi
  # shellcheck disable=SC2086
  "$cli" --print \
    --max-turns "${MAX_TURNS:-15}" \
    --max-budget-usd "${BUDGET_USD:-25}" \
    --dangerously-skip-permissions \
    ${MODEL_FLAG:-} \
    "${CONTINUE_PROMPT:-/target --resume}" > "${OUTPUT_FILE}" 2>&1
}

driver_check_promise() {
  # Prefer sentinel; fall back to stdout scan.
  if [[ -s "${SIGNAL_FILE}" ]] && grep -q 'MISSION COMPLETE' "${SIGNAL_FILE}" 2>/dev/null; then
    return 0
  fi
  if [[ -s "${OUTPUT_FILE}" ]] && grep -qE '<promise>[^<]*MISSION COMPLETE' "${OUTPUT_FILE}" 2>/dev/null; then
    return 0
  fi
  return 1
}

driver_persist_history() {
  # Claude Code sessions carry their own transcript; no explicit history
  # hand-off needed between iterations. Provide a no-op so the wrapper's
  # contract is satisfied uniformly.
  :
}
