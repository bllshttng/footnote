#!/usr/bin/env bash
# register-session-cost.sh — Calculate session cost and register in ledger.json
#
# Shared by /think, /plan, /audit skills. Non-blocking — failures logged, don't stop the skill.
#
# Usage:
#   bash register-session-cost.sh --type think --title "Feature X"
#   bash register-session-cost.sh --type spec --title "Feature X" --plan-path "path/to/plan"
#   bash register-session-cost.sh --type audit --title "Feature X"

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The cost helpers moved into the fno package (cli/src/fno/cost/). Run them as
# `"$FNO_PYTHON" -m fno.cost.<mod>`: a bare `python3` is whatever is first on
# PATH, and one without fno's deps drops the ledger row silently (the failures
# below are non-blocking by design). fno_python_init also puts the package
# source on PYTHONPATH so a checkout works pre-install.
_REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
# shellcheck source=../lib/fno-python.sh
source "${_REPO_ROOT}/scripts/lib/fno-python.sh" 2>/dev/null || true
declare -F fno_python_init >/dev/null && fno_python_init "${_REPO_ROOT}"
# A partial deploy that dropped the helper degrades to the old behavior rather
# than tripping `set -u` on an unset FNO_PYTHON in this non-blocking path.
FNO_PYTHON="${FNO_PYTHON:-python3}"

# Find current session ID from most recent JSONL in this project's Claude dir
find_session_id() {
    local encoded_path
    encoded_path=$(echo "$PWD" | sed 's|^/|-|;s|/|-|g')
    local project_dir="$HOME/.claude/projects/${encoded_path}"

    if [[ -d "$project_dir" ]]; then
        ls -t "$project_dir"/*.jsonl 2>/dev/null | head -1 | xargs basename 2>/dev/null | sed 's/\.jsonl$//'
    fi
}

SESSION_ID=$(find_session_id)
if [[ -z "$SESSION_ID" ]]; then
    echo "register-session-cost: no session found, skipping" >&2
    exit 0
fi

# Get cost JSON
COST_JSON=$("$FNO_PYTHON" -m fno.cost._session_cost --json "$SESSION_ID" 2>/dev/null || echo "{}")

# Pass all args through to fno.cost._register + add session and cost
"$FNO_PYTHON" -m fno.cost._register \
    --session "$SESSION_ID" \
    --cost-json "$COST_JSON" \
    "$@" 2>/dev/null || {
    echo "register-session-cost: registration failed (non-blocking)" >&2
    exit 0
}
