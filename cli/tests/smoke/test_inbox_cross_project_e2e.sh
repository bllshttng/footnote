#!/usr/bin/env bash
# Smoke test: cross-project send → drain → graph node, post-2026-05 layout.
# Exercises the real chain with stubbed LLM and real `fno new` against an
# isolated HOME/.fno/graph.json. Asserts provenance fields on the node.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
CLI_DIR="$REPO_ROOT/cli"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

PROJ_A="$WORK/proj-a"
PROJ_B="$WORK/proj-b"
INBOX_ROOT="$WORK/inboxes"
FAKE_HOME="$WORK/home"

mkdir -p "$PROJ_A/.fno" "$PROJ_B/.fno" "$INBOX_ROOT" "$FAKE_HOME/.fno"

cat > "$PROJ_A/.fno/settings.yaml" <<YAML
project: proj-a
project_work:
  domain: code
YAML
cat > "$PROJ_B/.fno/settings.yaml" <<YAML
project: proj-b
project_work:
  domain: code
YAML

# Triage stub: deterministic create_node decision.
STUB="$WORK/triage_stub.sh"
cat > "$STUB" <<'STUB_SCRIPT'
#!/usr/bin/env bash
cat > /dev/null
echo '{"action":"create_node","title":"Cross-project chain test","priority":"p2","body":"Auto-created from smoke.","follow_up_question":null}'
STUB_SCRIPT
chmod +x "$STUB"

# Initialize the isolated graph.json so `fno new` has somewhere to write.
echo '{"_lock_version": 1, "entries": []}' > "$FAKE_HOME/.fno/graph.json"

run_abi() {
  FNO_INBOX_ROOT="$INBOX_ROOT" HOME="$FAKE_HOME" \
    FNO_INBOX_TRIAGE_STUB="$STUB" \
    uv run --project "$CLI_DIR" fno "$@"
}

# 1) proj-a sends a heads-up to proj-b.
SEND_OUT=$(cd "$PROJ_A" && run_abi mail send --to-project proj-b --kind heads-up \
    --body "region data source live in PR 112; please plumb region column" \
    --ref-pr 112 --json)
SENT_MSG=$(echo "$SEND_OUT" | python3 -c "import json,sys; print(json.loads(sys.stdin.read().splitlines()[-1])['msg_id'])")

# 2) proj-b drains. Triage runs, fno new files a graph node in FAKE_HOME.
DRAIN_OUT=$(cd "$PROJ_B" && run_abi mail drain --json --max 1)
NODE_ID=$(echo "$DRAIN_OUT" | python3 -c "
import json, sys
results = json.loads(sys.stdin.read())
assert len(results) == 1, results
r = results[0]
assert r['action'] == 'created_node', r
assert r['kind'] == 'heads-up', r
assert r.get('node_id'), r
print(r['node_id'])
")

# 3) Provenance assertions on the new graph node.
NODE_ID="$NODE_ID" SENT_MSG="$SENT_MSG" FAKE_HOME="$FAKE_HOME" \
python3 <<'PY'
import json, os, sys
graph_path = os.path.join(os.environ["FAKE_HOME"], ".fno", "graph.json")
graph = json.load(open(graph_path))
entries = graph.get("entries", [])
node_id = os.environ["NODE_ID"]
match = [e for e in entries if e.get("id") == node_id]
if not match:
    print(f"FAIL: node {node_id} not found among {len(entries)} entries", file=sys.stderr)
    sys.exit(1)
e = match[0]
assert e.get("source_kind") == "from_inbox", e
assert e.get("source_project") == "proj-a", e
assert e.get("source_inbox_msg") == os.environ["SENT_MSG"], e
print(f"ok: provenance on {e['id']} (source_inbox_msg={e['source_inbox_msg']})")
PY

# 4) Idempotency: a second drain after the first must do nothing for this thread.
DRAIN2=$(cd "$PROJ_B" && run_abi mail drain --json --max 10)
if [[ "$DRAIN2" != "[]" ]]; then
  echo "FAIL: second drain should be empty (heads-up was already acked); got: $DRAIN2" >&2
  exit 1
fi

echo "PASS: inbox cross-project e2e"
