#!/usr/bin/env bash
# Driver: opencode (https://opencode.ai, sst/opencode) - loop-wrapper path.
# Contract: driver_invoke, driver_check_promise, driver_persist_history,
# driver_default_max. Sourced by scripts/run-target-loop.sh.

# Required env (set by the wrapper):
#   OUTPUT_FILE     path where driver stdout+stderr is captured
#   HISTORY_FILE    iteration marker between turns (its existence => not iter 1)
#   SIGNAL_FILE     path to .fno/target-promise.signal
#   MAX_TURNS       per-session turn cap (UNUSED: `opencode run` has no turn-cap
#                   flag; the loop iteration cap + budget bound the run instead)
#   MODEL_FLAG      optional "--model provider/model" string
#   CONTINUE_PROMPT the slash command to resume (/target --resume etc.)
#   PROMPT_FILE     initial prompt file (if set, read first iteration from it)

driver_default_max() {
  echo 20
}

driver_invoke() {
  local cli="${OPENCODE_CLI:-opencode}"
  if ! command -v "$cli" &>/dev/null; then
    return 77
  fi

  local prompt
  if [[ -n "${PROMPT_FILE:-}" && -f "${PROMPT_FILE}" && ! -s "${HISTORY_FILE:-/dev/null}" ]]; then
    prompt="$(cat "${PROMPT_FILE}")"
  else
    prompt="${CONTINUE_PROMPT:-/target --resume}"
  fi

  # opencode is headless via `opencode run [message..]`. The prompt is
  # POSITIONAL - NOT `-p` (that is opencode's basic-auth password flag).
  # Cross-iteration context uses opencode's native `--continue` (resume last
  # session) rather than a history file. MODEL_FLAG is "--model provider/model".
  # Build as a single argv to avoid bash 3.2's set -u empty-array bug (macOS).
  if [[ -s "${HISTORY_FILE:-/dev/null}" ]]; then
    # shellcheck disable=SC2086
    "$cli" run --continue ${MODEL_FLAG:-} "$prompt" > "${OUTPUT_FILE}" 2>&1
  else
    # shellcheck disable=SC2086
    "$cli" run ${MODEL_FLAG:-} "$prompt" > "${OUTPUT_FILE}" 2>&1
  fi
}

driver_check_promise() {
  if [[ -s "${SIGNAL_FILE}" ]] && grep -q 'MISSION COMPLETE' "${SIGNAL_FILE}" 2>/dev/null; then
    return 0
  fi
  if [[ -s "${OUTPUT_FILE}" ]] && grep -qE '<promise>[^<]*MISSION COMPLETE' "${OUTPUT_FILE}" 2>/dev/null; then
    return 0
  fi
  return 1
}

driver_persist_history() {
  # opencode resumes context itself via --continue, but the wrapper still keys
  # "first vs subsequent iteration" on HISTORY_FILE being non-empty, so we must
  # write to it. Strip ANSI CSI sequences and spinner carriage returns.
  local iter="${CURRENT_ITER:-?}"
  local esc=$'\033'
  {
    echo ""
    echo "### iteration ${iter}"
    echo ""
    sed -E "s/${esc}\[[0-9;?]*[a-zA-Z]//g; s/\r\$//" "${OUTPUT_FILE}"
  } >> "${HISTORY_FILE}"
}
