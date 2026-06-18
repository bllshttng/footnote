#!/usr/bin/env bash
# Smoke test: SessionStart hook handles REPO_ROOT with a single-quote in the path.
# Before Fix 1, $REPO_ROOT was shell-interpolated directly into a Python string literal,
# so a path like "/tmp/has'quote/repo" would cause a SyntaxError and silently drop signals.
# After the fix, REPO_ROOT is passed via env var and the hook must succeed.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"
HOOK="$REPO_DIR/hooks/inbox-wake-session-start.sh"
if [[ ! -f "$HOOK" ]]; then
  echo "SKIP: hook not found at $HOOK"
  exit 0
fi

TMPBASE=$(mktemp -d)
trap 'rm -rf "$TMPBASE"' EXIT

# Create a directory whose name contains a literal single-quote
QUOTED_DIR="$TMPBASE/has'quote/repo"
mkdir -p "$QUOTED_DIR/.fno/wake-signals"

# Drop a wake-signal in the quoted directory so the hook has something to drain
CLI_DIR="$REPO_DIR/cli"
REPO_ROOT_FOR_DROP="$QUOTED_DIR"
REPO_ROOT="$REPO_ROOT_FOR_DROP" uv run --project "$CLI_DIR" python3 -c "
import os, json, sys
from pathlib import Path
from fno.wake.signal import WakeSignal, drop_signal
from datetime import datetime, timezone
sig = WakeSignal(
    source='test',
    kind='question',
    msg_id='msg-quotepath01',
    from_project='test-proj',
    summary='quote path test signal',
    ts=datetime(2026, 5, 5, 18, 0, tzinfo=timezone.utc),
)
drop_signal(Path(os.environ['REPO_ROOT']), sig)
"

# Verify the signal file was created
SIGNAL_COUNT=$(find "$QUOTED_DIR/.fno/wake-signals" -name 'wake-*.json' 2>/dev/null | wc -l | tr -d ' ')
if [[ "$SIGNAL_COUNT" -ne 1 ]]; then
  echo "FAIL: expected 1 signal file, got $SIGNAL_COUNT"
  exit 1
fi
echo "AC0-SETUP: signal file written to quoted path: PASS"

# Run the hook. CLAUDE_PROJECT_DIR overrides repo-root detection.
# The hook should succeed (exit 0) and emit a system-reminder to stdout.
HOOK_OUTPUT=$(CLAUDE_PROJECT_DIR="$QUOTED_DIR" bash "$HOOK" 2>/dev/null)
HOOK_RC=$?

if [[ $HOOK_RC -ne 0 ]]; then
  echo "FAIL: hook exited with rc=$HOOK_RC on path containing single-quote"
  exit 1
fi
echo "AC1-HP: hook exits 0 on quoted path: PASS"

if [[ "$HOOK_OUTPUT" != *"<system-reminder>"* ]]; then
  echo "FAIL: hook output does not contain <system-reminder> block"
  echo "Output was: $HOOK_OUTPUT"
  exit 1
fi
echo "AC2-HP: hook output contains <system-reminder>: PASS"

# Verify the signal was drained (file deleted)
REMAINING=$(find "$QUOTED_DIR/.fno/wake-signals" -name 'wake-*.json' 2>/dev/null | wc -l | tr -d ' ')
if [[ "$REMAINING" -ne 0 ]]; then
  echo "FAIL: signal file not drained; $REMAINING file(s) remain"
  exit 1
fi
echo "AC3-HP: signal drained after hook: PASS"

echo ""
echo "ALL TESTS PASSED"
