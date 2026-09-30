#!/bin/bash
# Run one Run 1 lane through the eval runner, appending to a log:
#   run1.sh <lane> <repeat> <log> [<task-id>]
# The lane's cohort is harness-fit-<lane>. FNO_AGENTS_BIN pins the branch's fno-agents.
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
WS="${HARNESS_FIT_WS:-$HOME/evals-workspace/harness-fit}"
exec >> "$3" 2>&1
set -a; . "$HOME/.fno/.env"; set +a
export FNO_CONFIG="$WS/run1-config.toml"
export PI_CODING_AGENT_DIR="$WS/pi-agent"
cd "$REPO" || exit 1
task=()
[ -n "$4" ] && task=(--task "$4")
exec fno doctor evals run --bank "$REPO/evals/harness-fit/bank" --lane "harness-fit-$1" \
  --cohort "harness-fit-$1" --repeat "$2" "${task[@]}" -y
