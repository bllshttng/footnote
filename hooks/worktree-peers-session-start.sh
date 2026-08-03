#!/usr/bin/env bash
# Claude SessionStart carrier for the shared worktree peer advisory.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
note="$(bash "$SCRIPT_DIR/helpers/worktree-live-peers.sh" 2>/dev/null || true)"
[[ -n "$note" ]] || exit 0
printf '## Worktree hygiene\n%s\n' "$note"
