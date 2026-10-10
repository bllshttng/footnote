#!/usr/bin/env bash
# Driver: openclaw (`openclaw agent`)
# Contract: driver_invoke, driver_check_promise, driver_persist_history,
# driver_default_max. Sourced by scripts/run-target-loop.sh.

# Required env (set by the wrapper):
#   OUTPUT_FILE     path where driver stdout+stderr is captured
#   HISTORY_FILE    holds the openclaw session id between iterations
#   SIGNAL_FILE     path to .fno/target-promise.signal
#   MODEL_FLAG      optional "--model NAME" string
#   CONTINUE_PROMPT the slash command to resume (/target --resume etc.)
#   PROMPT_FILE     initial prompt file (if set, read first iteration from it)
# Optional env:
#   OPENCLAW_LOCAL=1  run the embedded agent (--local) instead of the Gateway.
#                     --local refuses while a Gateway owns the state directory.
#
# Each iteration is one `openclaw agent --json` turn. openclaw has no per-turn
# tool cap, so MAX_TURNS is not passed; the loop's own budget bounds the run.
# The first turn opens a fresh session key; the JSON reply carries sessionId,
# which the next iteration passes to --session-id so openclaw keeps the
# conversation in its own store instead of a pasted transcript.

driver_default_max() {
  echo 20
}

driver_invoke() {
  local cli="${OPENCLAW_CLI:-openclaw}"
  if ! command -v "$cli" &>/dev/null; then
    return 77
  fi

  local prompt
  if [[ -n "${PROMPT_FILE:-}" && -f "${PROMPT_FILE}" && ! -s "${HISTORY_FILE:-/dev/null}" ]]; then
    prompt="$(cat "${PROMPT_FILE}")"
  else
    prompt="${CONTINUE_PROMPT:-/target --resume}"
  fi

  local local_flag=""
  [[ "${OPENCLAW_LOCAL:-}" == "1" ]] && local_flag="--local"

  # --timeout 0 lifts openclaw's 600 s turn deadline; a target iteration can
  # run longer, and the loop runtime owns the budget.
  #
  # Build invocation as a single argv to avoid bash 3.2's set -u
  # empty-array bug (macOS).
  if [[ -s "${HISTORY_FILE:-/dev/null}" ]]; then
    # shellcheck disable=SC2086
    "$cli" agent --message "$prompt" --json --timeout 0 \
      ${local_flag} ${MODEL_FLAG:-} \
      --session-id "$(head -n 1 "${HISTORY_FILE}")" > "${OUTPUT_FILE}" 2>&1
  else
    # shellcheck disable=SC2086
    "$cli" agent --message "$prompt" --json --timeout 0 \
      ${local_flag} ${MODEL_FLAG:-} \
      --session-key "fno-loop-$(date +%s)-$$" > "${OUTPUT_FILE}" 2>&1
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
  # Keep only the session id from the JSON reply: openclaw stores the
  # conversation itself. A turn that printed no id leaves the previous one.
  local sid
  sid=$(grep -oE '"sessionId"[[:space:]]*:[[:space:]]*"[^"]+"' "${OUTPUT_FILE}" 2>/dev/null \
    | head -n 1 | sed -E 's/.*"([^"]+)"$/\1/')
  if [[ -n "$sid" ]]; then
    printf '%s\n' "$sid" > "${HISTORY_FILE}"
  fi
}
