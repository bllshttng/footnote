#!/usr/bin/env bash
# handoffs-migrate-to-vault.sh - one-time operator migration of session
# handoff artifacts from the pre-ab-f063 default (~/.fno/handoffs/<project>/)
# to the vault-derived location (<vault>/internal/<project>/handoffs/) that
# paths.handoffs_dir() now resolves to when Obsidian is configured
# (placement rule, ab-f063 Wave 2).
#
# No-op when Obsidian isn't configured for this project - old and new
# location are the same (state_dir()/handoffs/<project>), nothing to move.
#
# Non-destructive: COPIES files into the new location (skipping any filename
# that already exists there) rather than moving, so an interrupted run never
# loses data. The old directory is left in place - remove it by hand once
# you've confirmed the new location has everything.
#
# Requires the `fno` CLI on PATH. Usage: scripts/handoffs-migrate-to-vault.sh

set -euo pipefail

if ! command -v fno >/dev/null 2>&1; then
  echo "handoffs-migrate-to-vault: 'fno' not found on PATH; nothing to do" >&2
  exit 1
fi

PATHS_SH="$(fno paths shell-stub 2>/dev/null || true)"
if [[ -z "$PATHS_SH" || ! -f "$PATHS_SH" ]]; then
  echo "handoffs-migrate-to-vault: could not resolve live paths via 'fno paths shell-stub'" >&2
  exit 1
fi
# shellcheck source=/dev/null
source "$PATHS_SH"

NEW_DIR="$HANDOFFS_DIR"

PROJECT_NAME="$(fno config get project.id 2>/dev/null || true)"
if [[ -z "$PROJECT_NAME" || "$PROJECT_NAME" == "null" ]]; then
  PROJECT_NAME="$(basename "$REPO_ROOT")"
fi
OLD_DIR="$STATE_DIR/handoffs/$PROJECT_NAME"

if [[ "$OLD_DIR" == "$NEW_DIR" ]]; then
  echo "handoffs-migrate-to-vault: old and new location are the same ($NEW_DIR) - Obsidian not configured, nothing to migrate" >&2
  exit 0
fi

if [[ ! -d "$OLD_DIR" ]]; then
  echo "handoffs-migrate-to-vault: no legacy directory at $OLD_DIR; nothing to do" >&2
  exit 0
fi

mkdir -p "$NEW_DIR"

copied=0
skipped=0
while IFS= read -r -d '' f; do
  base="$(basename "$f")"
  if [[ -e "$NEW_DIR/$base" ]]; then
    skipped=$((skipped + 1))
    continue
  fi
  cp -p "$f" "$NEW_DIR/$base"
  copied=$((copied + 1))
done < <(find "$OLD_DIR" -maxdepth 1 -type f -print0)

echo "handoffs-migrate-to-vault: copied $copied file(s) from $OLD_DIR to $NEW_DIR (skipped $skipped already present)" >&2
echo "handoffs-migrate-to-vault: legacy directory $OLD_DIR left in place - remove by hand once verified" >&2
