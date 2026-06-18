#!/usr/bin/env bash
# Smoke test: fno mail triage with FNO_INBOX_TRIAGE_STUB on the new layout.
set -euo pipefail
REPO_ROOT="$(git rev-parse --show-toplevel)"
CLI_DIR="$REPO_ROOT/cli"
cd "$CLI_DIR"
uv sync --quiet

TMP=$(mktemp -d)
INBOX_ROOT="$TMP/inbox"
mkdir -p "$INBOX_ROOT"
trap 'rm -rf "$TMP"' EXIT

PROJ_A_DIR="$TMP/proj-a"
mkdir -p "$PROJ_A_DIR/.fno"
cat > "$PROJ_A_DIR/.fno/settings.yaml" <<YAML
project: proj-a
YAML

STUB_SCRIPT="$TMP/triage-stub.sh"
cat > "$STUB_SCRIPT" <<'STUB'
#!/usr/bin/env bash
_prompt=$(cat)
printf '{"action":"create_node","title":"From smoke","priority":"p2","body":"Stub body."}'
STUB
chmod +x "$STUB_SCRIPT"

export FNO_INBOX_ROOT="$INBOX_ROOT"
export FNO_INBOX_KNOWN_PROJECTS="proj-a,somesender"
export FNO_INBOX_TRIAGE_STUB="$STUB_SCRIPT"

# Seed proj-a with a heads-up thread, capture msg-id.
SEND_OUT=$(uv run fno mail send --to-project proj-a --kind heads-up \
    --body "Integration smoke test body for triage" --from-name somesender --json)
MSG_ID=$(echo "$SEND_OUT" | python3 -c "import json,sys; print(json.loads(sys.stdin.read().splitlines()[-1])['msg_id'])")

# Run triage on the thread (look up by any contained msg-id).
TRIAGE_OUT=$(uv run fno mail triage "$MSG_ID" --json --from proj-a)

# Assert plan shape.
echo "$TRIAGE_OUT" | python3 -c "
import json, sys
plan = json.loads(sys.stdin.read())
assert plan['action'] == 'create_node', plan
assert plan['title'] == 'From smoke', plan
assert plan['priority'] == 'p2', plan
print('plan ok')
"

# triage-log.jsonl gets a line keyed on thread_id.
TRIAGE_LOG="$REPO_ROOT/.fno/triage-log.jsonl"
if [[ ! -f "$TRIAGE_LOG" ]]; then
  echo "FAIL: triage-log.jsonl not found at $TRIAGE_LOG" >&2
  exit 1
fi
if ! tail -5 "$TRIAGE_LOG" | grep -q '"thread_id"'; then
  echo "FAIL: triage-log.jsonl missing thread_id field" >&2
  tail -5 "$TRIAGE_LOG" >&2
  exit 1
fi

echo "PASS: inbox triage subprocess"
