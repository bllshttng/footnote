#!/usr/bin/env bash
# Driver: hermes-agent
# Contract: driver_invoke, driver_check_promise, driver_persist_history,
# driver_default_max. Sourced by scripts/run-target-loop.sh.

# Required env (set by the wrapper):
#   OUTPUT_FILE     path where driver stdout+stderr is captured
#   HISTORY_FILE    conversation history between iterations
#   SIGNAL_FILE     path to .fno/target-promise.signal
#   MAX_TURNS       per-session turn cap (maps to hermes max_iterations)
#   MODEL_FLAG      optional "--model NAME" string
#   CONTINUE_PROMPT the slash command to resume (/target --resume etc.)
#   PROMPT_FILE     initial prompt file (if set, read first iteration from it)

driver_default_max() {
  echo 20
}

driver_invoke() {
  local cli="${HERMES_CLI:-hermes-agent}"
  if ! command -v "$cli" &>/dev/null; then
    return 77
  fi

  local prompt
  if [[ -n "${PROMPT_FILE:-}" && -f "${PROMPT_FILE}" && ! -s "${HISTORY_FILE:-/dev/null}" ]]; then
    # First iteration: use prompt file verbatim.
    prompt="$(cat "${PROMPT_FILE}")"
  else
    prompt="${CONTINUE_PROMPT:-/target --resume}"
  fi

  # Hermes accepts a single -p prompt. Conversation history is passed via
  # --conversation-history when present; hermes resolves the file path and
  # re-hydrates the prior turns. If the flag is not supported by the installed
  # version, the wrapper's history file is ignored and the bot starts fresh.
  #
  # Build the invocation as a single argv to avoid bash 3.2's set -u
  # empty-array bug (macOS).
  if [[ -s "${HISTORY_FILE:-/dev/null}" ]]; then
    # shellcheck disable=SC2086
    "$cli" -p "$prompt" \
      --max-iterations "${MAX_TURNS:-15}" \
      ${MODEL_FLAG:-} \
      --conversation-history "${HISTORY_FILE}" > "${OUTPUT_FILE}" 2>&1
  else
    # shellcheck disable=SC2086
    "$cli" -p "$prompt" \
      --max-iterations "${MAX_TURNS:-15}" \
      ${MODEL_FLAG:-} > "${OUTPUT_FILE}" 2>&1
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
  # Append this iteration's output to the rolling history so the next
  # iteration can re-hydrate. Hermes expects a plain-text transcript; we
  # use a minimal format keyed on iteration count.
  #
  # Strip ANSI CSI sequences and spinner carriage returns: HISTORY_FILE is
  # re-read as LLM context on the next turn, and terminal control bytes
  # bloat the transcript without adding semantic value.
  local iter="${CURRENT_ITER:-?}"
  local esc=$'\033'
  {
    echo ""
    echo "### iteration ${iter}"
    echo ""
    sed -E "s/${esc}\[[0-9;?]*[a-zA-Z]//g; s/\r\$//" "${OUTPUT_FILE}"
  } >> "${HISTORY_FILE}"
}
