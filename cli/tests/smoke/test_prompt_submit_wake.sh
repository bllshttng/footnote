#!/usr/bin/env bash
# Smoke test: UserPromptSubmit hook surfaces kind=question wake-signals
# AC1-HP: question signal -> reminder in stdout, signal deleted, exit 0
# AC2-ERR: no signals -> empty stdout, exit 0
# AC4-EDGE: two prompts in a row, one signal -> first prompt sees reminder, second sees nothing
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
CLI_DIR="$REPO_ROOT/cli"
HOOK="$REPO_ROOT/hooks/inbox-wake-prompt-submit.sh"

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

echo "$OUT_HP" | grep -q "Inbox: 1 new question(s) since your last turn" \
  || { echo "AC1-HP FAIL: missing reminder header in: $OUT_HP"; exit 1; }
echo "$OUT_HP" | grep -q "msg-deadbeef" \
  || { echo "AC1-HP FAIL: missing msg_id in output"; exit 1; }
echo "$OUT_HP" | grep -q "<system-reminder>" \
  || { echo "AC1-HP FAIL: missing <system-reminder> tag"; exit 1; }

# Signal file must be deleted
COUNT=$(find "$HP1_DIR/.fno/wake-signals" -name 'wake-*.json' 2>/dev/null | wc -l | tr -d ' ')
[[ "$COUNT" == "0" ]] || { echo "AC1-HP FAIL: signal not consumed, $COUNT files remain"; exit 1; }

# ---- AC4-EDGE: two prompts, one signal -> first sees it, second sees nothing ----
EDGE_DIR="$WORK/ac4"
mkdir -p "$EDGE_DIR"
cd "$EDGE_DIR"

uv run --project "$CLI_DIR" fno wake drop \
  --source inbox-drain \
  --kind question \
  --msg-id msg-edge \
  --from example-pipeline \
  --summary "edge case question" > /dev/null

# First prompt: should see the signal
OUT_EDGE1=$(CLAUDE_PROJECT_DIR="$EDGE_DIR" bash "$HOOK" 2>/dev/null)
echo "$OUT_EDGE1" | grep -q "Inbox: 1 new question(s) since your last turn" \
  || { echo "AC4-EDGE FAIL: first prompt missing reminder in: $OUT_EDGE1"; exit 1; }
echo "$OUT_EDGE1" | grep -q "msg-edge" \
  || { echo "AC4-EDGE FAIL: first prompt missing msg_id"; exit 1; }

# Second prompt: no signals (drained by first), must produce empty output
OUT_EDGE2=$(CLAUDE_PROJECT_DIR="$EDGE_DIR" bash "$HOOK" 2>/dev/null)
[[ -z "$OUT_EDGE2" ]] || { echo "AC4-EDGE FAIL: second prompt expected empty stdout, got: $OUT_EDGE2"; exit 1; }

echo "OK"
