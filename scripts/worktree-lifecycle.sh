#!/usr/bin/env bash
# Compatibility entrypoint. The implementation has exactly one owner so every
# destructive cleanup path shares the same liveness and app-ownership guards.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec bash "$SCRIPT_DIR/lib/worktree-lifecycle.sh" "$@"
