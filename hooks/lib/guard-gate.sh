#!/usr/bin/env bash
# guard-gate.sh <name> <cmd...> - the guardrail preset gate for the Python
# guards. Reads stdin once, asks the Rust binary whether the project's
# guardrail preset runs that guard, and on a "no" answers the empty allow
# without running it. Anything other than a clean exit-1 "disabled" answer
# runs the guard unchanged: a missing binary, an old build that does not know
# the entry, or a crash all fail closed.
set -uo pipefail

NAME="${1:?usage: guard-gate.sh <guard-name> <cmd...>}"
shift

PAYLOAD="$(cat)"

if BIN="$(command -v fno-agents)"; then
  printf '%s' "$PAYLOAD" | "$BIN" hook guard-enabled "$NAME" >/dev/null 2>&1
  rc=$?
  if [ "$rc" -eq 1 ]; then
    printf '{}\n'
    exit 0
  fi
  # rc 0 = the preset runs this guard; anything else (an old build, a crash)
  # could not answer, so the guard runs unchanged.
fi

printf '%s' "$PAYLOAD" | "$@"
