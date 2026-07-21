#!/usr/bin/env bash
# Smoke test: fno mail drain --json on the post-2026-05 thread layout.
# Sends one of each kind, drains, asserts per-kind side effects.
set -euo pipefail

CLI_DIR="$(git rev-parse --show-toplevel)/cli"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

cd "$WORK"

INBOX_ROOT="$WORK/agents"
mkdir -p "$INBOX_ROOT"
PROJECT="test-proj"

# Settings.yaml so resolve_project finds the receiver.
mkdir -p "$WORK/.fno"
cat > "$WORK/.fno/settings.yaml" <<YAML
project: $PROJECT
YAML

# The drain's graph-node creation shells out to the REAL `fno new`: a fake on
# PATH cannot intercept it, because `uv run` prepends the project venv's bin
# ahead of anything we set. Redirect HOME instead so `state_dir()` resolves
# here and not to the developer's ~/.fno.
export HOME="$WORK"
echo '{"_lock_version": 1, "entries": []}' > "$WORK/.fno/graph.json"

# Triage stub returning a deterministic create_node plan.
STUB="$WORK/triage_stub.sh"
cat > "$STUB" <<'STUB_SCRIPT'
#!/usr/bin/env bash
cat > /dev/null
echo '{"action":"create_node","title":"Stubbed node","priority":"p2","body":"Auto-created.","follow_up_question":null}'
STUB_SCRIPT
chmod +x "$STUB"

# Inject one of each kind via the real CLI. Use --project so cwd stays at $WORK
# (so drain's _git_root() / cwd resolves against $WORK/.fno/).
FNO_INBOX_ROOT="$INBOX_ROOT" uv run --project "$CLI_DIR" fno-py mail send \
  --to-project "$PROJECT" --from-name "sender-proj" --kind heads-up \
  --body "please file as a graph node"
FNO_INBOX_ROOT="$INBOX_ROOT" uv run --project "$CLI_DIR" fno-py mail send \
  --to-project "$PROJECT" --from-name "sender-proj" --kind question \
  --body "should we proceed with rollback"
FNO_INBOX_ROOT="$INBOX_ROOT" uv run --project "$CLI_DIR" fno-py mail send \
  --to-project "$PROJECT" --from-name "sender-proj" --kind fyi \
  --body "build complete in 4 minutes"

# Drain (triage stub stands in for the LLM; graph writes land in $WORK/.fno).
DRAIN_JSON=$(FNO_INBOX_ROOT="$INBOX_ROOT" \
  FNO_INBOX_TRIAGE_STUB="$STUB" \
  uv run --project "$CLI_DIR" fno-py mail drain --from "$PROJECT" --json --max 10)

# 3 results, 3 distinct actions.
echo "$DRAIN_JSON" | python3 -c "
import json, sys
results = json.loads(sys.stdin.read())
assert len(results) == 3, results
actions = {r['kind']: r['action'] for r in results}
assert actions.get('heads-up') == 'created_node', actions
assert actions.get('question') == 'wake_signal_dropped', actions
assert actions.get('fyi') == 'dismissed', actions
print(f'ok: heads-up={actions[\"heads-up\"]} question={actions[\"question\"]} fyi={actions[\"fyi\"]}')
"

# The heads-up's node really landed. Asserting only the drain's self-reported
# `created_node` above would pass against a graph that was never written.
WORK="$WORK" python3 -c "
import json, os
entries = json.load(open(os.path.join(os.environ['WORK'], '.fno', 'graph.json')))['entries']
assert len(entries) == 1, entries
node = entries[0]
assert node['title'] == 'Stubbed node', node
assert node['source_kind'] == 'from_inbox', node
print(f'ok: node {node[\"id\"]} created in the scratch graph')
"

# Filesystem-level checks: convo-signals capture removed + wake-signals.
if [[ -f "$WORK/.fno/convo-signals.jsonl" ]]; then
  echo "FAIL: fyi wrote convo-signals.jsonl (capture was removed)" >&2
  exit 1
fi
WAKE_FILES=$(find "$WORK/.fno/wake-signals" -name 'wake-*.json' 2>/dev/null | wc -l | tr -d ' ')
if [[ "$WAKE_FILES" -lt 1 ]]; then
  echo "FAIL: question did not drop a wake-signal" >&2
  exit 1
fi

# heads-up + fyi marked read; question still unread. The drain consumes via the
# md render's read_at (the cursor `mail unread` is a separate consume the drain
# does not advance), so check the render directly via the store.
FNO_INBOX_ROOT="$INBOX_ROOT" \
  uv run --project "$CLI_DIR" python3 -c "
from fno.inbox.store import read_unread_threads
kinds = {h.kind for h in read_unread_threads('$PROJECT')}
assert kinds == {'question'}, kinds
print('ok: only question stays unread')
"

echo "PASS: inbox drain e2e"
