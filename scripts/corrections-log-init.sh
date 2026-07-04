#!/usr/bin/env bash
# corrections-log-init.sh - ensure ~/.fno/corrections.log exists with mode 0600.
#
# Idempotent: safe to run any number of times. Creates the file on first run,
# verifies mode on subsequent runs and corrects if drifted.
#
# Called by /autocorrect install (one-time setup) and indirectly by any writer
# that touches the log for the first time (defense-in-depth).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/corrections-lock.sh
source "$SCRIPT_DIR/lib/corrections-lock.sh"

LOG_PATH="$(corrections_log_path)"
FNO_HOME_DIR="$(dirname "$LOG_PATH")"

# Unlike the old ~/.claude location, ~/.fno is owned by footnote itself, so
# it's safe to create lazily rather than refusing when absent.
mkdir -p "$FNO_HOME_DIR"

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
