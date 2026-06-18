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
# `python3 -m fno.cost.<mod>`; in a checkout point PYTHONPATH at the package
# source so it works pre-install, otherwise rely on the installed `fno`.
_REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
if [[ -f "${_REPO_ROOT}/cli/src/fno/cost/_session_cost.py" ]]; then
    export PYTHONPATH="${_REPO_ROOT}/cli/src${PYTHONPATH:+:${PYTHONPATH}}"
fi

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
COST_JSON=$(python3 -m fno.cost._session_cost --json "$SESSION_ID" 2>/dev/null || echo "{}")

# Pass all args through to fno.cost._register + add session and cost
python3 -m fno.cost._register \
    --session "$SESSION_ID" \
    --cost-json "$COST_JSON" \
    "$@" 2>/dev/null || {
    echo "register-session-cost: registration failed (non-blocking)" >&2
    exit 0
}
