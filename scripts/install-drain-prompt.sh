#!/usr/bin/env bash
set -euo pipefail
# Source paths.sh for typed path variables (STATE_DIR, etc.).
if command -v fno >/dev/null 2>&1; then
    _PATHS_SH="$(fno paths shell-stub 2>/dev/null || true)"
    [[ -f "$_PATHS_SH" ]] && source "$_PATHS_SH" 2>/dev/null || true
    unset _PATHS_SH
fi
TARGET="${STATE_DIR:-$HOME/.fno}/inbox-drain-prompt.md"
SOURCE="$(dirname "$0")/templates/inbox-drain-prompt.md"

mkdir -p "$(dirname "$TARGET")"
if [[ -f "$TARGET" ]]; then
  echo "already installed: $TARGET"
  exit 0
fi
cp "$SOURCE" "$TARGET"
echo "installed: $TARGET"
