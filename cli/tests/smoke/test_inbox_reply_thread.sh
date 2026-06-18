#!/usr/bin/env bash
# Smoke test: --reply-to appends to existing thread file (post-2026-05).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)/cli"
uv sync --quiet

TMP=$(mktemp -d)
INBOX_ROOT="$TMP/inbox"
mkdir -p "$INBOX_ROOT"
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/web/.fno"
cat > "$TMP/web/.fno/settings.yaml" <<YAML
project: web
YAML
mkdir -p "$TMP/api/.fno"
cat > "$TMP/api/.fno/settings.yaml" <<YAML
project: api
YAML

export FNO_INBOX_ROOT="$INBOX_ROOT"
export FNO_INBOX_KNOWN_PROJECTS="web,api"

# 1) web sends a question to api (creates a new thread file).
PARENT_OUT=$(uv run fno mail send --to-project api --kind question \
    --body "Is the payments endpoint ready?" --from-name web --json)
PARENT_PATH=$(echo "$PARENT_OUT" | python3 -c "import json, sys; print(json.loads(sys.stdin.read().splitlines()[-1])['thread_path'])")
PARENT_MSG=$(echo "$PARENT_OUT" | python3 -c "import json, sys; print(json.loads(sys.stdin.read().splitlines()[-1])['msg_id'])")

api_inbox="$INBOX_ROOT/api/inbox"
files_before=$(ls "$api_inbox" | wc -l | tr -d ' ')
if [[ "$files_before" != "1" ]]; then
  echo "FAIL: expected 1 thread file, found $files_before" >&2
  exit 1
fi

# 2) api replies via send --reply-to. Should APPEND to the same thread file.
REPLY_OUT=$(uv run fno mail send --to-project api --kind fyi \
    --reply-to "$PARENT_MSG" --body "Yes, ready and behind a feature flag" \
    --from-name api --json)
APPENDED=$(echo "$REPLY_OUT" | python3 -c "import json, sys; print(json.loads(sys.stdin.read().splitlines()[-1])['appended'])")
if [[ "$APPENDED" != "True" ]]; then
  echo "FAIL: expected appended=true, got $APPENDED" >&2
  echo "$REPLY_OUT" >&2
  exit 1
fi

files_after=$(ls "$api_inbox" | wc -l | tr -d ' ')
if [[ "$files_after" != "1" ]]; then
  echo "FAIL: appended reply created a new file (expected 1, got $files_after)" >&2
  exit 1
fi

# 3) The thread file contains both message blocks.
msg_count=$(grep -c '^## msg-' "$PARENT_PATH" || true)
if [[ "$msg_count" != "2" ]]; then
  echo "FAIL: expected 2 msg blocks in thread file, got $msg_count" >&2
  cat "$PARENT_PATH" >&2
  exit 1
fi

# 4) fno mail reply sends to the ORIGINAL sender (web here), not back to api.
#    Since web's inbox has no thread yet, an orphan thread lands in web/inbox/.
THIRD_OUT=$(uv run fno mail reply --to "$PARENT_MSG" --kind fyi \
    --body "Loud answer to web" --from api --json)
THIRD_ORPHAN=$(echo "$THIRD_OUT" | python3 -c "import json, sys; d=json.loads(sys.stdin.read().splitlines()[-1]); print(d.get('orphan', False))")
if [[ "$THIRD_ORPHAN" != "True" ]]; then
  echo "FAIL: reply expected orphan=true (web has no parent thread yet), got $THIRD_ORPHAN" >&2
  exit 1
fi
web_files=$(ls "$INBOX_ROOT/web/inbox" 2>/dev/null | wc -l | tr -d ' ')
if [[ "$web_files" != "1" ]]; then
  echo "FAIL: expected 1 thread in web/inbox after reply, got $web_files" >&2
  exit 1
fi
if ! grep -q "replies_to: $PARENT_MSG" "$INBOX_ROOT/web/inbox/"*.md; then
  echo "FAIL: replies_to back-reference missing in web's thread" >&2
  exit 1
fi

echo "PASS: inbox reply threading"
