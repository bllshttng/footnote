#!/usr/bin/env bash
# Clone events.jsonl + ship artifact to sandbox, mutate nonce, expect gate verify to fail.
# Tests that the nonce-binding integrity check catches forged artifacts.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)

SANDBOX=$(mktemp -d -t e2e-forgery-XXXXXX)
trap "rm -rf $SANDBOX" EXIT

echo "Sandbox: $SANDBOX" >&2

# Set up sandbox state
mkdir -p "$SANDBOX/artifacts"

# Write a valid phase_init event with a known nonce
KNOWN_NONCE=$(python3 -c "import secrets; print(secrets.token_hex(16))")
SID="20260421T000000Z-99999-testsid"

cat > "$SANDBOX/events.jsonl" <<EOF
{"type":"phase_init","campaign_id":null,"session_id":"$SID","nonce":"$KNOWN_NONCE","ts":"2026-04-21T00:00:00Z","payload":{"phase":"ship"}}
EOF

# Write a ship artifact with a FORGED nonce (not matching the event)
FORGED_NONCE="deadbeefdeadbeefdeadbeefdeadbeef"
cat > "$SANDBOX/artifacts/ship-$SID.md" <<EOF
---
phase: ship
session_id: $SID
nonce: $FORGED_NONCE
pr_number: 999
completed_at: 2026-04-21T00:00:00Z
---

Forged ship artifact for E2E forgery test.
EOF

# Write a minimal state file so gate verify can read session_id
# Also set artifact_shipped: true so factor 1 (state flag) passes and nonce check is reached
cat > "$SANDBOX/state.md" <<EOF
---
session_id: $SID
status: IN_PROGRESS
artifact_shipped: true
---
# State
EOF

echo "Known nonce  : $KNOWN_NONCE" >&2
echo "Forged nonce : $FORGED_NONCE" >&2
echo "Session ID   : $SID" >&2
echo "" >&2

# Run gate verify - expect non-zero exit
cd "$REPO_ROOT/cli"
set +e
out=$(uv run fno --json gate verify \
  --phase ship \
  --state "$SANDBOX/state.md" \
  --artifacts-dir "$SANDBOX/artifacts" \
  --events "$SANDBOX/events.jsonl" \
  --skip-reality-check 2>&1)
rc=$?
set -e

echo "gate verify output:" >&2
echo "$out" | python3 -m json.tool 1>&2 || echo "$out" >&2
echo "" >&2

if [[ $rc -eq 0 ]]; then
  echo "FAIL: gate verify returned 0 for forged nonce" >&2
  exit 1
fi

if ! echo "$out" | python3 -c "import sys,json; d=json.load(sys.stdin); e=d.get('error',{}); assert e.get('kind')=='nonce_mismatch', f'expected nonce_mismatch, got {e}'" 2>/dev/null; then
  echo "FAIL: output did not report nonce_mismatch (got: $out)" >&2
  exit 1
fi

# Check that integrity_violation event was appended
if ! python3 -c "
import json
events = [json.loads(l) for l in open('$SANDBOX/events.jsonl') if l.strip()]
iv_events = [e for e in events if e.get('type') == 'integrity_violation']
assert len(iv_events) > 0, f'no integrity_violation event found; events: {events}'
" 2>&1; then
  echo "FAIL: integrity_violation event not appended to events.jsonl" >&2
  exit 1
fi

echo "PASS: forgery detected (exit $rc, nonce_mismatch reported, integrity_violation logged)"
