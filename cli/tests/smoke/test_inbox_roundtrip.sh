#!/usr/bin/env bash
# Smoke test: send + unread + ack on the post-2026-05 thread-per-file layout.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)/cli"
uv sync --quiet

TMP=$(mktemp -d)
INBOX_ROOT="$TMP/inbox"
mkdir -p "$INBOX_ROOT"
trap 'rm -rf "$TMP"' EXIT

PROJ_A="$TMP/proj-a"
mkdir -p "$PROJ_A/.fno"
cat > "$PROJ_A/.fno/settings.yaml" <<YAML
project: proj-a
YAML

PROJ_B="$TMP/proj-b"
mkdir -p "$PROJ_B/.fno"
cat > "$PROJ_B/.fno/settings.yaml" <<YAML
project: proj-b
YAML

export FNO_INBOX_ROOT="$INBOX_ROOT"
export FNO_INBOX_KNOWN_PROJECTS="proj-a,proj-b"

# 1) Send from proj-a to proj-b's inbox/.
SEND_OUT=$(uv run fno mail send --to-project proj-b --kind heads-up \
    --body "hello from a please respond" --from-name proj-a --json)
THREAD_PATH=$(echo "$SEND_OUT" | python3 -c "import json, sys; print(json.loads(sys.stdin.read().splitlines()[-1])['thread_path'])")
MSG_ID=$(echo "$SEND_OUT" | python3 -c "import json, sys; print(json.loads(sys.stdin.read().splitlines()[-1])['msg_id'])")

if [[ ! -f "$THREAD_PATH" ]]; then
  echo "FAIL: thread file not created at $THREAD_PATH" >&2
  exit 1
fi
case "$THREAD_PATH" in
  "$INBOX_ROOT/proj-b/inbox/"*) ;;
  *) echo "FAIL: thread file landed outside inbox/: $THREAD_PATH" >&2; exit 1 ;;
esac

# 2) proj-b sees unread thread.
UNREAD_OUT=$(uv run fno mail unread --name proj-b --json)
if ! echo "$UNREAD_OUT" | grep -q "hello from a"; then
  echo "FAIL: unread did not show body:" >&2
  echo "$UNREAD_OUT" >&2
  exit 1
fi
if ! echo "$UNREAD_OUT" | grep -q '"from": "proj-a"'; then
  echo "FAIL: unread did not show sender" >&2
  exit 1
fi

# 3) ack by msg-id marks the thread read.
uv run fno mail ack "$MSG_ID" --name proj-b
UNREAD_OUT=$(uv run fno mail unread --name proj-b --json)
if [[ "$UNREAD_OUT" != "[]" ]]; then
  echo "FAIL: unread should be empty after ack:" >&2
  echo "$UNREAD_OUT" >&2
  exit 1
fi

# 4) deprecated kinds reject with replacement hint.
set +e
NOTIF_OUT=$(uv run fno mail send --to-project proj-b --kind notification \
    --body "x" --from-name proj-a 2>&1)
NOTIF_RC=$?
set -e
if [[ $NOTIF_RC -eq 0 ]]; then
  echo "FAIL: --kind notification should exit non-zero" >&2
  exit 1
fi
if ! echo "$NOTIF_OUT" | grep -qi "fyi"; then
  echo "FAIL: notification rejection should hint at fyi:" >&2
  echo "$NOTIF_OUT" >&2
  exit 1
fi

echo "PASS: inbox roundtrip"
