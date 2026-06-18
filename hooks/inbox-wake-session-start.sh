#!/usr/bin/env bash
# SessionStart hook: surface unread inbox threads as a system reminder.
# Hook contract: stdout is appended to the session prompt; exit 0 = no error.
set -euo pipefail

REPO_ROOT="${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLI_DIR="$(cd "$HOOK_DIR/.." && pwd)/cli"

# Single subprocess: drain question wake-signals + scan unread threads +
# render the entire system-reminder. Replaces the prior six-`uv run` chain
# (Gemini MEDIUM finding on PR #225 about subprocess startup overhead).
cd "$REPO_ROOT" 2>/dev/null || true
uv run --project "$CLI_DIR" python3 -m fno.inbox.unread_scan \
    wake-render "$REPO_ROOT" "question(s) waiting" 2>/dev/null || true
