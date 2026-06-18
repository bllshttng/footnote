#!/usr/bin/env bash
# Smoke test: archive_old_threads moves stale read threads under
# {recipient}/inbox/archive/{YYYY-MM}/ on the post-2026-05 layout.
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
config:
  inbox:
    keep_recent_read: 2
YAML

export FNO_INBOX_ROOT="$INBOX_ROOT"
export FNO_INBOX_KNOWN_PROJECTS="proj-a,proj-b"

# Send 5 fyi threads from a -> b.
for i in 1 2 3 4 5; do
  uv run fno mail send --to-project proj-b --kind fyi \
      --body "msg number $i body content" --from-name proj-a >/dev/null
done

# Mark 4 of them read. read_at is set by the drain path in production (the
# cursor `mail ack` advances a per-recipient cursor and does not touch the
# render's read_at); set it directly via the store for this rotation test.
export FNO_INBOX_SETTINGS_CWD="$PROJ_B"
uv run python3 -c "
from fno.inbox.store import read_all_threads, mark_thread_read
for h in read_all_threads('proj-b')[:4]:
    mark_thread_read(h.path)
"

# Run rotation directly via Python (the CLI surface for archive run is a follow-up).
uv run python3 -c "
from fno.inbox.archive import archive_old_threads, read_inbox_settings
result = archive_old_threads('proj-b', read_inbox_settings())
print(f'archived={result.archived_count} kept_unread={result.kept_unread} kept_recent_read={result.kept_recent_read}')
"

# Verify archive directory was created with at least one archived thread.
ARCHIVE_ROOT="$INBOX_ROOT/proj-b/inbox/archive"
if [[ ! -d "$ARCHIVE_ROOT" ]]; then
  echo "FAIL: archive root not created at $ARCHIVE_ROOT" >&2
  exit 1
fi
ARCHIVED=$(find "$ARCHIVE_ROOT" -name '*.md' -type f | wc -l | tr -d ' ')
if [[ "$ARCHIVED" -lt 1 ]]; then
  echo "FAIL: no thread files were archived" >&2
  ls -la "$ARCHIVE_ROOT" >&2
  exit 1
fi

# Live inbox should have unread + keep_recent_read (2) = at most 3 thread files.
LIVE=$(find "$INBOX_ROOT/proj-b/inbox" -maxdepth 1 -name '*.md' -type f | wc -l | tr -d ' ')
if [[ "$LIVE" -gt 3 ]]; then
  echo "FAIL: live inbox should be trimmed (expected <=3 thread files, got $LIVE)" >&2
  exit 1
fi

echo "PASS: inbox archive rotation (archived=$ARCHIVED live=$LIVE)"
