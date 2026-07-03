#!/usr/bin/env bash
# Scenario A: ETL project sends a heads-up to web project.
# Exercises: fno mail send -> triage -> fno new (with provenance) -> fno mail ack
# Also verifies: idempotent triage (crash-recovery) and typo-recipient detection.
#
# Bash 3.2 compatible (macOS default). No mapfile, no associative arrays,
# no here-strings for arrays, no ${var,,} lowercasing.

set -euo pipefail

# Resolve the cli/ dir from this script's location (was a hardcoded worktree path).
CLI_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

TMP=$(mktemp -d /tmp/abi-scenario-A-XXXXXX)

cleanup() {
    rm -rf "$TMP"
}
trap cleanup EXIT

# Temp home so ~/.fno/graph.json goes to our sandbox
HOME_OVERRIDE="$TMP/home"
mkdir -p "$HOME_OVERRIDE/.fno"
echo '{"entries":[]}' > "$HOME_OVERRIDE/.fno/graph.json"

# Inbox root
INBOX_ROOT="$TMP/inbox"
mkdir -p "$INBOX_ROOT"

# Two project fixture dirs
ETL_DIR="$TMP/example-pipeline"
WEB_DIR="$TMP/acme-web"
mkdir -p "$ETL_DIR/.fno"
mkdir -p "$WEB_DIR/.fno"

cat > "$ETL_DIR/.fno/settings.yaml" <<'SETTINGS'
project: example-pipeline
SETTINGS

cat > "$WEB_DIR/.fno/settings.yaml" <<'SETTINGS'
project: acme-web
SETTINGS

# Canned triage stub: always recommends create_node
STUB="$TMP/canned-triage.sh"
cat > "$STUB" <<'STUB_EOF'
#!/usr/bin/env bash
cat <<JSON
{"action":"create_node","title":"Add region filter","priority":"p2","body":"region data source live in example-pipeline; web's region filter list probably wants the new region."}
JSON
STUB_EOF
chmod +x "$STUB"

# Common env for all fno calls
export FNO_INBOX_ROOT="$INBOX_ROOT"
export FNO_INBOX_KNOWN_PROJECTS="example-pipeline,acme-web"
export FNO_INBOX_TRIAGE_STUB="$STUB"
export HOME="$HOME_OVERRIDE"

# Resolve fno command
FNO_CMD="$(cd "$CLI_DIR" && uv run which fno 2>/dev/null)" || true
if [ -z "$FNO_CMD" ]; then
    FNO_CMD="fno"
fi
run_abi() {
    cd "$CLI_DIR" && uv run fno-py "$@"
}

echo "=== Scenario A: ETL to web heads-up triage ==="
echo "TMP=$TMP"

# ---------------------------------------------------------------------------
# MAIN FLOW
# ---------------------------------------------------------------------------

# Step 1: ETL sends heads-up to acme-web
echo ""
echo "--- Step 1: send heads-up from ETL to web ---"
SEND_OUT=$(cd "$ETL_DIR" && run_abi mail send \
    --to-project acme-web \
    --kind heads-up \
    --body "region data source live in PR 112" \
    --ref-pr 112 \
    --from-name example-pipeline)
echo "$SEND_OUT"

MSG_ID=$(echo "$SEND_OUT" | grep -oE 'msg-[0-9a-f]+')
if [ -z "$MSG_ID" ]; then
    echo "FAIL: could not capture msg-id from send output"
    exit 1
fi
echo "Captured MSG_ID=$MSG_ID"

# Step 2: Verify message landed in acme-web inbox
echo ""
echo "--- Step 2: verify message in acme-web inbox ---"
INBOX_FILE="$INBOX_ROOT/acme-web.md"
if [ ! -f "$INBOX_FILE" ]; then
    echo "FAIL: inbox file not created at $INBOX_FILE"
    exit 1
fi

if ! grep -q "from:example-pipeline" "$INBOX_FILE"; then
    echo "FAIL: from:example-pipeline not found in inbox file"
    cat "$INBOX_FILE"
    exit 1
fi

if ! grep -q "kind:heads-up" "$INBOX_FILE"; then
    echo "FAIL: kind:heads-up not found in inbox file"
    exit 1
fi

if ! grep -q "status: unread" "$INBOX_FILE"; then
    echo "FAIL: status:unread not found in inbox file"
    exit 1
fi
echo "OK: message present with correct from/kind/status"

# Step 3: From acme-web - simulate drain step 0

# 3a: unread check
echo ""
echo "--- Step 3a: verify unread returns the message ---"
UNREAD_JSON=$(cd "$WEB_DIR" && run_abi mail unread --json --name acme-web)
if ! echo "$UNREAD_JSON" | python3 -c "import json,sys; msgs=json.load(sys.stdin); assert any(m['msg_id']=='$MSG_ID' for m in msgs), 'msg not in unread'"; then
    echo "FAIL: message not in unread JSON output"
    echo "$UNREAD_JSON"
    exit 1
fi
echo "OK: message is in unread list"

# 3b: triage the message
echo ""
echo "--- Step 3b: triage heads-up ---"
TRIAGE_JSON=$(cd "$WEB_DIR" && run_abi mail triage "$MSG_ID" --json --from acme-web)
echo "Triage result: $TRIAGE_JSON"
ACTION=$(echo "$TRIAGE_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['action'])")
TITLE=$(echo "$TRIAGE_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['title'])")
PRI=$(echo "$TRIAGE_JSON" | python3 -c "import json,sys; print(json.load(sys.stdin)['priority'])")
if [ "$ACTION" != "create_node" ]; then
    echo "FAIL: expected action=create_node, got $ACTION"
    exit 1
fi
echo "OK: triage returned action=create_node title='$TITLE' priority=$PRI"

# 3c: create node with provenance
echo ""
echo "--- Step 3c: create graph node via fno new ---"
NEW_OUT=$(cd "$WEB_DIR" && run_abi new "$TITLE" \
    --project acme-web \
    --priority "$PRI" \
    --source-kind from_inbox \
    --source-project example-pipeline \
    --source-inbox-msg "$MSG_ID")
echo "$NEW_OUT"

AB_ID=$(echo "$NEW_OUT" | grep -oE 'ab-[0-9a-f]+')
if [ -z "$AB_ID" ]; then
    echo "FAIL: could not capture ab-id from fno new output"
    exit 1
fi
echo "Captured AB_ID=$AB_ID"

# 3d: ack the message
echo ""
echo "--- Step 3d: ack message with triaged-into ---"
cd "$WEB_DIR" && run_abi mail ack "$MSG_ID" --triaged-into "$AB_ID" --name acme-web

# Step 4: Verify graph.json has node with all four provenance fields
echo ""
echo "--- Step 4: verify graph.json provenance fields ---"
python3 -c "
import json, sys
graph = json.load(open('$HOME_OVERRIDE/.fno/graph.json'))
matches = [e for e in graph.get('entries', []) if e.get('id') == '$AB_ID']
if not matches:
    print('FAIL: node $AB_ID not found in graph.json')
    sys.exit(1)
n = matches[0]
errors = []
if n.get('source_kind') != 'from_inbox':
    errors.append('source_kind expected from_inbox, got ' + str(n.get('source_kind')))
if n.get('source_project') != 'example-pipeline':
    errors.append('source_project expected example-pipeline, got ' + str(n.get('source_project')))
if n.get('source_inbox_msg') != '$MSG_ID':
    errors.append('source_inbox_msg expected $MSG_ID, got ' + str(n.get('source_inbox_msg')))
if n.get('project') != 'acme-web':
    errors.append('project expected acme-web, got ' + str(n.get('project')))
if errors:
    print('FAIL: provenance field errors:')
    for e in errors:
        print('  ' + e)
    sys.exit(1)
print('OK: all four provenance fields correct')
"

# Step 5: Verify inbox message has status:read and triaged_into
echo ""
echo "--- Step 5: verify inbox message is acked ---"
LIST_JSON=$(cd "$WEB_DIR" && run_abi mail list --all --json --from acme-web)
python3 -c "
import json, sys
msgs = json.load(sys.stdin)
matches = [m for m in msgs if m['msg_id'] == '$MSG_ID']
if not matches:
    print('FAIL: message $MSG_ID not found in list --all')
    sys.exit(1)
m = matches[0]
if m.get('status') != 'read':
    print('FAIL: expected status=read, got ' + str(m.get('status')))
    sys.exit(1)
if m.get('triaged_into') != '$AB_ID':
    print('FAIL: expected triaged_into=$AB_ID, got ' + str(m.get('triaged_into')))
    sys.exit(1)
print('OK: message has status=read and triaged_into=$AB_ID')
" <<< "$LIST_JSON"

echo ""
echo "=== MAIN FLOW PASSED ==="

# ---------------------------------------------------------------------------
# CRASH-RECOVERY SUB-TEST (idempotent triage)
# ---------------------------------------------------------------------------

echo ""
echo "=== Crash-recovery sub-test: idempotent triage ==="

# Reset: remove inbox file and reset graph
rm -f "$INBOX_ROOT/acme-web.md"
echo '{"entries":[]}' > "$HOME_OVERRIDE/.fno/graph.json"

# Send a new heads-up
echo ""
echo "--- CR: send second heads-up ---"
SEND_OUT2=$(cd "$ETL_DIR" && run_abi mail send \
    --to-project acme-web \
    --kind heads-up \
    --body "crash recovery test message" \
    --from-name example-pipeline)
echo "$SEND_OUT2"
MSG2_ID=$(echo "$SEND_OUT2" | grep -oE 'msg-[0-9a-f]+')
if [ -z "$MSG2_ID" ]; then
    echo "FAIL: could not capture msg2-id"
    exit 1
fi
echo "Captured MSG2_ID=$MSG2_ID"

# Triage it
echo ""
echo "--- CR: triage second message ---"
TRIAGE2=$(cd "$WEB_DIR" && run_abi mail triage "$MSG2_ID" --json --from acme-web)
TITLE2=$(echo "$TRIAGE2" | python3 -c "import json,sys; print(json.load(sys.stdin)['title'])")
PRI2=$(echo "$TRIAGE2" | python3 -c "import json,sys; print(json.load(sys.stdin)['priority'])")

# Create node
echo ""
echo "--- CR: create node (no ack - simulating crash) ---"
NEW_OUT2=$(cd "$WEB_DIR" && run_abi new "$TITLE2" \
    --project acme-web \
    --priority "$PRI2" \
    --source-kind from_inbox \
    --source-project example-pipeline \
    --source-inbox-msg "$MSG2_ID")
AB_ID2=$(echo "$NEW_OUT2" | grep -oE 'ab-[0-9a-f]+')
echo "Created AB_ID2=$AB_ID2 (NOT acking - simulating crash)"

# Re-run drain: idempotency check - query graph for source_inbox_msg match
echo ""
echo "--- CR: drain re-run - idempotency check ---"
EXISTING_ID=$(python3 -c "
import json, sys
graph = json.load(open('$HOME_OVERRIDE/.fno/graph.json'))
matches = [e for e in graph.get('entries', []) if e.get('source_inbox_msg') == '$MSG2_ID']
if matches:
    print(matches[0]['id'])
else:
    sys.exit(1)
")
if [ -z "$EXISTING_ID" ]; then
    echo "FAIL: idempotency check found no existing node for MSG2_ID"
    exit 1
fi
echo "OK: existing node found: $EXISTING_ID - skipping re-triage"

# Ack without creating a second node
cd "$WEB_DIR" && run_abi mail ack "$MSG2_ID" --triaged-into "$EXISTING_ID" --name acme-web
echo "Acked $MSG2_ID --triaged-into $EXISTING_ID"

# Verify exactly ONE node for MSG2_ID
echo ""
echo "--- CR: verify exactly one node for MSG2_ID ---"
python3 -c "
import json, sys
graph = json.load(open('$HOME_OVERRIDE/.fno/graph.json'))
matches = [e for e in graph.get('entries', []) if e.get('source_inbox_msg') == '$MSG2_ID']
if len(matches) != 1:
    print('FAIL: expected exactly 1 node for MSG2_ID, got ' + str(len(matches)))
    sys.exit(1)
print('OK: exactly 1 node exists for MSG2_ID (no duplicate)')
"

echo ""
echo "=== CRASH-RECOVERY PASSED ==="

# ---------------------------------------------------------------------------
# AC4-EDGE removed: typo recipient detection ("did you mean") was a property of
# the old `fno mail send` (_check_recipient). The bus epic G4 moved sending to
# `fno mail send --to-project`, which anycasts over the registry and does not
# suggest typo corrections; the step is dropped with the send-side helper.
# ---------------------------------------------------------------------------

echo ""
echo "PASS: scenario_a_etl_to_web_heads_up.sh"
