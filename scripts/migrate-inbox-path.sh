#!/usr/bin/env bash
# One-shot migration: legacy flat ``<root>/agents/inbox`` -> threaded
# ``<root>/internal/agents``. Roots are explicit (no hardcoded vault) - point
# them at your inbox layout:
#   FNO_INBOX_OLD_ROOT=<vault>/agents/inbox \
#   FNO_INBOX_NEW_ROOT=<vault>/internal/agents \
#   bash scripts/migrate-inbox-path.sh
set -euo pipefail

OLD_ROOT="${FNO_INBOX_OLD_ROOT:?set FNO_INBOX_OLD_ROOT to the legacy flat inbox dir}"
NEW_ROOT="${FNO_INBOX_NEW_ROOT:?set FNO_INBOX_NEW_ROOT to the threaded inbox dir}"

if [[ ! -d "$OLD_ROOT" ]]; then
  echo "already migrated"
  exit 0
fi

mkdir -p "$NEW_ROOT"

# Migrate per-project inbox files
for f in "$OLD_ROOT"/*.md; do
  [[ -f "$f" ]] || continue
  project=$(basename "$f" .md)
  dest_dir="$NEW_ROOT/$project"
  mkdir -p "$dest_dir"
  if [[ -e "$dest_dir/inbox.md" ]]; then
    echo "warning: $dest_dir/inbox.md already exists; skipping $f" >&2
  else
    mv "$f" "$dest_dir/inbox.md"
    echo "migrated: $f -> $dest_dir/inbox.md"
  fi
done

# Migrate archive
if [[ -d "$OLD_ROOT/archive" ]]; then
  for proj_dir in "$OLD_ROOT/archive"/*/; do
    [[ -d "$proj_dir" ]] || continue
    project=$(basename "$proj_dir")
    dest="$NEW_ROOT/$project/inbox-archive"
    mkdir -p "$dest"
    mv "$proj_dir"/*.md "$dest/" 2>/dev/null || true
    rmdir "$proj_dir" 2>/dev/null || true
  done
  rmdir "$OLD_ROOT/archive" 2>/dev/null || true
fi

# Old root should be empty now
rmdir "$OLD_ROOT" 2>/dev/null || {
  echo "warning: $OLD_ROOT not empty after migration; leaving in place for inspection" >&2
}

echo "migration complete"
