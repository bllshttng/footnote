#!/usr/bin/env bash
# Smoke test: target stop hook logs wake_signal_observed for kind=question signals
# AC1-HP: question signal on disk -> wake_signal_observed line in hook-events.jsonl, signal NOT deleted
# AC2-ERR: no signals -> no wake_signal_observed lines appended
# AC4-EDGE: two question signals -> two wake_signal_observed lines, both signals remain
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
CLI_DIR="$REPO_ROOT/cli"
HOOK="$REPO_ROOT/hooks/target-stop-hook.sh"
# Phase 1 of stop-hook refactor: _observe_wake_signals lives in
# scripts/lib/wake-signals.sh; the hook sources it. Sourcing the lib
# directly is more robust than awk-extracting from the hook and matches
# how production code loads the helper.
WAKE_LIB="$REPO_ROOT/scripts/lib/wake-signals.sh"

# Helper: source and run _observe_wake_signals in an isolated bash subshell.
# Sets REPO_ROOT, STATE_DIR, and SCRIPT_DIR so the helper can find the cli package.
_run_observe_helper() {
    local tmpdir="$1"
    mkdir -p "$tmpdir/.fno"

    bash -c "
set -euo pipefail
REPO_ROOT='$tmpdir'
STATE_DIR='$tmpdir/.fno'
SCRIPT_DIR='$REPO_ROOT'

source '$WAKE_LIB'

_observe_wake_signals
" 2>/dev/null
}

# ---- AC2-ERR: no signals -> no wake_signal_observed lines ---------------
TMP_ERR=$(mktemp -d)
trap 'rm -rf "$TMP_ERR"' EXIT

_run_observe_helper "$TMP_ERR"

HOOK_EVENTS_ERR="$TMP_ERR/.fno/hook-events.jsonl"
if [[ -f "$HOOK_EVENTS_ERR" ]]; then
    COUNT_ERR=$(grep -c '"wake_signal_observed"' "$HOOK_EVENTS_ERR" 2>/dev/null || echo 0)
    [[ "$COUNT_ERR" -eq 0 ]] || { echo "AC2-ERR FAIL: expected 0 wake_signal_observed lines, got $COUNT_ERR"; exit 1; }
fi
echo "AC2-ERR: PASS"

# ---- AC1-HP: one question signal -> one log line, signal NOT deleted -----
TMP_HP=$(mktemp -d)
mkdir -p "$TMP_HP/.fno/wake-signals"

# Drop a signal directly via the Python helper
uv run --project "$CLI_DIR" python3 -c "
from datetime import datetime, timezone
from pathlib import Path
from fno.wake.signal import WakeSignal, drop_signal
sig = WakeSignal(
    source='inbox-drain', kind='question',
    msg_id='msg-testac1', from_project='example-pipeline',
    summary='blocked on parser shape',
    ts=datetime.now(timezone.utc),
)
drop_signal(Path('$TMP_HP'), sig)
"

SIGNAL_COUNT_BEFORE=$(find "$TMP_HP/.fno/wake-signals" -name 'wake-*.json' 2>/dev/null | wc -l | tr -d ' ')
[[ "$SIGNAL_COUNT_BEFORE" -eq 1 ]] || { echo "AC1-HP SETUP FAIL: expected 1 signal before run, got $SIGNAL_COUNT_BEFORE"; exit 1; }

_run_observe_helper "$TMP_HP"

HOOK_EVENTS_HP="$TMP_HP/.fno/hook-events.jsonl"
[[ -f "$HOOK_EVENTS_HP" ]] || { echo "AC1-HP FAIL: hook-events.jsonl not created"; exit 1; }

COUNT_HP=$(grep -c '"wake_signal_observed"' "$HOOK_EVENTS_HP" 2>/dev/null || echo 0)
[[ "$COUNT_HP" -eq 1 ]] || { echo "AC1-HP FAIL: expected 1 wake_signal_observed line, got $COUNT_HP"; exit 1; }

# Verify the event contains the expected fields
grep '"wake_signal_observed"' "$HOOK_EVENTS_HP" | python3 -c "
import json, sys
line = sys.stdin.read().strip()
evt = json.loads(line)
assert evt.get('event') == 'wake_signal_observed', f'wrong event field: {evt}'
assert evt.get('kind') == 'question', f'wrong kind: {evt}'
assert evt.get('msg_id') == 'msg-testac1', f'wrong msg_id: {evt}'
assert evt.get('signal_id', '').startswith('wake-'), f'bad signal_id: {evt}'
print('fields OK')
" || { echo "AC1-HP FAIL: event JSON fields wrong"; exit 1; }

# Verify signal NOT deleted
SIGNAL_COUNT_AFTER=$(find "$TMP_HP/.fno/wake-signals" -name 'wake-*.json' 2>/dev/null | wc -l | tr -d ' ')
[[ "$SIGNAL_COUNT_AFTER" -eq 1 ]] || { echo "AC1-HP FAIL: signal was deleted (count after=$SIGNAL_COUNT_AFTER)"; exit 1; }

echo "AC1-HP: PASS"

# ---- AC4-EDGE: two signals -> two log lines, both remain on disk ---------
TMP_EDGE=$(mktemp -d)
mkdir -p "$TMP_EDGE/.fno/wake-signals"

# Write two signals directly as JSON (no CLI needed)
for ID in alpha beta; do
    cat > "$TMP_EDGE/.fno/wake-signals/wake-${ID}.json" <<SIGEOF
{
  "signal_id": "wake-${ID}",
  "kind": "question",
  "from_project": "example-pipeline",
  "msg_id": "msg-${ID}",
  "summary": "test signal ${ID}",
  "ts": "2026-05-05T00:00:00Z"
}
SIGEOF
done

_run_observe_helper "$TMP_EDGE"

HOOK_EVENTS_EDGE="$TMP_EDGE/.fno/hook-events.jsonl"
[[ -f "$HOOK_EVENTS_EDGE" ]] || { echo "AC4-EDGE FAIL: hook-events.jsonl not created"; exit 1; }

COUNT_EDGE=$(grep -c '"wake_signal_observed"' "$HOOK_EVENTS_EDGE" 2>/dev/null || echo 0)
[[ "$COUNT_EDGE" -eq 2 ]] || { echo "AC4-EDGE FAIL: expected 2 wake_signal_observed lines, got $COUNT_EDGE"; exit 1; }

SIGNAL_COUNT_EDGE=$(find "$TMP_EDGE/.fno/wake-signals" -name 'wake-*.json' 2>/dev/null | wc -l | tr -d ' ')
[[ "$SIGNAL_COUNT_EDGE" -eq 2 ]] || { echo "AC4-EDGE FAIL: expected 2 signals to remain, got $SIGNAL_COUNT_EDGE"; exit 1; }

echo "AC4-EDGE: PASS"

echo ""
echo "ALL TESTS PASSED"
