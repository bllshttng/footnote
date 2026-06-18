#!/usr/bin/env bash
# corrections-log-init.sh - ensure ~/.claude/corrections.log exists with mode 0600.
#
# Idempotent: safe to run any number of times. Creates the file on first run,
# verifies mode on subsequent runs and corrects if drifted.
#
# Called by /autocorrect install (one-time setup) and indirectly by any writer
# that touches the log for the first time (defense-in-depth).

set -euo pipefail

CLAUDE_DIR="${CLAUDE_DIR_OVERRIDE:-$HOME/.claude}"
LOG_PATH="$CLAUDE_DIR/corrections.log"

# ~/.claude must exist. We do not create it - if it doesn't exist, Claude Code
# is not installed and this loop has nothing to do.
if [[ ! -d "$CLAUDE_DIR" ]]; then
  echo "corrections-log-init: $CLAUDE_DIR does not exist; refusing to bootstrap" >&2
  echo "corrections-log-init: install Claude Code first, or set CLAUDE_DIR_OVERRIDE" >&2
  exit 1
fi

if [[ -f "$LOG_PATH" ]]; then
  # Verify mode 0600. Use stat with platform-specific format.
  if [[ "$(uname)" == "Darwin" ]]; then
    CURRENT_MODE=$(stat -f "%Lp" "$LOG_PATH" 2>/dev/null || echo "")
  else
    CURRENT_MODE=$(stat -c "%a" "$LOG_PATH" 2>/dev/null || echo "")
  fi
  if [[ "$CURRENT_MODE" != "600" ]]; then
    chmod 600 "$LOG_PATH"
    echo "corrections-log-init: corrected mode on $LOG_PATH (was $CURRENT_MODE, now 600)" >&2
  fi
  exit 0
fi

# First-run bootstrap.
touch "$LOG_PATH"
chmod 600 "$LOG_PATH"
echo "corrections-log-init: created $LOG_PATH (mode 0600)" >&2
