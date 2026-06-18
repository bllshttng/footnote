#!/usr/bin/env bash
# uninstall.sh - unload and remove the per-repo post-merge watcher LaunchAgent
# for THIS repo (ab-4e9fb05a). Safe to run when nothing is installed.

set -euo pipefail

# Script-relative fallback (not pwd): uninstall.sh lives in scripts/post-merge/.
REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || (cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd))"
REPO_NAME="$(basename "$REPO_ROOT")"
# Must match install.sh's label scheme (per-checkout hash) so this only ever
# removes THIS checkout's agent, never a same-basename sibling's (Codex P2).
ROOT_HASH="$(printf '%s' "$REPO_ROOT" | shasum 2>/dev/null | cut -d' ' -f1 | cut -c1-8)"
[[ -n "$ROOT_HASH" ]] || ROOT_HASH="$(printf '%s' "$REPO_ROOT" | cksum | tr -cd '0-9' | cut -c1-8)"
LABEL="com.fno.postmerge.${REPO_NAME}-${ROOT_HASH}"
PLIST_PATH="${HOME}/Library/LaunchAgents/${LABEL}.plist"

if [[ ! -f "$PLIST_PATH" ]]; then
  echo "post-merge watcher: no plist at ${PLIST_PATH} (nothing to do)"
  exit 0
fi

# Unload first (best-effort: not-loaded is fine), then remove the file.
launchctl unload "$PLIST_PATH" 2>/dev/null || true
rm -f "$PLIST_PATH"
echo "post-merge watcher: unloaded + removed ${PLIST_PATH}"
