#!/usr/bin/env bash
set -euo pipefail

CMD="${*:-}"
if [[ -z "$CMD" ]]; then
  echo "Usage: pre-tool-use.sh <command string>" >&2
  exit 1
fi

# Block high-risk destructive patterns in non-interactive agent runs.
if [[ "$CMD" =~ (^|[[:space:]])rm[[:space:]]+-rf([[:space:]]|$) ]]; then
  echo "Blocked command: rm -rf is not allowed by soft governance policy" >&2
  exit 2
fi

if [[ "$CMD" =~ git[[:space:]]+reset[[:space:]]+--hard ]]; then
  echo "Blocked command: git reset --hard is not allowed by soft governance policy" >&2
  exit 2
fi

if [[ "$CMD" =~ git[[:space:]]+push([^\n])*--force ]]; then
  echo "Blocked command: git push --force is not allowed by soft governance policy" >&2
  exit 2
fi

echo "Allowed command"
