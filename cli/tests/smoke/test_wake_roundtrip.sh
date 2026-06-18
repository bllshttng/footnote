#!/usr/bin/env bash
# Smoke test: fno wake drop -> drain_signals roundtrip
# Exercises CLI wiring + file paths + Python module exports end-to-end.
# No side effects outside TMPDIR.
set -euo pipefail

CLI_DIR="$(git rev-parse --show-toplevel)/cli"

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT
cd "$TMPDIR"

# Drop a signal via the CLI verb
uv run --project "$CLI_DIR" fno wake drop \
  --source inbox-drain \
  --kind question \
  --msg-id msg-deadbeef \
  --from foo \
  --summary "test signal" > /dev/null

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

echo "OK"
