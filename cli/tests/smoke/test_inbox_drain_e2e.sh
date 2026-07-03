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

# Triage stub returning a deterministic create_node plan.
STUB="$WORK/triage_stub.sh"
cat > "$STUB" <<'STUB_SCRIPT'
#!/usr/bin/env bash
cat > /dev/null
echo '{"action":"create_node","title":"Stubbed node","priority":"p2","body":"Auto-created.","follow_up_question":null}'
STUB_SCRIPT
chmod +x "$STUB"

# Fake `fno new` so drain's graph-node creation succeeds without a real CLI.
FAKE_BIN="$WORK/bin"
mkdir -p "$FAKE_BIN"
cat > "$FAKE_BIN/fno-py" <<'FAKE_ABI'
#!/usr/bin/env bash
case "${1:-}" in
  new)
    if [[ "${2:-}" == "--help" ]]; then
      echo "fake new help with --source-inbox-thread"
      exit 0
    fi
    echo "ab-smoketest1"
    ;;
  *)
    # Fall through to real fno for everything else (send/drain/etc).
    exec "$REAL_ABI" "$@"
    ;;
esac
FAKE_ABI
chmod +x "$FAKE_BIN/fno-py"
# Resolve the real fno only when the fake needs to delegate.
export REAL_ABI=$(command -v fno-py || echo "")

# Inject one of each kind via the real CLI. Use --project so cwd stays at $WORK
# (so drain's _git_root() / cwd lands convo-signals under $WORK/.fno/).
FNO_INBOX_ROOT="$INBOX_ROOT" uv run --project "$CLI_DIR" fno-py mail send \
  --to-project "$PROJECT" --from-name "sender-proj" --kind heads-up \
  --body "please file as a graph node"
FNO_INBOX_ROOT="$INBOX_ROOT" uv run --project "$CLI_DIR" fno-py mail send \
  --to-project "$PROJECT" --from-name "sender-proj" --kind question \
  --body "should we proceed with rollback"
FNO_INBOX_ROOT="$INBOX_ROOT" uv run --project "$CLI_DIR" fno-py mail send \
  --to-project "$PROJECT" --from-name "sender-proj" --kind fyi \
  --body "build complete in 4 minutes"

# Drain (with fake fno on PATH for graph creation, triage stub for LLM).
DRAIN_JSON=$(FNO_INBOX_ROOT="$INBOX_ROOT" \
  FNO_INBOX_TRIAGE_STUB="$STUB" PATH="$FAKE_BIN:$PATH" \
  uv run --project "$CLI_DIR" fno-py mail drain --from "$PROJECT" --json --max 10)

# 3 results, 3 distinct actions.
echo "$DRAIN_JSON" | python3 -c "
import json, sys
results = json.loads(sys.stdin.read())
assert len(results) == 3, results
actions = {r['kind']: r['action'] for r in results}
assert actions.get('heads-up') == 'created_node', actions
assert actions.get('question') == 'wake_signal_dropped', actions
assert actions.get('fyi') == 'logged', actions
print(f'ok: heads-up={actions[\"heads-up\"]} question={actions[\"question\"]} fyi={actions[\"fyi\"]}')
"

# Filesystem-level checks: convo-signals + wake-signals.
if [[ ! -f "$WORK/.fno/convo-signals.jsonl" ]]; then
  echo "FAIL: fyi did not write convo-signals.jsonl" >&2
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
