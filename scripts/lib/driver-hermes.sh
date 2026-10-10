#!/usr/bin/env bash
# Driver: hermes (Hermes Agent, `hermes chat`)
# Contract: driver_invoke, driver_check_promise, driver_persist_history,
# driver_default_max. Sourced by scripts/run-target-loop.sh.

# Required env (set by the wrapper):
#   OUTPUT_FILE     path where driver stdout+stderr is captured
#   HISTORY_FILE    holds the hermes session id between iterations
#   SIGNAL_FILE     path to .fno/target-promise.signal
#   MAX_TURNS       per-turn tool-call cap (hermes --max-turns)
#   MODEL_FLAG      optional "--model NAME" string
#   CONTINUE_PROMPT the slash command to resume (/target --resume etc.)
#   PROMPT_FILE     initial prompt file (if set, read first iteration from it)
#
# Each iteration is one `hermes chat -q` turn with --format stream-json: JSONL
# on stdout, ending in a `result` record that carries the session id. The next
# iteration passes that id to --resume, so hermes keeps the conversation in its
# own session store instead of a pasted transcript. The legacy `hermes-agent`
# runner has none of these flags.

driver_default_max() {
  echo 20
}

driver_invoke() {
  local cli="${HERMES_CLI:-hermes}"
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

  # --yolo matches the claude driver's --dangerously-skip-permissions: an
  # unattended loop has nobody to answer an approval prompt.
  #
  # Build the invocation as a single argv to avoid bash 3.2's set -u
  # empty-array bug (macOS).
  if [[ -s "${HISTORY_FILE:-/dev/null}" ]]; then
    # shellcheck disable=SC2086
    "$cli" chat -q "$prompt" \
      --format stream-json --yolo \
      --max-turns "${MAX_TURNS:-15}" \
      ${MODEL_FLAG:-} \
      --resume "$(head -n 1 "${HISTORY_FILE}")" > "${OUTPUT_FILE}" 2>&1
  else
    # shellcheck disable=SC2086
    "$cli" chat -q "$prompt" \
      --format stream-json --yolo \
      --max-turns "${MAX_TURNS:-15}" \
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
  # Keep only the session id: hermes stores the conversation itself. The
  # `result` record is the last line hermes writes; `system/init` carries the
  # same id when a turn fails before the result. A turn that printed neither
  # leaves the previous id in place.
  local sid
  sid=$(sed -n 's/^{"type": "result", "session_id": "\([^"]*\)".*/\1/p' "${OUTPUT_FILE}" | tail -n 1)
  if [[ -z "$sid" ]]; then
    sid=$(sed -n 's/^{"type": "system", "subtype": "init", .*"session_id": "\([^"]*\)".*/\1/p' "${OUTPUT_FILE}" | tail -n 1)
  fi
  if [[ -n "$sid" ]]; then
    printf '%s\n' "$sid" > "${HISTORY_FILE}"
  fi
}
