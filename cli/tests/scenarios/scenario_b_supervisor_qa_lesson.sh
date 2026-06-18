#!/usr/bin/env bash
# Scenario B: Q/A/lesson chain between abilities (supervisor) and acme-web.
# Exercises: question -> answer reply -> lesson send; verifies reply_to chain.
# No triage stub needed (no heads-up). No graph mutations expected.
#
# Bash 3.2 compatible (macOS default). No mapfile, no associative arrays,
# no here-strings for arrays, no ${var,,} lowercasing.

set -euo pipefail

# Resolve the cli/ dir from this script's location (was a hardcoded worktree path).
CLI_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

TMP=$(mktemp -d /tmp/abi-scenario-B-XXXXXX)

cleanup() {
    rm -rf "$TMP"
}
trap cleanup EXIT

# Temp home so ~/.fno/graph.json stays sandboxed
HOME_OVERRIDE="$TMP/home"
mkdir -p "$HOME_OVERRIDE/.fno"
echo '{"entries":[]}' > "$HOME_OVERRIDE/.fno/graph.json"
INITIAL_ENTRY_COUNT=0

# Inbox root
INBOX_ROOT="$TMP/inbox"
mkdir -p "$INBOX_ROOT"

# Two project fixture dirs
FNO_DIR="$TMP/abilities"
WEB_DIR="$TMP/acme-web"
mkdir -p "$FNO_DIR/.fno"
mkdir -p "$WEB_DIR/.fno"

cat > "$FNO_DIR/.fno/settings.yaml" <<'SETTINGS'
project: abilities
SETTINGS

cat > "$WEB_DIR/.fno/settings.yaml" <<'SETTINGS'
project: acme-web
SETTINGS

# Temp memory dir for the lesson handler simulation
MEMORY_DIR="$TMP/memory/acme-web"
mkdir -p "$MEMORY_DIR"

# Common env for all fno calls
export FNO_INBOX_ROOT="$INBOX_ROOT"
export FNO_INBOX_KNOWN_PROJECTS="abilities,acme-web"
export HOME="$HOME_OVERRIDE"

run_abi() {
    cd "$CLI_DIR" && uv run fno "$@"
}

echo "=== Scenario B: Q/A/lesson chain (supervisor) ==="
echo "TMP=$TMP"

# ---------------------------------------------------------------------------
# MAIN FLOW
# ---------------------------------------------------------------------------

# Step 1: abilities sends a question to acme-web
echo ""
echo "--- Step 1: abilities sends question to acme-web ---"
SEND_Q_OUT=$(cd "$FNO_DIR" && run_abi mail send \
    --to-project acme-web \
    --kind question \
    --ref-node ab-abc12345 \
    --ref-gate review_passed \
    --body "You hit gate review_passed 3 iterations running. What is stuck?" \
    --from-name abilities)
echo "$SEND_Q_OUT"

MSG_Q=$(echo "$SEND_Q_OUT" | grep -oE 'msg-[0-9a-f]+')
if [ -z "$MSG_Q" ]; then
    echo "FAIL: could not capture MSG_Q from send output"
    exit 1
fi
echo "Captured MSG_Q=$MSG_Q"

# Step 2: From acme-web - simulate question handler (without /think spawn)

# 2a: verify question is in web's unread
echo ""
echo "--- Step 2a: acme-web sees the question in unread ---"
UNREAD_JSON=$(cd "$WEB_DIR" && run_abi mail unread --json --name acme-web)
if ! echo "$UNREAD_JSON" | python3 -c "
import json, sys
msgs = json.load(sys.stdin)
assert any(m['msg_id'] == '$MSG_Q' for m in msgs), 'MSG_Q not in unread'
print('OK: MSG_Q is in unread list')
"; then
    echo "FAIL: MSG_Q not in unread"
    echo "$UNREAD_JSON"
    exit 1
fi

# 2b: hard-coded answer body (no /think spawn in test)
ANSWER_BODY="silent-failure-hunter HIGH on swallow_errors_in_dispatch.py; that file blocks gate convergence"

# 2c: reply with kind=answer (routes to abilities inbox)
echo ""
echo "--- Step 2c: acme-web replies with kind=answer ---"
REPLY_OUT=$(cd "$WEB_DIR" && run_abi mail reply \
    --to "$MSG_Q" \
    --kind answer \
    --body "$ANSWER_BODY" \
    --from acme-web)
echo "$REPLY_OUT"

MSG_A=$(echo "$REPLY_OUT" | grep -oE 'msg-[0-9a-f]+')
if [ -z "$MSG_A" ]; then
    echo "FAIL: could not capture MSG_A from reply output"
    exit 1
fi
echo "Captured MSG_A=$MSG_A"

# 2d: ack the original question in acme-web's inbox
echo ""
echo "--- Step 2d: acme-web acks the question ---"
cd "$WEB_DIR" && run_abi mail ack "$MSG_Q" --name acme-web

# Step 3: Verify abilities's inbox has the answer with correct chain
echo ""
echo "--- Step 3: verify abilities inbox has answer with reply_to chain ---"
FNO_INBOX="$INBOX_ROOT/abilities.md"
if [ ! -f "$FNO_INBOX" ]; then
    echo "FAIL: abilities inbox file not created at $FNO_INBOX"
    exit 1
fi

LIST_ABILITIES_JSON=$(cd "$FNO_DIR" && run_abi mail list --all --json --from fno)
python3 -c "
import json, sys
msgs = json.load(sys.stdin)
matches = [m for m in msgs if m['msg_id'] == '$MSG_A']
if not matches:
    print('FAIL: MSG_A not found in abilities inbox')
    sys.exit(1)
m = matches[0]
errors = []
if m.get('reply_to') != '$MSG_Q':
    errors.append('reply_to expected $MSG_Q, got ' + str(m.get('reply_to')))
if m.get('from') != 'acme-web':
    errors.append('from expected acme-web, got ' + str(m.get('from')))
if m.get('kind') != 'answer':
    errors.append('kind expected answer, got ' + str(m.get('kind')))
if errors:
    print('FAIL:')
    for e in errors:
        print('  ' + e)
    sys.exit(1)
print('OK: answer in abilities inbox with correct reply_to chain')
" <<< "$LIST_ABILITIES_JSON"

# Step 4: Verify acme-web's inbox has MSG_Q with status:read
echo ""
echo "--- Step 4: verify acme-web question is acked ---"
LIST_WEB_JSON=$(cd "$WEB_DIR" && run_abi mail list --all --json --from acme-web)
python3 -c "
import json, sys
msgs = json.load(sys.stdin)
matches = [m for m in msgs if m['msg_id'] == '$MSG_Q']
if not matches:
    print('FAIL: MSG_Q not found in acme-web inbox')
    sys.exit(1)
m = matches[0]
if m.get('status') != 'read':
    print('FAIL: expected status=read, got ' + str(m.get('status')))
    sys.exit(1)
print('OK: MSG_Q has status=read in acme-web inbox')
" <<< "$LIST_WEB_JSON"

# Step 5: Simulate supervisor (abilities) writing a lesson to memory and sending it
echo ""
echo "--- Step 5: abilities writes lesson memory and sends it to acme-web ---"
MEMORY_FNAME="feedback_silent_failure_hunter_high_threshold.md"
MEMORY_FILE="$MEMORY_DIR/$MEMORY_FNAME"

cat > "$MEMORY_FILE" <<MEMORY
---
auto_generated: true
source_session: abilities
---

# feedback_silent_failure_hunter_high_threshold

When silent-failure-hunter flags HIGH on code in the current diff, fix inline.
Do not defer because it is pre-existing - external reviewers lack that context
and will raise it again (confirmed on PR review iteration 2026-05-04).
MEMORY

if ! grep -q "auto_generated: true" "$MEMORY_FILE"; then
    echo "FAIL: memory file does not have auto_generated: true frontmatter"
    exit 1
fi
echo "OK: memory file written at $MEMORY_FILE"

LESSON_OUT=$(cd "$FNO_DIR" && run_abi mail send \
    --to-project acme-web \
    --kind fyi --persist memory \
    --reply-to "$MSG_A" \
    --body "Saved memory: $MEMORY_FNAME" \
    --from-name abilities)
echo "$LESSON_OUT"

MSG_L=$(echo "$LESSON_OUT" | grep -oE 'msg-[0-9a-f]+')
if [ -z "$MSG_L" ]; then
    echo "FAIL: could not capture MSG_L from lesson send output"
    exit 1
fi
echo "Captured MSG_L=$MSG_L"

# Step 6: Verify acme-web has the lesson with correct reply_to
echo ""
echo "--- Step 6: verify lesson in acme-web inbox with reply_to chain ---"
LIST_WEB2_JSON=$(cd "$WEB_DIR" && run_abi mail list --all --json --from acme-web)
python3 -c "
import json, sys
msgs = json.load(sys.stdin)
matches = [m for m in msgs if m['msg_id'] == '$MSG_L']
if not matches:
    print('FAIL: MSG_L not found in acme-web inbox')
    sys.exit(1)
m = matches[0]
errors = []
if m.get('reply_to') != '$MSG_A':
    errors.append('reply_to expected $MSG_A, got ' + str(m.get('reply_to')))
if m.get('from') != 'abilities':
    errors.append('from expected abilities, got ' + str(m.get('from')))
if m.get('kind') != 'lesson':
    errors.append('kind expected lesson, got ' + str(m.get('kind')))
if errors:
    print('FAIL:')
    for e in errors:
        print('  ' + e)
    sys.exit(1)
print('OK: lesson in acme-web inbox with reply_to=$MSG_A from fno')
" <<< "$LIST_WEB2_JSON"

# Step 7: Verify graph.json has zero new entries throughout the chain
echo ""
echo "--- Step 7: verify graph.json unchanged (zero new nodes) ---"
python3 -c "
import json, sys
graph = json.load(open('$HOME_OVERRIDE/.fno/graph.json'))
count = len(graph.get('entries', []))
if count != $INITIAL_ENTRY_COUNT:
    print('FAIL: graph.json has ' + str(count) + ' entries, expected $INITIAL_ENTRY_COUNT')
    sys.exit(1)
print('OK: graph.json unchanged - ' + str(count) + ' entries (Q/A/lesson chain leaves no backlog nodes)')
"

# Verify three messages exist across two inbox files
echo ""
echo "--- Final: three-message integrity check ---"
python3 -c "
import json, sys

# abilities inbox: should have MSG_A
abilities_msgs = json.load(open('$INBOX_ROOT/abilities.md'.replace('.md', '.md')))
" 2>/dev/null || true

# Count messages in each inbox file via fno list
FNO_COUNT=$(cd "$FNO_DIR" && run_abi mail list --all --json --from fno | python3 -c "import json,sys; print(len(json.load(sys.stdin)))")
WEB_COUNT=$(cd "$WEB_DIR" && run_abi mail list --all --json --from acme-web | python3 -c "import json,sys; print(len(json.load(sys.stdin)))")
TOTAL=$((FNO_COUNT + WEB_COUNT))

echo "abilities inbox: $FNO_COUNT messages"
echo "acme-web inbox: $WEB_COUNT messages"
echo "total: $TOTAL messages (expected 3)"

if [ "$TOTAL" -ne 3 ]; then
    echo "FAIL: expected 3 total messages across both inboxes, got $TOTAL"
    exit 1
fi
echo "OK: exactly 3 messages across two inbox files"

# Verify memory file has auto_generated: true
if ! grep -q "auto_generated: true" "$MEMORY_FILE"; then
    echo "FAIL: memory file missing auto_generated: true"
    exit 1
fi
echo "OK: memory file has auto_generated: true frontmatter"

echo ""
echo "PASS: scenario_b_supervisor_qa_lesson.sh (chain=$MSG_Q->$MSG_A->$MSG_L)"
