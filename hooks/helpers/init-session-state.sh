#!/bin/bash
# init-session-state.sh - PreToolUse hook for think/plan/audit skills
#
# Writes a lightweight state file so the stop hook can detect the session
# type and auto-register cost in ledger.json on exit.
#
# Usage: Called via PreToolUse hook (once: true) on think/plan/audit skills.
# Expects SESSION_TYPE env var to be set by the hook config.

set -euo pipefail

STATE_DIR=".fno"
STATE_FILE="${STATE_DIR}/session-state.md"
SESSION_TYPE="${SESSION_TYPE:-unknown}"

mkdir -p "$STATE_DIR"

# Don't overwrite if target is running (it has its own state)
if [[ -f "${STATE_DIR}/target-state.md" ]]; then
    echo "✓ target active — skipping session state init" >&2
    exit 0
fi

# Clear sentinel from previous session
rm -f "${STATE_DIR}/.session-registered"

TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ)

cat > "$STATE_FILE" << EOF
---
type: ${SESSION_TYPE}
status: IN_PROGRESS
created_at: ${TIMESTAMP}
---
# Session State

${SESSION_TYPE} session initialized at ${TIMESTAMP}
EOF

echo "✓ Session state initialized: type=${SESSION_TYPE}" >&2
