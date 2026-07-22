#!/usr/bin/env bash
# Smoke test: cross-project send works on the post-2026-05 thread layout.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)/cli"
uv sync --quiet

TMP=$(mktemp -d)
INBOX_ROOT="$TMP/inbox"
mkdir -p "$INBOX_ROOT"
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/fno/.fno"
cat > "$TMP/fno/.fno/settings.yaml" <<YAML
project: fno
YAML
mkdir -p "$TMP/example-pipeline/.fno"
cat > "$TMP/example-pipeline/.fno/settings.yaml" <<YAML
project: example-pipeline
YAML

export FNO_INBOX_ROOT="$INBOX_ROOT"
export FNO_INBOX_KNOWN_PROJECTS="fno,example-pipeline"

# 1) example-pipeline sends a fyi to fno (replaces the old `notification` kind).
SEND_OUT=$(uv run fno-py mail send --to-project fno --kind fyi \
    --body "region data source live in PR 112 please be advised" \
    --from-name example-pipeline --json)
THREAD_PATH=$(echo "$SEND_OUT" | python3 -c "import json, sys; print(json.loads(sys.stdin.read().splitlines()[-1])['thread_path'])")
case "$THREAD_PATH" in
  "$INBOX_ROOT/fno/inbox/"*) ;;
  *) echo "FAIL: thread file not under fno/inbox/: $THREAD_PATH" >&2; exit 1 ;;
esac

# 2) fno reads the unread thread, including the body and sender.
JSON_OUT=$(uv run fno-py mail unread --name fno --json)
echo "$JSON_OUT" | python3 -c "
import json, sys
msgs = json.loads(sys.stdin.read())
assert len(msgs) == 1, msgs
m = msgs[0]
assert m['from'] == 'example-pipeline', m
assert m['kind'] == 'fyi', m
assert 'region data' in m['body'], m
print('envelope shape ok')
"

# 3) example-pipeline's own inbox is empty (it sent; it did not receive).
RR_OUT=$(uv run fno-py mail unread --name example-pipeline --json)
if [[ "$RR_OUT" != "[]" ]]; then
  echo "FAIL: example-pipeline should not have received any message; got $RR_OUT" >&2
  exit 1
fi

echo "PASS: inbox cross-project"
