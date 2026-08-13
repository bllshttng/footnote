#!/usr/bin/env bash
# Smoke test: drop_signal -> drain_signals roundtrip.
# Exercises the wake-signal file paths + Python module exports end-to-end.
# No side effects outside TMPDIR.
#
# This used to drive the write half through `fno wake drop`. Those ADMIN verbs
# were removed for having no caller; the signals themselves are live, written
# and read by `fno.wake.signal` on the inbox-drain and mail paths. So the
# roundtrip is still worth proving, through the surface that actually runs it.
set -euo pipefail

CLI_DIR="$(git rev-parse --show-toplevel)/cli"

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT
cd "$TMPDIR"

# Drop a signal via the live writer.
uv run --project "$CLI_DIR" python3 -c "
from datetime import datetime, timezone
from pathlib import Path
from fno.wake.signal import WakeSignal, drop_signal
drop_signal(Path('.'), WakeSignal(
    source='inbox-drain',
    kind='question',
    msg_id='msg-deadbeef',
    from_project='foo',
    summary='test signal',
    ts=datetime.now(timezone.utc),
))
" > /dev/null

# Verify exactly one signal file was created
[[ -d .fno/wake-signals ]] || { echo "FAIL: wake-signals dir missing"; exit 1; }
COUNT=$(find .fno/wake-signals -name 'wake-*.json' 2>/dev/null | wc -l | tr -d ' ')
[[ "$COUNT" == "1" ]] || { echo "FAIL: expected 1 signal file after drop, got $COUNT"; exit 1; }

# Drain via the Python helper (mirrors what the hook will do)
DRAINED=$(uv run --project "$CLI_DIR" python3 -c "
import json
from pathlib import Path
from fno.wake.signal import drain_signals
out = drain_signals(Path('.'), kind='question')
print(json.dumps(out))
")

# Confirm the drain returned the signal
echo "$DRAINED" | uv run --project "$CLI_DIR" python3 -c "
import json, sys
data = json.load(sys.stdin)
assert len(data) == 1, f'expected 1 drained signal, got {len(data)}'
assert data[0]['msg_id'] == 'msg-deadbeef', f'wrong msg_id: {data[0][\"msg_id\"]}'
"

# Confirm the file is gone (use find to avoid glob-fail when dir is empty)
COUNT_AFTER=$(find .fno/wake-signals -name 'wake-*.json' 2>/dev/null | wc -l | tr -d ' ')
[[ "$COUNT_AFTER" == "0" ]] || { echo "FAIL: drain did not delete: $COUNT_AFTER files left"; exit 1; }

# The removed verb must still refuse BY NAME rather than as a typo.
OUT=$(uv run --project "$CLI_DIR" fno-py wake drop 2>&1 || true)
case "$OUT" in
  *"was removed"*) ;;
  *) echo "FAIL: \`fno wake\` no longer names its removal: $OUT"; exit 1 ;;
esac

echo "OK"
