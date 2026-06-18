#!/usr/bin/env bash
# Smoke test: SessionStart hook surfaces kind=question wake-signals
# AC1-HP: question signal -> reminder in stdout, signal deleted, exit 0
# AC2-ERR: no signals -> empty stdout, exit 0
# AC4-EDGE: lesson + question -> only question surfaced, lesson stays on disk
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
CLI_DIR="$REPO_ROOT/cli"
HOOK="$REPO_ROOT/hooks/inbox-wake-session-start.sh"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# ---- AC2-ERR: no signals dir -> empty stdout, exit 0 --------------------
HP2_DIR="$WORK/ac2"
mkdir -p "$HP2_DIR"
OUT_ERR=$(CLAUDE_PROJECT_DIR="$HP2_DIR" bash "$HOOK" 2>/dev/null)
[[ -z "$OUT_ERR" ]] || { echo "AC2-ERR FAIL: expected empty stdout, got: $OUT_ERR"; exit 1; }

# ---- AC1-HP: one question signal -> reminder header, signal deleted ------
HP1_DIR="$WORK/ac1"
mkdir -p "$HP1_DIR"
cd "$HP1_DIR"

uv run --project "$CLI_DIR" fno wake drop \
  --source inbox-drain \
  --kind question \
  --msg-id msg-deadbeef \
  --from example-pipeline \
  --summary "blocked on parser shape" > /dev/null

OUT_HP=$(CLAUDE_PROJECT_DIR="$HP1_DIR" bash "$HOOK" 2>/dev/null)

echo "$OUT_HP" | grep -q "Inbox: 1 question(s) waiting" \
  || { echo "AC1-HP FAIL: missing reminder header in: $OUT_HP"; exit 1; }
echo "$OUT_HP" | grep -q "msg-deadbeef" \
  || { echo "AC1-HP FAIL: missing msg_id in output"; exit 1; }
echo "$OUT_HP" | grep -q "<system-reminder>" \
  || { echo "AC1-HP FAIL: missing <system-reminder> tag"; exit 1; }

# Signal file must be deleted
COUNT=$(find "$HP1_DIR/.fno/wake-signals" -name 'wake-*.json' 2>/dev/null | wc -l | tr -d ' ')
[[ "$COUNT" == "0" ]] || { echo "AC1-HP FAIL: signal not consumed, $COUNT files remain"; exit 1; }

# ---- AC4-EDGE: lesson + question -> only question surfaced ---------------
EDGE_DIR="$WORK/ac4"
mkdir -p "$EDGE_DIR"
cd "$EDGE_DIR"

# Drop a question signal
uv run --project "$CLI_DIR" fno wake drop \
  --source inbox-drain \
  --kind question \
  --msg-id msg-question \
  --from example-pipeline \
  --summary "a question" > /dev/null

# Drop a lesson signal via raw Python (kind=lesson may not have a CLI verb yet)
uv run --project "$CLI_DIR" python3 -c "
import json, time
from pathlib import Path
p = Path('$EDGE_DIR/.fno/wake-signals/wake-lesson-test.json')
p.parent.mkdir(parents=True, exist_ok=True)
p.write_text(json.dumps({
    'signal_id': 'wake-lesson-test',
    'kind': 'lesson',
    'from_project': 'example-pipeline',
    'msg_id': 'msg-lesson',
    'summary': 'a lesson',
    'dropped_at': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
}))
"

OUT_EDGE=$(CLAUDE_PROJECT_DIR="$EDGE_DIR" bash "$HOOK" 2>/dev/null)

# Question must surface
echo "$OUT_EDGE" | grep -q "msg-question" \
  || { echo "AC4-EDGE FAIL: question not surfaced"; exit 1; }

# Lesson must NOT appear in output
if echo "$OUT_EDGE" | grep -q "msg-lesson"; then
  echo "AC4-EDGE FAIL: lesson should not appear in output"
  exit 1
fi

# Question signal gone, lesson still on disk
COUNT_Q=$(find "$EDGE_DIR/.fno/wake-signals" -name 'wake-*.json' -exec grep -l '"kind": "question"' {} \; 2>/dev/null | wc -l | tr -d ' ')
COUNT_L=$(find "$EDGE_DIR/.fno/wake-signals" -name 'wake-*.json' -exec grep -l '"kind": "lesson"' {} \; 2>/dev/null | wc -l | tr -d ' ')
[[ "$COUNT_Q" == "0" ]] || { echo "AC4-EDGE FAIL: question signal not consumed"; exit 1; }
[[ "$COUNT_L" == "1" ]] || { echo "AC4-EDGE FAIL: lesson signal should remain on disk, found $COUNT_L"; exit 1; }

echo "OK"
